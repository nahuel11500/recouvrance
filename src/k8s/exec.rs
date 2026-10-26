//! Exec, attach, file copy and local commands (shell, kubectl, helm) with a context's kubeconfig.

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use futures::channel::mpsc;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::Api;

use super::{TaskGuard, format_bytes, gate, private_kubeconfig, run, spawn_guarded};

pub enum ExecInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub enum ExecOutput {
    Data(Vec<u8>),
    Exit(String),
}

/// A terminal session: a shell in a container or on this machine, or an attach. Send it
/// keys and sizes, read what it prints; dropping `guard` ends it.
pub struct Session {
    pub guard: TaskGuard,
    pub input: mpsc::UnboundedSender<ExecInput>,
    pub output: mpsc::UnboundedReceiver<ExecOutput>,
}

impl Session {
    /// A session whose task is `work`, given the keys typed and where to print.
    fn spawn<F, Fut>(work: F) -> Self
    where
        F: FnOnce(mpsc::UnboundedReceiver<ExecInput>, mpsc::UnboundedSender<ExecOutput>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (input, keys) = mpsc::unbounded();
        let (out, output) = mpsc::unbounded();
        Self { guard: spawn_guarded(work(keys, out)), input, output }
    }

    /// A session that does nothing, for off-screen rendering checks.
    #[cfg(feature = "dev")]
    pub fn disabled() -> Self {
        Self::spawn(|_, _| async {})
    }
}

/// Copy what a remote process prints to the session, until it ends. Without a tty
/// (`raw_newlines`), bare newlines become CR LF.
async fn forward_output(
    mut from: impl tokio::io::AsyncRead + Unpin,
    out: mpsc::UnboundedSender<ExecOutput>,
    raw_newlines: bool,
) {
    use tokio::io::AsyncReadExt as _;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let bytes = if raw_newlines { crlf(&buf[..n]) } else { buf[..n].to_vec() };
                if out.unbounded_send(ExecOutput::Data(bytes)).is_err() {
                    break;
                }
            }
        }
    }
}

/// Send the keys typed and the terminal's size to a remote process, until the session's
/// input closes or the process stops reading.
async fn forward_input(
    mut keys: mpsc::UnboundedReceiver<ExecInput>,
    mut stdin: Option<impl tokio::io::AsyncWrite + Unpin>,
    mut resize: Option<mpsc::Sender<kube::api::TerminalSize>>,
) {
    use futures::SinkExt as _;
    use tokio::io::AsyncWriteExt as _;
    while let Some(input) = keys.next().await {
        match input {
            ExecInput::Bytes(b) => {
                if let Some(w) = stdin.as_mut() {
                    if w.write_all(&b).await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
                }
            }
            ExecInput::Resize { cols, rows } => {
                if let Some(r) = resize.as_mut() {
                    let _ = r.send(kube::api::TerminalSize { width: cols, height: rows }).await;
                }
            }
        }
    }
}

/// Open an interactive shell (bash if present, else sh) in a container.
pub fn exec_shell(client: Client, ns: String, pod: String, container: Option<String>) -> Session {
    Session::spawn(|keys, out| async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let mut ap = kube::api::AttachParams::interactive_tty();
        if let Some(c) = container {
            ap = ap.container(c);
        }
        let shell = ["sh", "-c", "command -v bash >/dev/null 2>&1 && exec bash || exec sh"];
        let mut proc = match api.exec(&pod, shell, &ap).await {
            Ok(p) => p,
            Err(e) => {
                let _ = out.unbounded_send(ExecOutput::Exit(format!("exec failed: {e}")));
                return;
            }
        };
        let (Some(stdin), Some(stdout), Some(resize)) = (proc.stdin(), proc.stdout(), proc.terminal_size()) else {
            let _ = out.unbounded_send(ExecOutput::Exit("no tty streams".into()));
            return;
        };
        let status = proc.take_status();
        tokio::select! {
            () = forward_output(stdout, out.clone(), false) => {}
            () = forward_input(keys, Some(stdin), Some(resize)) => {}
        }
        let reason = match status {
            Some(s) => match s.await {
                Some(st) if st.status.as_deref() != Some("Success") => {
                    format!("Session ended: {}", st.message.unwrap_or_default())
                }
                _ => "Session ended".to_string(),
            },
            None => "Session ended".to_string(),
        };
        let _ = out.unbounded_send(ExecOutput::Exit(reason));
    })
}

/// The most a container download may write to disk (a bigger file or folder is refused).
pub const MAX_DOWNLOAD_BYTES: u64 = 2 << 30;
/// The most kept in memory from a command whose output is only a short answer.
const MAX_EXEC_OUTPUT: u64 = 16 << 20;
/// The most of stderr kept for an error message.
const MAX_STDERR: usize = 64 * 1024;

/// Read `r` to its end, keeping at most `max` bytes (the rest is read and dropped, so the
/// stream never stalls). Returns the bytes kept and whether some were dropped.
pub(super) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, max: usize) -> (Vec<u8>, bool) {
    use tokio::io::AsyncReadExt as _;
    let (mut kept, mut dropped) = (Vec::new(), false);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
                dropped |= n > room;
            }
        }
    }
    (kept, dropped)
}

/// Hand `r` to `sink` chunk by chunk, failing once more than `max` bytes came, before
/// passing them on. Returns the bytes copied.
pub(super) async fn copy_capped<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    sink: &mut impl FnMut(&[u8]) -> std::io::Result<()>,
    max: u64,
) -> Result<u64> {
    use tokio::io::AsyncReadExt as _;
    let (mut total, mut buf) = (0u64, vec![0u8; 64 * 1024]);
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
        anyhow::ensure!(total <= max, "larger than {}", format_bytes(max as f64));
        sink(&buf[..n])?;
    }
}

/// Run a command with optional stdin and return its stdout (at most 16 MiB).
pub(super) async fn exec_bytes(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    command: Vec<String>,
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut sink = |b: &[u8]| {
        out.extend_from_slice(b);
        Ok(())
    };
    let stdin = stdin.map(std::io::Cursor::new);
    exec_to(client, ns, pod, container, command, stdin, &mut sink, MAX_EXEC_OUTPUT).await?;
    Ok(out)
}

/// Run a command with optional stdin (streamed from a reader), streaming its stdout into
/// `sink` (at most `max` bytes, more is an error). Returns the bytes written.
#[allow(clippy::too_many_arguments)]
pub(super) async fn exec_to(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    command: Vec<String>,
    stdin: Option<impl tokio::io::AsyncBufRead + Unpin>,
    sink: &mut impl FnMut(&[u8]) -> std::io::Result<()>,
    max: u64,
) -> Result<u64> {
    use kube::api::AttachParams;
    use tokio::io::AsyncWriteExt as _;
    let api: Api<Pod> = Api::namespaced(client, &ns);
    let mut ap = AttachParams::default().stdout(true).stderr(true).stdin(stdin.is_some());
    if let Some(c) = container {
        ap = ap.container(c);
    }
    let mut proc = api.exec(&pod, command, &ap).await?;
    let stdout = proc.stdout().context("no stdout")?;
    let stderr = proc.stderr().context("no stderr")?;
    // A write error is kept for later: a command that fails at once closes its stdin
    // ("broken pipe"), and what it said on stderr is the better message.
    let writer = async {
        let mut written = Ok(());
        if let (Some(mut data), Some(mut w)) = (stdin, proc.stdin()) {
            written = async {
                tokio::io::copy_buf(&mut data, &mut w).await?;
                w.shutdown().await
            }
            .await;
        }
        anyhow::Ok(written)
    };
    // try_join: past the cap, stop at once (the command may still be writing).
    let stderr = async { anyhow::Ok(read_capped(stderr, MAX_STDERR).await.0) };
    let (written, copied, err) = tokio::try_join!(writer, copy_capped(stdout, sink, max), stderr)?;
    let status = match proc.take_status() {
        Some(s) => s.await,
        None => None,
    };
    if status.as_ref().is_some_and(|s| s.status.as_deref() != Some("Success")) {
        let msg = String::from_utf8_lossy(&err).trim().to_string();
        anyhow::bail!(
            "{}",
            if msg.is_empty() { status.and_then(|s| s.message).unwrap_or_else(|| "failed".into()) } else { msg }
        );
    }
    written?;
    Ok(copied)
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Copy a file or directory out of a container into ~/Downloads (a directory
/// comes as a .tar), like `kubectl cp pod:path .`. Returns where it was saved.
pub async fn download_from_container(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    path: String,
) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    run(async move {
        let (parent, member, base) = tar_source(&path);
        // Succeeds for a file too, so a failure (refused, no shell) is said, not read as a file.
        let is_dir = exec_bytes(
            client.clone(),
            ns.clone(),
            pod.clone(),
            container.clone(),
            vec!["sh".into(), "-c".into(), format!("if test -d {}; then echo dir; fi", shell_quote(&path))],
            None,
        )
        .await
        .map(|o| String::from_utf8_lossy(&o).contains("dir"))?;
        let (cmd, name) = if is_dir {
            (
                vec!["tar".into(), "cf".into(), "-".into(), "-C".into(), parent, "--".into(), member],
                format!("{pod}-{base}.tar"),
            )
        } else {
            (vec!["cat".into(), "--".into(), path.clone()], format!("{pod}-{base}"))
        };
        // Streamed to a new private file (it may hold secrets), never over an earlier one.
        let (file, dest) = crate::private_files::create_new_in(&crate::paths::downloads_dir(), &name)?;
        // Through a buffer, and off the async workers while the disk is written.
        let mut file = std::io::BufWriter::with_capacity(1 << 20, file);
        let mut sink = |b: &[u8]| tokio::task::block_in_place(|| file.write_all(b));
        let copied = exec_to(client, ns, pod, container, cmd, None::<&[u8]>, &mut sink, MAX_DOWNLOAD_BYTES).await;
        let copied = copied.and_then(|_| Ok(tokio::task::block_in_place(|| file.flush())?));
        drop(file);
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&dest);
            return Err(e);
        }
        Ok(dest)
    })
    .await
}

/// For `tar -C parent -- member`: the folder to run in, the entry to archive and the name to save
/// it under. `/` is the whole filesystem, saved as `root`.
fn tar_source(path: &str) -> (String, String, String) {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return ("/".into(), ".".into(), "root".into());
    }
    let (parent, base) = match trimmed.rsplit_once('/') {
        Some((p, b)) => (if p.is_empty() { "/" } else { p }, b),
        None => (".", trimmed),
    };
    (parent.into(), base.into(), base.into())
}

/// Copy a local file into a container directory, like `kubectl cp file pod:dir/`.
pub async fn upload_to_container(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    local: std::path::PathBuf,
    dir: String,
) -> Result<String> {
    run(async move {
        let name = local.file_name().and_then(|n| n.to_str()).context("bad file name")?.to_string();
        let dest = format!("{}/{name}", dir.trim_end_matches('/'));
        let file = upload_source(&local).await?;
        let cmd = vec!["sh".into(), "-c".into(), format!("cat > {}", shell_quote(&dest))];
        let mut sink = |_: &[u8]| Ok(());
        exec_to(client, ns, pod, container, cmd, Some(file), &mut sink, MAX_EXEC_OUTPUT).await?;
        Ok(dest)
    })
    .await
}

/// A local file to upload, read a MiB at a time as it is sent: a big one is never held in
/// memory, and the shared runtime's workers don't wait on the disk.
async fn upload_source(local: &std::path::Path) -> Result<tokio::io::BufReader<tokio::fs::File>> {
    let file = tokio::fs::File::open(local).await?;
    // Caught before the command runs, which would leave an empty file behind.
    anyhow::ensure!(file.metadata().await?.is_file(), "{} is not a file", local.display());
    Ok(tokio::io::BufReader::with_capacity(1 << 20, file))
}

/// Run a command (no tty) and capture its output, like `kubectl exec -- cmd`.
pub async fn exec_capture(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    command: Vec<String>,
    timeout_secs: u64,
    max_bytes: usize,
) -> Result<String> {
    use kube::api::AttachParams;
    run(async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let mut ap = AttachParams::default().stdout(true).stderr(true).stdin(false);
        if let Some(c) = container {
            ap = ap.container(c);
        }
        let mut proc = api.exec(&pod, command, &ap).await?;
        let stdout = proc.stdout().context("no stdout")?;
        let stderr = proc.stderr().context("no stderr")?;
        let status = proc.take_status();
        let work = async {
            // Capped while reading: a chatty command cannot fill memory before the timeout.
            let (out, err) = tokio::join!(read_capped(stdout, max_bytes), read_capped(stderr, max_bytes));
            let status = match status {
                Some(s) => s.await,
                None => None,
            };
            (out, err, status)
        };
        let (out, err, status) = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), work)
            .await
            .map_err(|_| anyhow!("command timed out after {timeout_secs}s"))?;
        let clip = |(b, dropped): (Vec<u8>, bool)| {
            let s = String::from_utf8_lossy_owned(b);
            if dropped { format!("{s}\n… [truncated]") } else { s }
        };
        let code = match &status {
            Some(s) if s.status.as_deref() == Some("Success") => "exit 0".to_string(),
            Some(s) => s.message.clone().unwrap_or_else(|| "failed".into()),
            None => "unknown status".into(),
        };
        Ok(format!("[{code}]\n--- stdout ---\n{}\n--- stderr ---\n{}", clip(out), clip(err)))
    })
    .await
}

/// Kills the shell when the session is dropped.
pub(super) struct KillOnDrop(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// Run a plugin command in the background with KUBECONFIG set to `context`, and `env` set.
pub async fn run_command(
    context: String,
    namespace: Option<String>,
    command: String,
    env: Vec<(String, String)>,
) -> Result<String> {
    run(async move {
        let kubeconfig = private_kubeconfig(&context, namespace.as_deref(), "plugin")?;
        let out = tokio::process::Command::new("sh")
            .args(["-c", &command])
            .envs(env)
            .env("KUBECONFIG", kubeconfig.path())
            .output()
            .await;
        drop(kubeconfig);
        let out = out?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
            .trim()
            .to_string();
        if !out.status.success() {
            anyhow::bail!("{}", text.lines().last().unwrap_or("failed"));
        }
        Ok(text)
    })
    .await
}

/// Whether a helm command line changes the cluster: install, upgrade, rollback or
/// uninstall, unless it is a dry run. Only `--dry-run`, `=client` and `=server` are dry runs
/// (`--dry-run=false` or `=none` is a real one), and the last one given is what helm uses.
pub fn helm_writes(args: &[String]) -> bool {
    let first = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let dry_run = args.iter().rev().find(|a| *a == "--dry-run" || a.starts_with("--dry-run="));
    matches!(first, Some("install" | "upgrade" | "rollback" | "uninstall" | "delete"))
        && !matches!(dry_run.map(String::as_str), Some("--dry-run" | "--dry-run=client" | "--dry-run=server"))
}

/// Run the `helm` CLI against a context and return its output. A command that changes the
/// cluster needs write mode when it starts, like the UI's other writes.
pub async fn helm(context: String, args: Vec<String>) -> Result<String> {
    if helm_writes(&args) && !gate::ui_writes() {
        anyhow::bail!(gate::WRITES_OFF);
    }
    run(async move {
        let kubeconfig = private_kubeconfig(&context, None, "helm")?;
        let out = tokio::process::Command::new("helm").args(&args).env("KUBECONFIG", kubeconfig.path()).output().await;
        drop(kubeconfig);
        let out = out.context("running helm (is it installed?)")?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
            .trim()
            .to_string();
        if !out.status.success() {
            anyhow::bail!("{text}");
        }
        Ok(text)
    })
    .await
}

/// A shell on this machine, in a pty, whose kubectl points at `context`
/// (and `namespace`), via a private copy of the kubeconfig. `env` is added to its environment.
pub fn local_shell(
    context: String,
    namespace: Option<String>,
    command: Option<String>,
    env: Vec<(String, String)>,
) -> Session {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read as _, Write as _};

    Session::spawn(|mut in_rx, out_tx| async move {
        let fail = |e: String| {
            let _ = out_tx.unbounded_send(ExecOutput::Exit(e));
        };
        // Private kubeconfig with this context selected; the user's file is untouched.
        // Removed when this task ends or is aborted (tab closed), after the shell is killed.
        let kubeconfig = match private_kubeconfig(&context, namespace.as_deref(), "shell") {
            Ok(p) => p,
            Err(e) => return fail(format!("kubeconfig: {e:#}")),
        };
        let pty = native_pty_system();
        let pair = match pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }) {
            Ok(p) => p,
            Err(e) => return fail(format!("pty: {e}")),
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        // A plugin runs its command, then waits so its output stays readable.
        let mut cmd = match &command {
            Some(c) => {
                let mut b = CommandBuilder::new("sh");
                b.args([
                    "-c",
                    &format!("{c}\nstatus=$?\nprintf '\\n[exited with %s — press Enter to close]' \"$status\"\nread _"),
                ]);
                b
            }
            None => CommandBuilder::new(shell),
        };
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.env("KUBECONFIG", kubeconfig.path());
        cmd.env("RECOUVRANCE_CONTEXT", &context);
        cmd.env("TERM", "xterm-256color");
        if let Some(home) = std::env::var_os("HOME") {
            cmd.cwd(home);
        }
        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => KillOnDrop(c),
            Err(e) => return fail(format!("shell: {e}")),
        };
        drop(pair.slave);
        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => return fail(format!("pty: {e}")),
        };
        let mut writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => return fail(format!("pty: {e}")),
        };
        let out = out_tx.clone();
        let reading = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.unbounded_send(ExecOutput::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        // Writes block while the shell is not reading (a big paste): they get their own thread,
        // never a worker of the shared kube runtime. It ends when `keys` is dropped below.
        let (keys, typed) = std::sync::mpsc::channel::<Vec<u8>>();
        let spawned = std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
            for b in typed {
                if writer.write_all(&b).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });
        if let Err(e) = spawned {
            return fail(format!("pty: {e}"));
        }
        let master = std::sync::Mutex::new(pair.master);
        let writing = async {
            while let Some(input) = in_rx.next().await {
                match input {
                    ExecInput::Bytes(b) => {
                        if keys.send(b).is_err() {
                            break;
                        }
                    }
                    ExecInput::Resize { cols, rows } => {
                        let _ = master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    }
                }
            }
        };
        tokio::select! {
            _ = reading => {}
            () = writing => {}
        }
        drop(child);
        drop(keys);
        drop(kubeconfig);
        let _ = out_tx.unbounded_send(ExecOutput::Exit("Shell exited".into()));
    })
}

/// `kubectl attach`: the container's own process output (and stdin/tty when the
/// container was started with them), instead of a new shell.
pub fn attach_container(client: Client, ns: String, pod: String, container: Option<String>) -> Session {
    Session::spawn(|keys, out| async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        // Attach with what the container offers: stdin and a tty only if its spec has them.
        let spec = api.get(&pod).await.ok().and_then(|p| p.spec);
        let (name, stdin, tty) = attach_target(spec.as_ref(), container.as_deref());
        let mut ap = kube::api::AttachParams::default().stdin(stdin).stdout(true).stderr(!tty).tty(tty);
        if let Some(name) = name {
            ap = ap.container(name);
        }
        let mut proc = match api.attach(&pod, &ap).await {
            Ok(p) => p,
            Err(e) => {
                let _ = out.unbounded_send(ExecOutput::Exit(format!("attach failed: {e}")));
                return;
            }
        };
        let note = format!(
            "\x1b[2mAttached to the container's main process{}. Closing detaches; it keeps running.\x1b[0m\r\n",
            if stdin { "" } else { " (output only: it was not started with stdin)" }
        );
        let _ = out.unbounded_send(ExecOutput::Data(note.into_bytes()));
        // Without a tty, output uses bare newlines.
        let stdout = proc.stdout().map(|s| forward_output(s, out.clone(), !tty));
        let stderr = proc.stderr().map(|s| forward_output(s, out.clone(), !tty));
        let readers = async move {
            match (stdout, stderr) {
                (Some(a), Some(b)) => {
                    futures::join!(a, b);
                }
                (Some(a), None) => a.await,
                (None, Some(b)) => b.await,
                (None, None) => {}
            }
        };
        // Detaching is the process ending or closing its output, not the input closing.
        let writer = async {
            forward_input(keys, proc.stdin(), proc.terminal_size()).await;
            futures::future::pending::<()>().await;
        };
        tokio::select! {
            () = readers => {}
            () = writer => {}
        }
        let _ = out.unbounded_send(ExecOutput::Exit("Detached: the process ended or closed its output".into()));
    })
}

/// Bare newlines as CR LF, on the bytes: a character split across two reads stays whole.
fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// The container to attach to and whether it has stdin and a tty. The requested name is always
/// kept, so the server attaches to it (an init or ephemeral container too) or says it is not there.
fn attach_target(
    spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    container: Option<&str>,
) -> (Option<String>, bool, bool) {
    let Some(name) = container else {
        let first = spec.and_then(|s| s.containers.first());
        return first.map_or((None, false, false), |c| {
            (Some(c.name.clone()), c.stdin.unwrap_or(false), c.tty.unwrap_or(false))
        });
    };
    let streams = spec.and_then(|s| {
        (s.containers.iter().chain(s.init_containers.iter().flatten()))
            .find(|c| c.name == name)
            .map(|c| (c.stdin, c.tty))
            .or_else(|| s.ephemeral_containers.iter().flatten().find(|c| c.name == name).map(|c| (c.stdin, c.tty)))
    });
    let (stdin, tty) = streams.unwrap_or_default();
    (Some(name.to_string()), stdin.unwrap_or(false), tty.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::{attach_target, copy_capped, crlf, helm_writes, read_capped, tar_source, upload_source};
    use k8s_openapi::api::core::v1::{Container, EphemeralContainer, PodSpec};

    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_stream_a_file_and_refuse_a_folder() {
        use tokio::io::AsyncReadExt as _;
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.bin");
        let data: Vec<u8> = (0..3 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let mut read = Vec::new();
        upload_source(&file).await.unwrap().read_to_end(&mut read).await.unwrap();
        assert!(read == data);
        assert!(upload_source(&dir).await.is_err());
        assert!(upload_source(&dir.join("missing")).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_real_dry_run_gets_past_the_write_gate() {
        let writes = |line: &str| helm_writes(&line.split(' ').map(String::from).collect::<Vec<_>>());
        assert!(writes("upgrade web ./chart"));
        assert!(!writes("upgrade web ./chart --dry-run"));
        assert!(!writes("upgrade web ./chart --dry-run=server"));
        assert!(!writes("install web ./chart --dry-run=client"));
        assert!(writes("upgrade web ./chart --dry-run=false"));
        assert!(writes("upgrade web ./chart --dry-run=none"));
        assert!(writes("upgrade web ./chart --dry-run --dry-run=false"));
        assert!(!writes("history web"));
    }

    #[test]
    fn attach_keeps_the_requested_container() {
        let spec = PodSpec {
            containers: vec![Container { name: "app".into(), ..Default::default() }],
            ephemeral_containers: Some(vec![EphemeralContainer {
                name: "debugger-x".into(),
                stdin: Some(true),
                tty: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(attach_target(Some(&spec), Some("debugger-x")), (Some("debugger-x".into()), true, true));
        assert_eq!(attach_target(Some(&spec), None), (Some("app".into()), false, false));
        // Unknown or unreadable spec: still ask for that container, output only.
        assert_eq!(attach_target(Some(&spec), Some("sidecar")), (Some("sidecar".into()), false, false));
        assert_eq!(attach_target(None, Some("app")), (Some("app".into()), false, false));
    }

    #[test]
    fn tar_sources() {
        assert_eq!(tar_source("/"), ("/".into(), ".".into(), "root".into()));
        assert_eq!(tar_source("/var/log/"), ("/var".into(), "log".into(), "log".into()));
        assert_eq!(tar_source("/etc"), ("/".into(), "etc".into(), "etc".into()));
        assert_eq!(tar_source("data"), (".".into(), "data".into(), "data".into()));
    }

    #[test]
    fn newlines_keep_split_characters() {
        let text = "é\nok\n".as_bytes();
        // "é" is two bytes; each half goes through on its own.
        let joined = [crlf(&text[..1]), crlf(&text[1..])].concat();
        assert_eq!(String::from_utf8(joined).unwrap(), "é\r\nok\r\n");
    }

    #[tokio::test]
    async fn reads_are_capped_but_drained() {
        let data = vec![7u8; 100_000];
        let (kept, dropped) = read_capped(&data[..], 1000).await;
        assert_eq!((kept.len(), dropped), (1000, true));
        let (kept, dropped) = read_capped(&data[..10], 1000).await;
        assert_eq!((kept.len(), dropped), (10, false));
    }

    #[tokio::test]
    async fn copies_fail_past_the_cap() {
        let data = vec![1u8; 4096];
        let mut out = Vec::new();
        let mut sink = |b: &[u8]| {
            out.extend_from_slice(b);
            Ok(())
        };
        assert_eq!(copy_capped(&data[..], &mut sink, 4096).await.unwrap(), 4096);
        assert_eq!(out.len(), 4096);
        let mut out = Vec::new();
        let mut sink = |b: &[u8]| {
            out.extend_from_slice(b);
            Ok(())
        };
        assert!(copy_capped(&data[..], &mut sink, 4095).await.is_err());
        // Nothing past the cap is passed on.
        assert!(out.len() <= 4095);
    }
}
