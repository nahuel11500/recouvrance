//! Files that may hold secrets (tokens, kubeconfigs, values, agent rules): only the user
//! can read them (0600, in 0700 directories), they are created new rather than written
//! through whatever is already there, and temporary ones are removed when dropped.
//!
//! On Windows there are no modes: a new file takes the access list of its folder. Every folder
//! these files go in is `%LOCALAPPDATA%\recouvrance` (config, data and temporary files, see
//! `paths`), which only the user, administrators and the system can read by default, or one the
//! user picked (downloads).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Create `dir` and its missing parents, readable only by the user.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    private_dir(DirBuilder::new().recursive(true)).create(dir)
}

/// `builder` making directories only the user can read (0700); a no-op on Windows.
fn private_dir(builder: &mut DirBuilder) -> &mut DirBuilder {
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
    builder
}

/// `options` creating files only the user can read (0600); a no-op on Windows, where a new
/// file takes the access list of its folder.
pub fn private_file(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options
}

/// Replace `path` atomically with a file only the user can read: a fresh temporary file
/// next to it (never one someone else prepared, or left over with looser permissions),
/// written, flushed, then renamed over it. A crash never leaves half a file.
///
/// A symlink at `path` (a dotfiles repo, stow, home-manager) is written through: the file
/// it leads to is replaced, next to itself, and the link stays a link. Renaming over the
/// link would turn it into a regular file, and later edits where it pointed wouldn't be read.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let path = &through_links(path)?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_dir(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = create_new(&tmp).and_then(|mut f| {
        // The umask can only remove bits; set them anyway so the result never depends on it.
        #[cfg(unix)]
        f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Where `path` leads once its symlinks are followed (a relative target from the link's
/// folder), or `path` itself when it is not a link. A link to nothing yet leads to where
/// its target would be, so writing creates the target and keeps the link.
pub fn through_links(path: &Path) -> io::Result<PathBuf> {
    let mut at = path.to_path_buf();
    // As many links as Linux follows before ELOOP.
    for _ in 0..40 {
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&at)?;
                // An absolute target replaces the folder when joined.
                at = at.parent().map_or_else(|| target.clone(), |dir| dir.join(&target));
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => return Ok(at),
        }
    }
    Err(io::Error::other(format!("{}: too many levels of symbolic links", path.display())))
}

/// A new file, mode 0600; fails if anything (a file, a symlink) is already at `path`.
fn create_new(path: &Path) -> io::Result<File> {
    private_file(OpenOptions::new().write(true).create_new(true)).open(path)
}

/// A directory only the user can read (mode 0700, fresh random name in the runtime
/// directory), removed with what it holds when dropped.
pub struct PrivateDir(PathBuf);

impl PrivateDir {
    pub fn create() -> io::Result<Self> {
        let dir = crate::paths::runtime_dir().join(format!("recouvrance-{}", uuid::Uuid::new_v4().simple()));
        // Not recursive: fails if the path already exists, so we never reuse someone else's.
        private_dir(&mut DirBuilder::new()).create(&dir)?;
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
        private_file(OpenOptions::new().write(true).create(true).truncate(true)).open(&path)?.write_all(contents)?;
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

/// A file's permission bits, for tests. Windows has no modes (a new file takes its folder's
/// access list): there the mode a private file or folder stands for, so the other checks
/// of a test still run.
#[cfg(all(test, unix))]
pub fn mode(p: &Path) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o777
}

/// A file's permission bits, for tests: on Windows, the ones a private file or folder stands for.
#[cfg(all(test, not(unix)))]
pub fn mode(p: &Path) -> u32 {
    if p.is_dir() { 0o700 } else { 0o600 }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::{PrivateDir, TempFile, create_new_in, mode, numbered, save_new, write_atomic};

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

    /// A config file kept as a symlink (into a dotfiles repo) stays one when written: the
    /// file it leads to gets the contents, whether the link is relative, absolute, chained,
    /// or leads to nothing yet. A link loop is refused rather than replaced.
    #[cfg(unix)]
    #[test]
    fn atomic_writes_go_through_symlinks() {
        let is_link = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().file_type().is_symlink();
        let dir = PrivateDir::create().unwrap();
        let (config, repo) = (dir.path().join("config"), dir.path().join("dotfiles"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let target = repo.join("rules.yaml");
        std::fs::write(&target, b"old").unwrap();
        let cases = [
            (config.join("relative.yaml"), std::path::PathBuf::from("../dotfiles/rules.yaml")),
            (config.join("absolute.yaml"), target.clone()),
        ];
        for (link, to) in &cases {
            symlink(to, link).unwrap();
            write_atomic(link, link.to_string_lossy().as_bytes()).unwrap();
            assert!(is_link(link), "{}", link.display());
            assert_eq!(std::fs::read(&target).unwrap(), link.to_string_lossy().as_bytes());
            assert_eq!(mode(&target), 0o600);
        }
        // A link to a link.
        let chained = config.join("chained.yaml");
        symlink("absolute.yaml", &chained).unwrap();
        write_atomic(&chained, b"chained").unwrap();
        assert!(is_link(&chained) && is_link(&config.join("absolute.yaml")));
        assert_eq!(std::fs::read(&target).unwrap(), b"chained");
        // A link whose target isn't there yet: the target is created.
        let dangling = config.join("new.json");
        symlink("../dotfiles/new.json", &dangling).unwrap();
        write_atomic(&dangling, b"{}").unwrap();
        assert!(is_link(&dangling));
        assert_eq!(std::fs::read(repo.join("new.json")).unwrap(), b"{}");
        // No temporary file left in either folder.
        assert_eq!(std::fs::read_dir(&repo).unwrap().count(), 2);
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 4);
        // A loop is refused, and left as it was.
        let (a, b) = (config.join("a"), config.join("b"));
        symlink("b", &a).unwrap();
        symlink("a", &b).unwrap();
        assert!(write_atomic(&a, b"x").is_err());
        assert!(is_link(&a) && is_link(&b));
    }
}
