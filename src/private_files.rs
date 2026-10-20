//! Files that may hold secrets: temporary ones only we can read, and saved ones that
//! never replace a file already there.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A directory only we can read (mode 0700, fresh random name), removed when dropped.
pub struct PrivateDir(PathBuf);

impl PrivateDir {
    pub fn create() -> std::io::Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("recouvrance-{}", uuid::Uuid::new_v4().simple()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        // Not recursive: fails if the path already exists, so we never reuse someone else's.
        b.create(&dir)?;
        Ok(Self(dir))
    }

    /// Write a new file readable only by us (mode 0600).
    pub fn write(&self, name: &str, text: &str) -> std::io::Result<PathBuf> {
        let path = self.0.join(name);
        create_private(&path)?.write_all(text.as_bytes())?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new file, mode 0600; fails if anything (a file, a symlink) is already at `path`.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// `name` in `dir`, or `name (1)`, `name (2)`… before the extension when taken.
fn numbered(name: &str, n: usize) -> String {
    if n == 0 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// Create a new private file (0600) named `name` in `dir`, numbered when the name is
/// taken: an earlier download is never overwritten. Returns the open file and its path.
pub fn create_new_in(dir: &Path, name: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    for n in 0..1000 {
        let path = dir.join(numbered(name, n));
        match create_private(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("too many files named {name}")))
}

/// ~/Downloads.
pub fn downloads_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_names() {
        assert_eq!(numbered("a.yaml", 0), "a.yaml");
        assert_eq!(numbered("a.yaml", 2), "a (2).yaml");
        assert_eq!(numbered("hostname", 1), "hostname (1)");
        assert_eq!(numbered(".env", 1), ".env (1)");
    }

    #[test]
    fn never_overwrites_and_is_private() {
        let dir = PrivateDir::create().unwrap();
        let (mut f, first) = create_new_in(&dir.0, "s.yaml").unwrap();
        f.write_all(b"one").unwrap();
        let (_, second) = create_new_in(&dir.0, "s.yaml").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&second).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let p = dir.write("v.yaml", "x").unwrap();
        assert!(dir.write("v.yaml", "y").is_err(), "create_new refuses an existing file");
        assert_eq!(std::fs::read_to_string(p).unwrap(), "x");
        let path = dir.0.clone();
        drop(dir);
        assert!(!path.exists());
    }
}
