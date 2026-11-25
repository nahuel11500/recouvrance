//! Programs the app starts (helm, plugins, the claude CLI, an editor, the file manager).
//!
//! On Windows the app is a GUI program: a console program it starts (helm, `sh`, `claude`, a
//! `.cmd` script such as VS Code's `code.cmd`) would open a console window of its own. These
//! commands ask for none (`CREATE_NO_WINDOW`; a GUI program ignores it). Their output is read
//! through pipes as on other systems. Terminals are not started here: they run in a pty.
//! Elsewhere they are plain commands.

use std::ffi::OsStr;

/// `CREATE_NO_WINDOW`: a console program runs without a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that opens no console window on Windows.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    cmd
}

/// The same, for tokio.
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(test)]
mod tests {
    /// Every program the app starts goes through here (tests and the developer tools may start
    /// theirs directly).
    #[test]
    fn the_app_starts_programs_only_through_this_module() {
        let mut found = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if !path.ends_with("dev") {
                        dirs.push(path);
                    }
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if !name.ends_with(".rs") || ["process.rs", "e2e.rs"].contains(&name.as_str()) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // The code before the file's tests.
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                for (i, line) in code.lines().enumerate() {
                    if line.contains("process::Command::new(") {
                        found.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "use crate::process::command or tokio_command:\n{}", found.join("\n"));
    }
}
