//! The app menu entry and icons on Linux, for the installs that don't add them (the shell
//! installer, a tarball, `cargo install`). It does what `scripts/install.sh` does: the icons into
//! the user's hicolor theme, and `recouvrance.desktop` with `Exec` set to this binary's absolute
//! path. The .deb (and any distribution package) has its own entry in `/usr/share`, left alone.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

const DESKTOP: &str = include_str!("../assets/recouvrance.desktop");
const ICON: &[u8] = include_bytes!("../assets/recouvrance.svg");
/// A simpler drawing for menus and panels, where the full ship is too detailed.
const ICON_SMALL: &[u8] = include_bytes!("../assets/recouvrance-small.svg");
/// The first line of an entry the app wrote: only such an entry is ever replaced.
const MARK: &str = "# Written by Recouvrance; `recouvrance --install-desktop-entry` writes it again.";
/// The file name matches the window's Wayland app id, so the dock shows the icon.
const FILE: &str = "recouvrance.desktop";

/// What `install` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The icons and the entry were written; the entry's path.
    Written(PathBuf),
    /// An entry the app didn't write (the user's, `scripts/install.sh`'s) is there: kept.
    Kept(PathBuf),
}

/// `$XDG_DATA_HOME` when it is an absolute path (the spec says to ignore a relative one), else
/// `~/.local/share`.
fn data_home_from(var: Option<OsString>, home: &Path) -> PathBuf {
    var.map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".local/share"))
}

fn data_home() -> PathBuf {
    data_home_from(std::env::var_os("XDG_DATA_HOME"), &crate::paths::home())
}

/// The other folders desktop entries are read from: `$XDG_DATA_DIRS`, by default
/// `/usr/local/share:/usr/share`.
fn data_dirs_from(var: Option<OsString>) -> Vec<PathBuf> {
    let var = var.filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::env::split_paths(&var).filter(|p| p.is_absolute()).collect()
}

fn entry_in(data: &Path) -> PathBuf {
    data.join("applications").join(FILE)
}

/// `path` as the `Exec` key's program: the Desktop Entry spec wants it double-quoted with each
/// backslash, double quote, backtick and dollar sign backslash-escaped, then every backslash
/// escaped again (the string value rule), and `%` doubled. None when it can't be written on one
/// line.
fn exec_quoted(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    for c in path.chars() {
        match c {
            '\\' => quoted.push_str(r"\\\\"),
            '"' | '`' | '$' => {
                quoted.push_str(r"\\");
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    Some(format!("\"{quoted}\""))
}

/// The repository's entry, marked as the app's, with `Exec` running `exe`.
fn entry_text(exe: &str) -> Option<String> {
    let exec = exec_quoted(exe)?;
    let mut text = format!("{MARK}\n");
    for line in DESKTOP.lines() {
        if line.starts_with("Exec=") {
            text.push_str(&format!("Exec={exec}\n"));
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    Some(text)
}

/// Replace `path` with `contents` (readable by all, as the desktop expects): a fresh temporary
/// file next to it, written, flushed, then renamed over it. A crash never leaves half a file.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).and_then(|mut f| {
        #[cfg(unix)]
        f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Install the icons and the entry under `data` (the user's data folder) for the binary at
/// `exe`. An entry already there is kept, unless the app wrote it and `replace_own` is set.
fn install_in(data: &Path, exe: &Path, replace_own: bool) -> io::Result<Outcome> {
    let entry = entry_in(data);
    match std::fs::read_to_string(&entry) {
        Ok(text) if !(replace_own && text.starts_with(MARK)) => return Ok(Outcome::Kept(entry)),
        // Not text: someone else's file, kept as well.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(Outcome::Kept(entry)),
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let exe = exe.to_str().ok_or_else(|| io::Error::other(format!("{} is not UTF-8", exe.display())))?;
    let text = entry_text(exe).ok_or_else(|| io::Error::other(format!("{exe:?} can't be written in an Exec key")))?;
    let hicolor = data.join("icons/hicolor");
    write_atomic(&hicolor.join("scalable/apps/recouvrance.svg"), ICON)?;
    for size in ["16x16", "24x24"] {
        write_atomic(&hicolor.join(size).join("apps/recouvrance.svg"), ICON_SMALL)?;
    }
    // The entry last: it shows in the menu with its icon already there.
    write_atomic(&entry, text.as_bytes())?;
    refresh_caches(data);
    Ok(Outcome::Written(entry))
}

/// Tell the desktop about the new files, as `scripts/install.sh` does; either tool may be absent.
fn refresh_caches(data: &Path) {
    let quiet = |program: &str, args: &[&Path]| {
        let _ = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    quiet("update-desktop-database", &[&data.join("applications")]);
    quiet("gtk-update-icon-cache", &[Path::new("-q"), &data.join("icons/hicolor")]);
}

/// The binary a menu entry should run: the `AppImage` itself rather than its mount point, which
/// changes every time it starts.
fn program() -> io::Result<PathBuf> {
    let install = crate::updates::install();
    match (install.method, &install.path) {
        (crate::updates::Method::AppImage, Some(image)) => Ok(image.clone()),
        _ => std::env::current_exe(),
    }
}

/// `recouvrance --install-desktop-entry`: write the entry and icons for this binary, replacing an
/// entry the app wrote before (the binary moved), never one it didn't. Returns the exit code.
pub fn install_command() -> i32 {
    if !cfg!(target_os = "linux") {
        eprintln!("A desktop entry is only for Linux: the app is already in the menus elsewhere.");
        return 1;
    }
    match program().and_then(|exe| install_in(&data_home(), &exe, true)) {
        Ok(Outcome::Written(entry)) => {
            println!("Added Recouvrance to the app menu: {}", entry.display());
            0
        }
        Ok(Outcome::Kept(entry)) => {
            eprintln!(
                "{} is already there and Recouvrance didn't write it: kept. Remove it and run this again to replace it.",
                entry.display()
            );
            1
        }
        Err(e) => {
            eprintln!("Couldn't add the desktop entry: {e}");
            1
        }
    }
}

/// Whether the first start installs an entry for the binary at `exe`: not for a package's binary
/// under `/usr` (it brings its own), an `AppImage` (often moved, or integrated by another tool),
/// or a build in a cargo `target` folder.
fn auto_installs(exe: &Path, appimage: bool) -> bool {
    let built = exe.ancestors().skip(2).take(2).any(|dir| dir.file_name().is_some_and(|n| n == "target"));
    !(exe.starts_with("/usr") || appimage || built)
}

/// On Linux, the first time the app starts: install the entry and icons when no entry exists
/// anywhere the desktop looks, so an install without one is still in the app menu. Done once: an
/// entry the user removes later stays removed. In the background; a failure is only logged.
pub fn on_start() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let appimage = crate::updates::install().method == crate::updates::Method::AppImage;
    let Ok(exe) = std::env::current_exe() else { return };
    if !auto_installs(&exe, appimage) {
        return;
    }
    let done = crate::paths::data_dir().join("desktop-entry");
    if done.exists() {
        return;
    }
    std::thread::spawn(move || {
        let data = data_home();
        let mut dirs = vec![data.clone()];
        dirs.extend(data_dirs_from(std::env::var_os("XDG_DATA_DIRS")));
        if dirs.iter().any(|d| entry_in(d).exists()) {
            return;
        }
        match install_in(&data, &exe, false) {
            Ok(Outcome::Written(entry)) => {
                let _ = crate::private_files::write_atomic(&done, entry.to_string_lossy().as_bytes())
                    .map_err(|e| eprintln!("{}: {e}", done.display()));
            }
            Ok(Outcome::Kept(_)) => {}
            Err(e) => eprintln!("desktop entry: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        FILE, ICON, ICON_SMALL, MARK, Outcome, auto_installs, data_dirs_from, data_home_from, exec_quoted, install_in,
    };
    use crate::private_files::PrivateDir;

    #[test]
    fn exec_is_quoted_as_the_spec_says() {
        assert_eq!(exec_quoted("/home/u/.cargo/bin/recouvrance").unwrap(), r#""/home/u/.cargo/bin/recouvrance""#);
        assert_eq!(exec_quoted("/a b/r").unwrap(), r#""/a b/r""#);
        // Inside the quotes `"` `$` `` ` `` and `\` take a backslash, then every backslash is
        // doubled again for the string value, and `%` is doubled for the field codes.
        assert_eq!(exec_quoted(r#"/x"y"#).unwrap(), r#""/x\\"y""#);
        assert_eq!(exec_quoted("/x$y`z").unwrap(), r#""/x\\$y\\`z""#);
        assert_eq!(exec_quoted(r"/x\y").unwrap(), r#""/x\\\\y""#);
        assert_eq!(exec_quoted("/100%/r").unwrap(), r#""/100%%/r""#);
        assert_eq!(exec_quoted("/a\nb"), None);
    }

    #[test]
    fn data_folders_follow_xdg() {
        let home = Path::new("/home/u");
        assert_eq!(data_home_from(None, home), Path::new("/home/u/.local/share"));
        assert_eq!(data_home_from(Some("/d".into()), home), Path::new("/d"));
        // A relative or empty value is ignored, as the spec says.
        assert_eq!(data_home_from(Some("rel".into()), home), Path::new("/home/u/.local/share"));
        assert_eq!(data_home_from(Some("".into()), home), Path::new("/home/u/.local/share"));
        assert_eq!(data_dirs_from(None), [Path::new("/usr/local/share"), Path::new("/usr/share")]);
        assert_eq!(data_dirs_from(Some("/a:rel:/b".into())), [Path::new("/a"), Path::new("/b")]);
    }

    #[test]
    fn only_installs_without_a_package() {
        assert!(auto_installs(Path::new("/home/u/.cargo/bin/recouvrance"), false));
        assert!(auto_installs(Path::new("/opt/recouvrance/recouvrance"), false));
        assert!(!auto_installs(Path::new("/usr/bin/recouvrance"), false));
        assert!(!auto_installs(Path::new("/home/u/.cargo/bin/recouvrance"), true));
        assert!(!auto_installs(Path::new("/src/recouvrance/target/release/recouvrance"), false));
        assert!(!auto_installs(Path::new("/src/r/target/x86_64-unknown-linux-gnu/debug/recouvrance"), false));
    }

    #[test]
    fn installs_the_entry_and_icons_and_keeps_others() {
        let data = PrivateDir::create().unwrap();
        let data = data.path();
        let exe = Path::new("/home/u/my apps/$bin/recouvrance");
        let entry = data.join("applications").join(FILE);
        assert_eq!(install_in(data, exe, false).unwrap(), Outcome::Written(entry.clone()));

        let text = std::fs::read_to_string(&entry).unwrap();
        assert!(text.starts_with(MARK));
        let exec: Vec<_> = text.lines().filter(|l| l.starts_with("Exec=")).collect();
        assert_eq!(exec, [r#"Exec="/home/u/my apps/\\$bin/recouvrance""#]);
        assert!(text.contains("\nIcon=recouvrance\n") && text.contains("\nName=Recouvrance\n"));
        let icons = data.join("icons/hicolor");
        assert_eq!(std::fs::read(icons.join("scalable/apps/recouvrance.svg")).unwrap(), ICON);
        for size in ["16x16", "24x24"] {
            assert_eq!(std::fs::read(icons.join(size).join("apps/recouvrance.svg")).unwrap(), ICON_SMALL);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&entry).unwrap().permissions().mode() & 0o777, 0o644);
        }
        // No temporary file left behind (update-desktop-database, when installed, adds its cache).
        let mut left = std::fs::read_dir(data.join("applications")).unwrap().map(|e| e.unwrap().file_name());
        assert!(!left.any(|n| n.to_string_lossy().ends_with(".tmp")));

        // The first start never replaces an entry, even its own; the command replaces its own.
        let moved = Path::new("/opt/r/recouvrance");
        assert_eq!(install_in(data, moved, false).unwrap(), Outcome::Kept(entry.clone()));
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), text);
        assert_eq!(install_in(data, moved, true).unwrap(), Outcome::Written(entry.clone()));
        assert!(std::fs::read_to_string(&entry).unwrap().contains("\nExec=\"/opt/r/recouvrance\"\n"));

        // An entry the app didn't write is kept, even by the command.
        std::fs::write(&entry, "[Desktop Entry]\nExec=mine\n").unwrap();
        assert_eq!(install_in(data, exe, true).unwrap(), Outcome::Kept(entry.clone()));
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "[Desktop Entry]\nExec=mine\n");
    }
}
