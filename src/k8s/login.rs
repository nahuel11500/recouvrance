//! A context's login command (its kubeconfig exec plugin) that failed, said without what it
//! was given or printed. kube's error shows the whole command line, arguments and environment
//! included (where a kubeconfig keeps an OIDC client secret), then the raw output (where the
//! token is). Errors reach pages, agents and the audit log as text, by many paths: so each
//! client puts [`LoginFailed`] in that error's place where it comes out, when it is built (the
//! command runs a first time) and on every request (it runs again when the token expires).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use kube::client::AuthError;

/// The most characters of the command's error output shown.
const MAX_DETAIL: usize = 200;
/// The longest program name shown.
const MAX_NAME: usize = 64;

/// A failed login command, as [`login_failure`] says it.
#[derive(Debug)]
pub struct LoginFailed(String);

impl std::fmt::Display for LoginFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoginFailed {}

/// What to say when `e`, or an error behind it, is a context's login command that failed: its
/// program's file name, its exit code and the last line it wrote to stderr; never its
/// arguments, environment or standard output. None for any other error.
pub fn login_failure(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    std::iter::successors(Some(e), |e| e.source()).find_map(|e| {
        if let Some(failed) = e.downcast_ref::<LoginFailed>() {
            return Some(failed.0.clone());
        }
        match e.downcast_ref::<AuthError>()? {
            AuthError::AuthExecRun { cmd, out } => Some(run_failed(cmd, out)),
            // An `io::Error` from starting it ("No such file or directory"), which names nothing.
            AuthError::AuthExecStart(e) => Some(format!("the login command could not be started: {e}")),
            // The parse error quotes the output around where it failed.
            AuthError::AuthExecParse(_) => Some("the login command's output could not be read as a credential".into()),
            _ => None,
        }
    })
}

/// The error of building a client, with [`LoginFailed`] in its place when it is a failed login
/// command: building runs it a first time.
pub fn build_error(e: kube::Error) -> anyhow::Error {
    match login_failure(&e) {
        Some(said) => LoginFailed(said).into(),
        None => e.into(),
    }
}

/// Puts [`LoginFailed`] in place of a failed login command's error on every request of a
/// client: kube runs the command again on whatever request comes after its token expired.
#[derive(Clone, Copy)]
pub struct LoginErrorLayer;

impl<S> tower::Layer<S> for LoginErrorLayer {
    type Service = LoginErrors<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoginErrors { inner }
    }
}

#[derive(Clone)]
pub struct LoginErrors<S> {
    inner: S,
}

impl<S, R> tower::Service<R> for LoginErrors<S>
where
    S: tower::Service<R>,
    S::Future: Send + 'static,
    S::Error: Into<tower::BoxError>,
{
    type Response = S::Response;
    type Error = tower::BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, tower::BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|e| scrub(e.into()))
    }

    fn call(&mut self, req: R) -> Self::Future {
        let fut = self.inner.call(req);
        Box::pin(async move { fut.await.map_err(|e| scrub(e.into())) })
    }
}

/// `e`, or [`LoginFailed`] in its place when it is a failed login command.
fn scrub(e: tower::BoxError) -> tower::BoxError {
    match login_failure(&*e) {
        Some(said) => Box::new(LoginFailed(said)),
        None => e,
    }
}

/// kube's `AuthExecRun`, said by the program's file name, its exit code and its last stderr line.
fn run_failed(cmd: &str, out: &std::process::Output) -> String {
    let program = program_name(cmd).map_or_default(|p| format!(" `{p}`"));
    let how = ended(out.status);
    match last_line(&out.stderr) {
        Some(line) => format!("the login command{program} failed ({how}): {line}"),
        None => format!("the login command{program} failed ({how})"),
    }
}

/// How the command ended: "exit 1", or "signal 9" when it was killed.
fn ended(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// The last line of `stderr` with text in it, as plain text, at most `MAX_DETAIL` characters.
fn last_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().map(plain).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(MAX_DETAIL) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line,
    })
}

/// A line of terminal output as text: color and cursor sequences (`ESC [`, parameters, a final
/// character) and other control characters left out, spaces trimmed.
fn plain(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            text.push(c);
        }
    }
    text.trim().to_string()
}

/// The file name of the program a failed login command ran, from the command line kube gives as
/// text: `Command`'s debug form for an exec plugin, `cmd-path` then `cmd-args` for a gcp auth
/// provider. None when it can't be told apart from the rest or is not a plain file name: no
/// name is better than a piece of an argument.
fn program_name(cmd: &str) -> Option<String> {
    // The debug form quotes every word; a gcp provider's line quotes none.
    let program = if cmd.contains('"') { debug_program(cmd)? } else { cmd.split_whitespace().next()?.to_string() };
    let name = program.rsplit('/').next()?;
    let ok = name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    (ok && !name.is_empty() && name.len() <= MAX_NAME).then(|| name.to_string())
}

/// The program in `Command`'s debug form, which is
/// `[cd "dir" && ][env -i |env -u KEY …][KEY="value" …][["program"] ]"argv0" "arg" …`.
fn debug_program(cmd: &str) -> Option<String> {
    let mut rest = cmd;
    loop {
        rest = rest.trim_start_matches(' ');
        if let Some(r) = rest.strip_prefix("cd ") {
            rest = quoted(r)?.1.strip_prefix(" && ")?;
        } else if let Some(r) = rest.strip_prefix("env -i ") {
            rest = r;
        } else if let Some(mut r) = rest.strip_prefix("env ") {
            // The variables taken away, `-u KEY` each.
            while let Some(after) = r.strip_prefix("-u ") {
                r = after.split_once(' ')?.1;
            }
            rest = r;
        } else if let Some(r) = rest.strip_prefix('[') {
            // The program, shown apart when argv[0] was set to something else.
            return Some(quoted(r)?.0);
        } else if rest.starts_with('"') {
            return Some(quoted(rest)?.0);
        } else {
            // A variable set for it: `KEY="value"`.
            rest = quoted(rest.split_once('=')?.1)?.1;
        }
    }
}

/// The quoted string `s` starts with, unescaped (`\"`, `\\` and `\'`; other escapes are kept as
/// they are), and the text after it.
fn quoted(s: &str) -> Option<(String, &str)> {
    let inner = s.strip_prefix('"')?;
    let mut text = String::new();
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((text, &inner[i + 1..])),
            '\\' => match chars.next()?.1 {
                c @ ('"' | '\\' | '\'') => text.push(c),
                c => {
                    text.push('\\');
                    text.push(c);
                }
            },
            c => text.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Command, ExitStatus, Output};

    use super::*;

    /// A failed exec plugin as kube reports it: `Command`'s debug form, then what it printed.
    fn failed_run(cmd: &Command, code: i32, stdout: &str, stderr: &str) -> AuthError {
        let out = Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        AuthError::AuthExecRun { cmd: format!("{cmd:?}"), out: Box::new(out) }
    }

    /// kubelogin as kube runs it, with an OIDC client secret in its arguments and another secret
    /// in its environment.
    fn kubelogin() -> Command {
        let mut cmd = Command::new("/usr/local/bin/kubelogin");
        cmd.args(["get-token", "--oidc-issuer-url=https://issuer.example", "--oidc-client-secret=s3cr3t-arg"]);
        cmd.env("KUBERNETES_EXEC_INFO", r#"{"kind":"ExecCredential","spec":{"interactive":false}}"#);
        cmd.env("LOGIN_ENV", "s3cr3t \"env\" = x");
        cmd.env_remove("S3CR3T_DROPPED");
        cmd
    }

    fn expired() -> AuthError {
        let stderr = "warning: no cached token\n\x1b[31merror:\x1b[0m oidc: refresh token expired  \n\n";
        failed_run(&kubelogin(), 1, r#"{"status":{"token":"s3cr3t-out"}}"#, stderr)
    }

    const SAID: &str = "the login command `kubelogin` failed (exit 1): error: oidc: refresh token expired";

    #[test]
    fn a_failed_login_command_says_its_name_exit_code_and_error_only() {
        let e = expired();
        // kube's own text has the arguments, the environment and the output.
        assert!(e.to_string().contains("s3cr3t-arg") && e.to_string().contains("s3cr3t-out"), "{e}");
        assert_eq!(login_failure(&e).as_deref(), Some(SAID));
        // Nothing on stderr, killed: said by how it ended.
        let mut quiet = failed_run(&kubelogin(), 2, "s3cr3t-out", " \n");
        assert_eq!(login_failure(&quiet).as_deref(), Some("the login command `kubelogin` failed (exit 2)"));
        if let AuthError::AuthExecRun { out, .. } = &mut quiet {
            out.status = ExitStatus::from_raw(9);
        }
        assert_eq!(login_failure(&quiet).as_deref(), Some("the login command `kubelogin` failed (signal 9)"));
        // A long last line is cut.
        let long = failed_run(&kubelogin(), 1, "", &"x".repeat(500));
        let said = login_failure(&long).unwrap();
        assert!(said.ends_with(&format!(": {}…", "x".repeat(MAX_DETAIL))), "{said}");
    }

    /// Building the client (`kube::Error::Auth`) gives the few words alone, whichever way it is
    /// printed. kube's error as it comes (in a `Service` error, under the app's context) is
    /// said the same by the pages' and the agents' formatters.
    #[test]
    fn every_path_says_it_without_the_secrets() {
        let built = build_error(kube::Error::Auth(expired()));
        assert!(built.downcast_ref::<LoginFailed>().is_some());
        assert_eq!(format!("{built:#}"), SAID);
        assert!(!format!("{built:?}").contains("s3cr3t"), "{built:?}");
        let sent = anyhow::Error::from(kube::Error::Service(Box::new(expired()))).context("listing pods");
        for e in [&built, &sent] {
            assert_eq!(crate::k8s::short_error(e), SAID);
            assert_eq!(crate::k8s::error_chain(e), SAID);
        }
        // Other errors keep what was being done.
        let other = anyhow::anyhow!("connection refused").context("discovery of apps/v1 failed");
        assert_eq!(crate::k8s::error_chain(&other), "discovery of apps/v1 failed: connection refused");
        assert!(build_error(kube::Error::Service("refused".into())).downcast_ref::<LoginFailed>().is_none());
    }

    /// A request whose token refresh runs the login command, which fails.
    struct Refresh;

    impl tower::Service<()> for Refresh {
        type Response = ();
        type Error = tower::BoxError;
        type Future = std::future::Ready<Result<(), tower::BoxError>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, (): ()) -> Self::Future {
            let e: tower::BoxError = Box::new(expired());
            std::future::ready(Err(e))
        }
    }

    #[test]
    fn the_layer_replaces_the_error_of_every_request() {
        use tower::{Layer as _, Service as _};
        let e = futures::executor::block_on(LoginErrorLayer.layer(Refresh).call(())).unwrap_err();
        assert_eq!(e.to_string(), SAID);
        assert!(e.downcast_ref::<LoginFailed>().is_some());
        // As `kube::Client` hands it on.
        let e = kube::Error::Service(e);
        assert_eq!(e.to_string(), format!("ServiceError: {SAID}"));
        assert!(!format!("{e:?}").contains("s3cr3t"), "{e:?}");
        assert_eq!(crate::k8s::short_error(&anyhow::Error::from(e)), SAID);
    }

    #[test]
    fn a_login_command_that_does_not_start_or_prints_no_credential() {
        let e = AuthError::AuthExecStart(std::io::Error::from_raw_os_error(2));
        let said = login_failure(&e).unwrap();
        assert!(said.starts_with("the login command could not be started: ") && said.contains("os error 2"), "{said}");
        let e = kube::Error::Auth(AuthError::AuthExecParse(Box::new(
            serde_saphyr::from_str::<std::collections::BTreeMap<String, String>>("token: [s3cr3t").unwrap_err(),
        )));
        assert_eq!(login_failure(&e).as_deref(), Some("the login command's output could not be read as a credential"));
        // Other auth errors say what they say.
        assert_eq!(login_failure(&AuthError::MissingCommand), None);
        assert_eq!(login_failure(&kube::Error::Auth(AuthError::ExecPluginFailed)), None);
    }

    #[test]
    fn the_program_is_found_in_the_command_line() {
        let debug = |cmd: &Command| program_name(&format!("{cmd:?}"));
        assert_eq!(debug(&kubelogin()).as_deref(), Some("kubelogin"));
        let mut cmd = Command::new("aws");
        cmd.args(["eks", "get-token"]).current_dir("/tmp/a \"b\" && c").env_clear().env("A", "1");
        assert_eq!(debug(&cmd).as_deref(), Some("aws"));
        let mut cmd = Command::new("/opt/bin/gke-gcloud-auth-plugin");
        cmd.arg0("s3cr3t-arg0").env("B", "\\\" \"x\"");
        assert_eq!(debug(&cmd).as_deref(), Some("gke-gcloud-auth-plugin"));
        // A gcp auth provider: `cmd-path` then `cmd-args`.
        assert_eq!(
            program_name("/usr/lib/google-cloud-sdk/bin/gcloud config config-helper").as_deref(),
            Some("gcloud")
        );
        // A name that is not a plain file name, or a line that doesn't read, names nothing.
        assert_eq!(debug(&Command::new("/bin/a b")), None);
        assert_eq!(program_name("KEY=\"s3cr3t\" unquoted"), None);
        assert_eq!(program_name("KEY=\"unterminated s3cr3t"), None);
        assert_eq!(program_name(""), None);
    }

    #[test]
    fn stderr_is_read_as_plain_text() {
        assert_eq!(plain("\x1b[1;31mfailed\x1b[0m:\tnot\rlogged in "), "failed:notlogged in");
        assert_eq!(last_line(b"first\n\x1b[0m\n  \n"), Some("first".into()));
        assert_eq!(last_line(b"\xffbad\n"), Some("\u{fffd}bad".into()));
        assert_eq!(last_line(b""), None);
    }
}

/// Whatever a failed login command was given and printed, what is said of it.
#[cfg(test)]
mod properties {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Command, ExitStatus, Output};

    use proptest::prelude::*;

    use super::{AuthError, MAX_DETAIL, login_failure};

    /// In everything the command was given and printed, never in what is said of it.
    const SECRET: &str = "XSECRETX";

    /// What is said of a command that exited with `code` after printing this.
    fn say(cmd: &Command, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> String {
        let out = Output { status: ExitStatus::from_raw(code << 8), stdout, stderr };
        login_failure(&AuthError::AuthExecRun { cmd: format!("{cmd:?}"), out: Box::new(out) }).unwrap()
    }

    proptest! {
        /// Its arguments, environment, folder, `argv[0]` and standard output never show,
        /// whatever they hold (quotes, spaces, `=`, backslashes, any character); its program's
        /// name and exit code always do, and so does the last line of stderr with text in it.
        #[test]
        fn a_failed_login_never_shows_what_it_was_given(
            dir in proptest::option::of("/[a-z]{1,8}"),
            name in "[a-z][a-z0-9._-]{0,15}",
            args in proptest::collection::vec("[^\\x00]{0,12}", 0..4),
            env in proptest::collection::vec(("[A-Z_]{0,6}", "[^\\x00]{0,12}"), 0..3),
            removed in proptest::collection::vec("[A-Z_]{0,6}", 0..2),
            cwd in proptest::option::of("[^\\x00]{0,12}"),
            clear in any::<bool>(),
            arg0 in proptest::option::of("[^\\x00]{0,12}"),
            code in 1..=255i32,
            stdout in "[^\\x00]{0,20}",
            stderr in proptest::collection::vec("[a-z :]{0,20}", 0..4),
        ) {
            let mut cmd = Command::new(dir.map_or_else(|| name.clone(), |dir| format!("{dir}/{name}")));
            cmd.args(args.iter().map(|a| format!("{SECRET}{a}")));
            if clear {
                cmd.env_clear();
            }
            for key in &removed {
                cmd.env_remove(format!("{SECRET}{key}"));
            }
            for (key, value) in &env {
                cmd.env(format!("{SECRET}{key}"), format!("{SECRET}{value}"));
            }
            if let Some(cwd) = &cwd {
                cmd.current_dir(format!("{SECRET}{cwd}"));
            }
            if let Some(arg0) = &arg0 {
                cmd.arg0(format!("{SECRET}{arg0}"));
            }
            let said = say(&cmd, code, format!("{SECRET}{stdout}").into_bytes(), stderr.join("\n").into_bytes());
            prop_assert!(!said.contains(SECRET), "{said}\nfrom {cmd:?}");
            prop_assert!(said.contains(&format!("`{name}`")), "{said}\nfrom {cmd:?}");
            prop_assert!(said.contains(&format!("(exit {code})")), "{said}");
            match stderr.iter().rev().map(String::as_str).map(str::trim).find(|l| !l.is_empty()) {
                Some(last) => prop_assert!(said.ends_with(&format!("): {last}")), "{said}"),
                None => prop_assert!(said.ends_with(')'), "{said}"),
            }
        }

        /// Whatever bytes it wrote to stderr, one line of text comes out: no control
        /// characters, at most `MAX_DETAIL` characters and an ellipsis.
        #[test]
        fn stderr_comes_out_as_one_short_line(stderr in proptest::collection::vec(any::<u8>(), 0..600)) {
            let said = say(&Command::new("kubelogin"), 1, Vec::new(), stderr);
            prop_assert!(!said.chars().any(char::is_control), "{said:?}");
            let detail = said.strip_prefix("the login command `kubelogin` failed (exit 1)").unwrap();
            prop_assert!(detail.is_empty() || detail.starts_with(": "), "{said:?}");
            prop_assert!(detail.chars().count() <= ": ".len() + MAX_DETAIL + 1, "{said:?}");
        }
    }
}
