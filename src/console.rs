//! Windows builds the release app for the GUI subsystem (see the top of `main.rs`): no console
//! window opens with it. A command line run from a terminal still has to print, so the commands
//! attach to the console of the program that started them.
//!
//! The app itself gets a console without a window (`hidden_for_gui`): a console program that a
//! library starts for it, such as a kubeconfig's login command (`aws eks get-token`, `kubelogin`,
//! `gke-gcloud-auth-plugin`) run by kube's client at each token refresh, shares that console
//! instead of opening a window of its own. Programs the app starts itself also pass
//! `crate::process`, which asks for no window.

/// Whether these arguments run a command line that prints (and exits, or serves), not the app.
pub fn is_cli(args: &[String]) -> bool {
    matches!(args.get(1).map(String::as_str), Some("serve" | "agent-create" | "--version" | "-V" | "--help" | "-h"))
}

/// Attach to the parent's console (the terminal the command was typed in) when neither stdout nor
/// stderr goes anywhere. Output redirected to a file or a pipe is inherited even by a GUI program
/// and is left alone. Started from Explorer, there is no parent console and nothing happens.
///
/// The shell doesn't wait for a GUI program: in `cmd` and PowerShell the prompt comes back at once
/// and the command's output is printed after it (docs/configuration.md says how to wait).
#[cfg(windows)]
pub fn attach_for_cli() {
    if !usable(&std::io::stdout()) && !usable(&std::io::stderr()) {
        // Fails when the parent has no console: there is nowhere to print then.
        let _ = winsafe::AttachConsole(winsafe::PidParent::Parent);
    }
}

/// A standard handle that can be written to. A GUI program started from a terminal without
/// redirection has none: its handle is null (or invalid) and can't be duplicated.
#[cfg(windows)]
fn usable(stream: &impl std::os::windows::io::AsHandle) -> bool {
    stream.as_handle().try_clone_to_owned().is_ok()
}

/// Linux and macOS: the program always has the terminal it was started from.
#[cfg(not(windows))]
pub fn attach_for_cli() {}

/// Give the app (a GUI program, which has no console) a console without a window, before it
/// starts anything. Windows has no call for that the crate can make without `unsafe`, so a
/// console program is started without a window (`cmd`, waiting on its stdin), which makes such
/// a console, and the app attaches to it; `cmd` is then let go and the console stays with the
/// app. Every console program started afterwards inherits it: none opens a window. If anything
/// fails, the app runs as before, without a console. Not when its output was redirected (then
/// a login command may still open a window).
#[cfg(all(windows, not(feature = "dev")))]
pub fn hidden_for_gui() {
    // Output redirected by whoever started the app is left alone: attaching could take it over.
    if usable(&std::io::stdout()) || usable(&std::io::stderr()) {
        return;
    }
    if let Err(e) = attach_windowless() {
        eprintln!("recouvrance: no hidden console ({e}); a login command may open a console window");
    }
}

/// Dev builds are console programs (they have one), and other systems have no console windows.
#[cfg(not(all(windows, not(feature = "dev"))))]
pub fn hidden_for_gui() {}

/// Attach to a new console that has no window.
#[cfg(all(windows, any(test, not(feature = "dev"))))]
fn attach_windowless() -> std::io::Result<()> {
    use std::process::Stdio;
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    // `/k` keeps cmd running, reading commands from stdin, until stdin is closed.
    let mut host = crate::process::command(shell)
        .args(["/d", "/q", "/k"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let attached = winsafe::AttachConsole(winsafe::PidParent::Pid(host.id()));
    // Closing its stdin ends cmd; the console stays while the app is attached to it.
    drop(host.stdin.take());
    let _ = std::thread::Builder::new().name("reap".into()).spawn(move || host.wait());
    attached.map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    /// A process without a console (the GUI app, here a test that left its console) gets one,
    /// started without a window, which the console programs it starts then share.
    #[cfg(windows)]
    #[test]
    fn a_process_without_a_console_gets_a_windowless_one() {
        const CHILD: &str = "RECOUVRANCE_TEST_WINDOWLESS";
        if std::env::var_os(CHILD).is_some() {
            winsafe::FreeConsole().unwrap();
            super::attach_windowless().unwrap();
            // Attached to a console now: attaching to another one is refused.
            let again = winsafe::AttachConsole(winsafe::PidParent::Parent);
            assert_eq!(again, Err(winsafe::co::ERROR::ACCESS_DENIED));
            return;
        }
        let me = std::env::current_exe().unwrap();
        let out = crate::process::command(me)
            .args(["--exact", "console::tests::a_process_without_a_console_gets_a_windowless_one", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    use super::is_cli;

    fn args(a: &[&str]) -> Vec<String> {
        std::iter::once("recouvrance").chain(a.iter().copied()).map(String::from).collect()
    }

    #[test]
    fn commands_attach_and_the_app_does_not() {
        for cli in [&["serve"][..], &["agent-create", "ci", "kind-x"], &["--version"], &["-V"], &["--help"], &["-h"]] {
            assert!(is_cli(&args(cli)), "{cli:?}");
        }
        for app in [&[][..], &["--snapshot", "a.png"], &["unknown"]] {
            assert!(!is_cli(&args(app)), "{app:?}");
        }
    }
}
