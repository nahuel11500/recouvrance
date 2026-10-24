//! Files that may hold secrets (tokens, kubeconfigs, values, agent rules): only the user
//! can read them (0600, in 0700 directories), they are created new rather than written
//! through whatever is already there, and temporary ones are removed when dropped.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Create `dir` and its missing parents, readable only by the user.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Replace `path` atomically with a file only the user can read: a fresh temporary file
/// next to it (never one someone else prepared, or left over with looser permissions),
/// written, flushed, then renamed over it. A crash never leaves half a file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_dir(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = create_new(&tmp).and_then(|mut f| {
        // The umask can only remove bits; set them anyway so the result never depends on it.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A new file, mode 0600; fails if anything (a file, a symlink) is already at `path`.
fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

/// A directory only the user can read (mode 0700, fresh random name in the runtime
/// directory), removed with what it holds when dropped.
pub struct PrivateDir(PathBuf);

impl PrivateDir {
    pub fn create() -> io::Result<Self> {
        let dir = crate::paths::runtime_dir().join(format!("recouvrance-{}", uuid::Uuid::new_v4().simple()));
        // Not recursive: fails if the path already exists, so we never reuse someone else's.
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write a new file readable only by the user; fails if `name` is already there.
    pub fn write(&self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        create_new(&path)?.write_all(contents)?;
        Ok(path)
    }

    /// Write `name`, replacing the file this directory already holds under that name.
    pub fn overwrite(&self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?.write_all(contents)?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private file (0600, created new) removed when dropped, whatever happens in between.
pub struct TempFile(PathBuf);

impl TempFile {
    pub fn create(path: PathBuf, contents: &[u8]) -> io::Result<Self> {
        let mut f = create_new(&path)?;
        let file = Self(path);
        f.write_all(contents)?;
        Ok(file)
    }

    /// A fresh file in the runtime directory, its name made of `stem`, the process id and
    /// a random part, so concurrent uses never share one.
    pub fn in_runtime_dir(stem: &str, ext: &str, contents: &[u8]) -> io::Result<Self> {
        let name = format!("recouvrance-{}-{stem}-{}.{ext}", std::process::id(), uuid::Uuid::new_v4().simple());
        Self::create(crate::paths::runtime_dir().join(name), contents)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `name`, or `name (1)`, `name (2)`… before the extension: how a download is renamed.
fn numbered(name: &str, n: usize) -> String {
    if n == 0 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// `stem.ext`, or `stem-1.ext`, `stem-2.ext`…: a saved kubeconfig's stem names its
/// contexts when they are renamed (`name@stem`), so it stays a plain word.
pub fn dashed(stem: &str, ext: &str, n: usize) -> String {
    if n == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{n}.{ext}") }
}

/// The first name `name(n)` gives that `dir` doesn't have, created as a new private file
/// there: an earlier file is never overwritten. Returns the open file and its path.
fn create_first_free(dir: &Path, name: impl Fn(usize) -> String) -> io::Result<(File, PathBuf)> {
    create_dir(dir)?;
    for n in 0..1000 {
        let path = dir.join(name(n));
        match create_new(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("too many files named {} in {}", name(0), dir.display())))
}

/// A new private file `name` in `dir`, numbered `name (1)`… when taken (downloads).
pub fn create_new_in(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    create_first_free(dir, |n| numbered(name, n))
}

/// Save `contents` as a new private `stem.ext` in `dir`, or `stem-1.ext`… when taken.
pub fn save_new(dir: &Path, stem: &str, ext: &str, contents: &[u8]) -> io::Result<PathBuf> {
    let (mut f, path) = create_first_free(dir, |n| dashed(stem, ext, n))?;
    f.write_all(contents)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    use super::{PrivateDir, TempFile, create_new_in, numbered, save_new, write_atomic};

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

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
        let (mut f, first) = create_new_in(dir.path(), "s.yaml").unwrap();
        f.write_all(b"one").unwrap();
        let (_, second) = create_new_in(dir.path(), "s.yaml").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(mode(&second), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
        let a = save_new(dir.path(), "kubeconfig", "yaml", b"a").unwrap();
        let b = save_new(dir.path(), "kubeconfig", "yaml", b"b").unwrap();
        assert_eq!(b.file_name().unwrap(), "kubeconfig-1.yaml");
        assert_eq!(std::fs::read(a).unwrap(), b"a");
        let p = dir.write("v.yaml", b"x").unwrap();
        assert!(dir.write("v.yaml", b"y").is_err(), "create_new refuses an existing file");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x");
        dir.overwrite("v.yaml", b"z").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "z");
        let path = dir.path().to_path_buf();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn temporary_files_are_private_and_go_away() {
        let (a, b) = (PrivateDir::create().unwrap(), PrivateDir::create().unwrap());
        assert_ne!(a.path(), b.path());
        let cfg = TempFile::create(a.path().join("mcp.json"), b"{}").unwrap();
        assert_eq!(mode(cfg.path()), 0o600);
        // Never written through an existing file.
        assert!(TempFile::create(a.path().join("mcp.json"), b"x").is_err());
        let file = cfg.path().to_path_buf();
        drop(cfg);
        assert!(!file.exists());
        let t = TempFile::in_runtime_dir("helm", "yaml", b"k").unwrap();
        assert_eq!(mode(t.path()), 0o600);
    }

    #[test]
    fn atomic_writes_are_private_and_replace() {
        let dir = PrivateDir::create().unwrap();
        let p = dir.path().join("sub").join("x.json");
        write_atomic(&p, b"1").unwrap();
        write_atomic(&p, b"2").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"2");
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(p.parent().unwrap()), 0o700);
        // No temporary file left behind.
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }
}
