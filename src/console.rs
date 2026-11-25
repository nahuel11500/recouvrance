//! Windows builds the release app for the GUI subsystem (see the top of `main.rs`): no console
//! window opens with it. A command line run from a terminal still has to print, so the commands
//! attach to the console of the program that started them.

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

#[cfg(test)]
mod tests {
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
