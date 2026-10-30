//! A container's files, read by running small POSIX shell scripts in it: list a folder
//! (`stat`), preview a file (`head`), search by name (`find`), download (`cat`, `tar`).
//!
//! The scripts run on GNU and busybox alike. Names travel as bytes both ways: a path is
//! passed as `printf %b` escapes, and names come back NUL-separated, apart from the `stat`
//! lines, so spaces, newlines and non-UTF-8 bytes in them survive.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Result, anyhow};
use kube::Client;

use super::exec::{MAX_EXEC_OUTPUT, ended_reason, exec_to, save_download};

pub mod debug;

/// Where the files shown come from, said next to them: only a running container's own
/// show what its process wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Exec in the container itself: its live filesystem, mounts included.
    Live,
    /// Exec in a debug container sharing its processes, through `/proc/<pid>/root`: the
    /// same live filesystem, for an image without a shell or tools.
    Debug,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "live container",
            Self::Debug => "through a debug container",
        }
    }
}

/// How a container's files are reached: the pod and container the commands run in, and
/// the folder the filesystem is seen at from there. Exec in the container itself sees it at
/// `/`; another container sharing its processes would see it at `/proc/<pid>/root`, and a
/// copy of the pod at `/` of the copy. Paths given to and returned by the methods are the
/// container's own (`/etc`), whichever way they are read.
#[derive(Clone)]
pub struct FileSource {
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    /// Put before every path in the commands run, without a trailing `/` (empty for `/`).
    root: Vec<u8>,
    origin: Origin,
}

/// How long a listing or a preview may take (a hung network mount), and a search.
const LIST_SECS: u64 = 60;
const FIND_SECS: u64 = 120;
/// The most of a search's output read (its paths): past it, the search stops.
const MAX_FOUND_BYTES: usize = 1 << 20;

impl FileSource {
    /// Exec in `container` of the pod (its default one when none is named).
    pub fn exec(client: Client, ns: String, pod: String, container: Option<String>) -> Self {
        Self { client, ns, pod, container, root: Vec::new(), origin: Origin::Live }
    }

    /// Exec in debug container `debug` of the pod, which shares the processes of the
    /// container read: its process `pid` sees that container's filesystem at its root.
    pub fn debug(client: Client, ns: String, pod: String, debug: String, pid: u32) -> Self {
        let root = format!("/proc/{pid}/root").into_bytes();
        Self { client, ns, pod, container: Some(debug), root, origin: Origin::Debug }
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// The container the commands run in; none for the pod's default one.
    pub fn container(&self) -> Option<&str> {
        self.container.as_deref()
    }

    /// `path` as the commands see it.
    fn real(&self, path: &[u8]) -> Vec<u8> {
        [self.root.as_slice(), &normalize(path)].concat()
    }

    /// `real` the other way: a path the commands printed, as the container sees it.
    fn own(&self, real: &[u8]) -> Vec<u8> {
        match real.strip_prefix(self.root.as_slice()) {
            Some([]) => b"/".to_vec(),
            Some(rest) if rest.starts_with(b"/") => normalize(rest),
            _ => real.to_vec(),
        }
    }

    /// Owners by number only, when read from another container: `stat` names them from that
    /// container's `/etc/passwd`, not from the one read.
    fn owners(&self, entries: &mut [Entry]) {
        if self.root.is_empty() {
            return;
        }
        for e in entries {
            e.user.clear();
            e.group.clear();
        }
    }

    /// The entries of folder `dir`, folders first.
    pub async fn list(&self, dir: &[u8]) -> Result<Vec<Entry>> {
        let script = format!("{DECODE}{LIST}");
        let out = self.script(script, vec![sh_arg(&self.real(dir))], MAX_EXEC_OUTPUT, LIST_SECS).await?;
        let mut entries = parse_listing(&out)?;
        self.owners(&mut entries);
        sort_entries(&mut entries);
        Ok(entries)
    }

    /// The first `max` bytes of the regular file at `path`.
    pub async fn read(&self, path: &[u8], max: usize) -> Result<Preview> {
        let script = format!("{DECODE}{READ}");
        let args = vec![sh_arg(&self.real(path)), (max + 1).to_string()];
        let out = self.script(script, args, max as u64 + 1, LIST_SECS).await?;
        Ok(Preview::of(out, max))
    }

    /// A folder's entries, or a file's first `max` bytes: whichever `path` is.
    pub async fn open(&self, path: &[u8], max: usize) -> Result<Opened> {
        let script =
            format!("{DECODE}if [ -d \"$p\" ]; then\nprintf 'D\\000'\n{LIST}else\nprintf 'F\\000'\n{READ}fi\n");
        let args = vec![sh_arg(&self.real(path)), (max + 1).to_string()];
        let out = self.script(script, args, MAX_EXEC_OUTPUT, LIST_SECS).await?;
        match out.split_first() {
            Some((b'D', rest)) => {
                let mut entries = parse_listing(rest.strip_prefix(b"\0").unwrap_or(rest))?;
                self.owners(&mut entries);
                sort_entries(&mut entries);
                Ok(Opened::Folder(entries))
            }
            Some((b'F', rest)) => Ok(Opened::File(Preview::of(rest.strip_prefix(b"\0").unwrap_or(rest).to_vec(), max))),
            _ => Err(anyhow!("the container's answer could not be read")),
        }
    }

    /// The paths under `dir` whose name contains `name` (any case; `*` and `?` match
    /// anything). From `/`, `/proc` and `/sys` are left out.
    pub async fn find(&self, dir: &[u8], name: &str) -> Result<Found> {
        let dir = normalize(dir);
        let prune = if dir == b"/" { "\\( -path \"$r/proc\" -o -path \"$r/sys\" \\) -prune -o " } else { "" };
        // `$r/`: a folder given as a symlink (`/proc/<pid>/root`) is searched, not the link.
        let script = format!(
            "{DECODE}r=${{p%/}}\n{NEEDS_FIND}find \"$r/\" {prune}-iname \"*$2*\" -print0 2>/dev/null | head -c {MAX_FOUND_BYTES}\n"
        );
        let args = vec![sh_arg(&self.real(&dir)), name.to_string()];
        let out = self.script(script, args, MAX_FOUND_BYTES as u64, FIND_SECS).await?;
        let (paths, truncated) = parse_found(&out, MAX_FOUND_BYTES);
        Ok(Found { paths: paths.into_iter().map(|p| self.own(&p)).collect(), truncated })
    }

    /// Copy `path` to a new private file in ~/Downloads, a folder as a `.tar`, like
    /// `kubectl cp`. Returns where it was saved.
    pub async fn download(&self, path: &[u8], folder: bool) -> Result<std::path::PathBuf> {
        let path = normalize(path);
        let base = match display_name(base_name(&path)) {
            b if b.is_empty() => "root".to_string(),
            b => b.replace(['/', '\\'], "_"),
        };
        let real = self.real(&path);
        let (script, args, name) = if folder {
            let (parent, member) = tar_source(&path);
            let parent = self.real(&parent);
            let script = format!(
                "{DECODE}d=$(dec \"$2\"); d=${{d%x}}\n{IN_ROOT_D}m=$(dec \"$3\"); m=${{m%x}}\nexec tar cf - -C \"$d\" -- \"$m\"\n"
            );
            (script, vec![sh_arg(&real), sh_arg(&parent), sh_arg(&member)], format!("{}-{base}.tar", self.pod))
        } else {
            (format!("{DECODE}exec cat -- \"$p\"\n"), vec![sh_arg(&real)], format!("{}-{base}", self.pod))
        };
        let command = self.command(&script, args);
        let (client, ns, pod, container) =
            (self.client.clone(), self.ns.clone(), self.pod.clone(), self.container.clone());
        super::run_abortable(
            async move { save_download(client, ns, pod, container, command, &name).await.map_err(explain) },
        )
        .await
    }

    /// Run `script` with `args` (as `$1`…), and return what it printed: at most `max`
    /// bytes (more is an error), within `secs`.
    async fn script(&self, script: String, args: Vec<String>, max: u64, secs: u64) -> Result<Vec<u8>> {
        let command = self.command(&script, args);
        let (client, ns, pod, container) =
            (self.client.clone(), self.ns.clone(), self.pod.clone(), self.container.clone());
        // Abortable: a newer listing replacing this one ends the command in the container.
        super::run_abortable(async move {
            let mut out = Vec::new();
            let mut sink = |b: &[u8]| {
                out.extend_from_slice(b);
                Ok(())
            };
            let run = exec_to(client, ns, pod, container, command, None::<&[u8]>, &mut sink, max);
            let Ok(done) = tokio::time::timeout(Duration::from_secs(secs), run).await else {
                anyhow::bail!("the container did not answer within {secs}s");
            };
            done.map_err(explain)?;
            Ok(out)
        })
        .await
    }

    /// `sh -c script` with `args`, `$R` set to the root the paths are under (and `in_root`
    /// defined when there is one).
    fn command(&self, script: &str, args: Vec<String>) -> Vec<String> {
        let script = if self.root.is_empty() {
            format!("R=\n{script}")
        } else {
            let root = super::shell_quote(&String::from_utf8_lossy(&self.root));
            format!("R={root}\n{IN_ROOT}{script}")
        };
        let mut command = vec!["sh".to_string(), "-c".into(), script, "sh".into()];
        command.extend(args);
        command
    }
}

/// Decodes `$1` (a path passed as `printf %b` escapes) into `$p`. `dec` keeps a trailing
/// newline of a name, which `$(…)` alone would drop: it ends with an `x`, taken off after.
/// Under a root (`$R`), the path's symlinks are then followed as the container sees them.
const DECODE: &str = concat!(
    "dec() { printf '%b' \"$1\"; printf x; }\np=$(dec \"$1\"); p=${p%x}\n",
    "[ -z \"$R\" ] || { p=$(in_root \"${p#\"$R\"}\" && printf x) || exit; p=${p%x}; }\n"
);

/// The same for the folder `$d` a download archives from.
const IN_ROOT_D: &str = "[ -z \"$R\" ] || { d=$(in_root \"${d#\"$R\"}\" && printf x) || exit; d=${d%x}; }\n";

/// Prints `$R` followed by path `$1` with its symlinks followed as the container under `$R`
/// sees them: an absolute target from `$R`, not from the root of the container the commands
/// run in, which the kernel would use (`/proc/<pid>/root/var/run` leads to its own `/run`).
/// A part that isn't there is kept as it is, for the command after to say so.
const IN_ROOT: &str = r#"in_root() {
rest=$1 out= n=0
while [ -n "$rest" ]; do
c=${rest%%/*}
case $rest in */*) rest=${rest#*/} ;; *) rest= ;; esac
case $c in
'' | .) ;;
..) out=${out%/*} ;;
*)
if [ -L "$R$out/$c" ] && t=$(readlink -- "$R$out/$c" && printf x); then
n=$((n + 1))
[ "$n" -le 40 ] || { echo "$1: too many levels of symbolic links" >&2; return 1; }
t=${t%?x}
case $t in /*) out= ;; esac
rest=$t/$rest
else
out=$out/$c
fi
;;
esac
done
printf '%s' "$R$out"
}
"#;

/// Lists folder `$p`: its names (each ended by NUL, the list by an empty one), a `stat`
/// line for each, a NUL, then for each symlink its name, `d` when it leads to a folder, and
/// its target, each ended by NUL. `stat` gets the names through `xargs`, in batches the
/// system takes, so a big folder lists in one exec.
const LIST: &str = r#"cd "$p" || exit
set -- * .[!.]* ..?*
names() { for f in "$@"; do if [ -e "$f" ] || [ -L "$f" ]; then printf '%s\000' "$f"; fi; done; }
names "$@"
printf '\000'
names "$@" | xargs -0 -r stat -c '%f %h %u %g %s %Y %t %T %U %G %n' --
s=$?
[ "$s" -lt 126 ] || exit "$s"
printf '\000'
for f in "$@"; do
if [ -L "$f" ]; then
printf '%s\000' "$f"
t=$f
[ -z "$R" ] || { t=$(in_root "${p#"$R"}/$f" && printf x); t=${t%x}; }
if [ -d "$t" ]; then printf d; fi
printf '\000'
readlink -- "$f"
printf '\000'
fi
done
"#;

/// Prints the first `$2` bytes of regular file `$p`.
const READ: &str = r#"if [ -d "$p" ]; then echo "$p is a folder" >&2; exit 1; fi
if [ ! -f "$p" ]; then echo "$p is not a regular file" >&2; exit 1; fi
exec head -c "$2" "$p"
"#;

/// Says so when the container has no `find`, rather than finding nothing.
const NEEDS_FIND: &str =
    "command -v find >/dev/null 2>&1 || { echo 'there is no find command in this container' >&2; exit 127; }\n";

/// `e`, with the reason when the container has no shell to run the scripts.
fn explain(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}");
    match ended_reason(&text) {
        Some(hint) => anyhow!("{hint}: {text}"),
        None => e,
    }
}

/// Bytes as an argument for `printf %b`: printable ASCII as is, the rest (and `\`) as
/// `\0ooo` octal escapes, so any byte but NUL gets through.
pub(crate) fn sh_arg(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if (b.is_ascii_graphic() || b == b' ') && b != b'\\' {
            s.push(char::from(b));
        } else {
            let _ = write!(s, "\\0{b:03o}");
        }
    }
    s
}

/// What a folder entry is, from its mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    Other,
}

impl FileKind {
    fn of(mode: u32) -> Self {
        match mode & 0o170_000 {
            0o100_000 => Self::File,
            0o040_000 => Self::Dir,
            0o120_000 => Self::Symlink,
            0o020_000 => Self::CharDevice,
            0o060_000 => Self::BlockDevice,
            0o010_000 => Self::Fifo,
            0o140_000 => Self::Socket,
            _ => Self::Other,
        }
    }

    /// The first letter `ls -l` shows for it.
    fn letter(self) -> char {
        match self {
            Self::File => '-',
            Self::Dir => 'd',
            Self::Symlink => 'l',
            Self::CharDevice => 'c',
            Self::BlockDevice => 'b',
            Self::Fifo => 'p',
            Self::Socket => 's',
            Self::Other => '?',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "folder",
            Self::Symlink => "symbolic link",
            Self::CharDevice => "character device",
            Self::BlockDevice => "block device",
            Self::Fifo => "named pipe",
            Self::Socket => "socket",
            Self::Other => "special file",
        }
    }
}

/// One entry of a folder, as `stat` saw it (a symlink itself, not what it leads to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name's bytes, as on disk.
    pub name: Vec<u8>,
    pub kind: FileKind,
    /// Permission bits, setuid, setgid and sticky included.
    pub mode: u32,
    pub links: u64,
    pub uid: u64,
    pub gid: u64,
    /// The owner's and group's names, empty when the container doesn't know them.
    pub user: String,
    pub group: String,
    pub size: u64,
    /// Last modification, in seconds since the epoch.
    pub modified: i64,
    /// A device's major and minor numbers.
    pub device: Option<(u64, u64)>,
    /// A symlink's target, as written in it.
    pub target: Option<Vec<u8>>,
    /// A symlink that leads to a folder.
    pub target_is_dir: bool,
}

impl Entry {
    /// A folder, or a symlink to one: opening it lists it.
    pub fn opens(&self) -> bool {
        self.kind == FileKind::Dir || (self.kind == FileKind::Symlink && self.target_is_dir)
    }

    /// The name to show (see [`display_name`]).
    pub fn display(&self) -> String {
        display_name(&self.name)
    }

    /// The mode as `ls -l` shows it: `drwxr-xr-x`, `-rwsr-xr-x`, `drwxrwxrwt`.
    pub fn mode_string(&self) -> String {
        let m = self.mode;
        let bit = |mask: u32, c: char| if m & mask == 0 { '-' } else { c };
        // Execute, combined with setuid, setgid or sticky: lowercase with it, uppercase without.
        let exec = |x: u32, special: u32, lower: char| match (m & x != 0, m & special != 0) {
            (true, true) => lower,
            (false, true) => lower.to_ascii_uppercase(),
            (true, false) => 'x',
            (false, false) => '-',
        };
        [
            self.kind.letter(),
            bit(0o400, 'r'),
            bit(0o200, 'w'),
            exec(0o100, 0o4000, 's'),
            bit(0o040, 'r'),
            bit(0o020, 'w'),
            exec(0o010, 0o2000, 's'),
            bit(0o004, 'r'),
            bit(0o002, 'w'),
            exec(0o001, 0o1000, 't'),
        ]
        .into_iter()
        .collect()
    }

    /// `user:group`, by name when the container knows them, else by number.
    pub fn owner(&self) -> String {
        let user = if self.user.is_empty() { self.uid.to_string() } else { self.user.clone() };
        let group = if self.group.is_empty() { self.gid.to_string() } else { self.group.clone() };
        format!("{user}:{group}")
    }

    /// The size column: bytes for a file, the numbers of a device.
    pub fn size_text(&self) -> String {
        match (self.device, self.kind) {
            (Some((major, minor)), _) => format!("{major}, {minor}"),
            (None, FileKind::Dir) => String::new(),
            (None, _) => human_size(self.size),
        }
    }
}

/// A size in bytes, KiB, MiB or GiB.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.;
    let mut unit = 0;
    while size >= 1024. && unit + 1 < UNITS.len() {
        size /= 1024.;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// A name to show: invalid UTF-8 as `�`, and newlines, tabs and other control characters
/// escaped (`\n`), so one name stays on one line and can't fake another.
pub fn display_name(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

/// Folders (and symlinks to them) first, then by name, case aside.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| (!e.opens(), e.display().to_lowercase(), e.name.clone()));
}

/// The listing script's output (see [`LIST`]) as entries, in the order listed. A name with
/// no `stat` line (gone meanwhile) is left out.
pub(crate) fn parse_listing(out: &[u8]) -> Result<Vec<Entry>> {
    let cut = || anyhow!("the folder's listing was cut short");
    let mut rest = out;
    let mut names = Vec::new();
    loop {
        let end = rest.iter().position(|&b| b == 0).ok_or_else(cut)?;
        let name = &rest[..end];
        rest = &rest[end + 1..];
        if name.is_empty() {
            break;
        }
        names.push(name);
    }
    let end = rest.iter().position(|&b| b == 0).ok_or_else(cut)?;
    let (stats, links) = (&rest[..end], &rest[end + 1..]);
    let mut entries = Vec::with_capacity(names.len());
    let mut at = 0;
    for name in &names {
        if let Some((entry, used)) = stat_line(&stats[at..], name) {
            entries.push(entry);
            at += used;
        }
    }
    if entries.is_empty() && !names.is_empty() {
        let first = stats.split(|&b| b == b'\n').next().unwrap_or_default();
        anyhow::bail!("stat's output could not be read: {:?}", String::from_utf8_lossy(first));
    }
    let index: std::collections::HashMap<Vec<u8>, usize> =
        entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect();
    let mut parts = links.split(|&b| b == 0);
    while let (Some(name), Some(flag), Some(target)) = (parts.next(), parts.next(), parts.next()) {
        if let Some(&i) = index.get(name) {
            let e = &mut entries[i];
            e.target = Some(target.strip_suffix(b"\n").unwrap_or(target).to_vec());
            e.target_is_dir = flag == b"d";
        }
    }
    Ok(entries)
}

/// The fields `stat -c '%f %h %u %g %s %Y %t %T %U %G %n'` printed for `name`, at the start
/// of `text`, and the bytes they took (their newline included). None when the line there is
/// another name's.
fn stat_line(text: &[u8], name: &[u8]) -> Option<(Entry, usize)> {
    let mut fields: Vec<&[u8]> = Vec::with_capacity(10);
    let mut at = 0;
    for _ in 0..10 {
        let len = text[at..].iter().position(|&b| b == b' ' || b == b'\n')?;
        if text[at + len] != b' ' {
            return None;
        }
        fields.push(&text[at..at + len]);
        at += len + 1;
    }
    let after = at + name.len();
    if text.get(at..after)? != name || text.get(after) != Some(&b'\n') {
        return None;
    }
    let field = |i: usize| std::str::from_utf8(fields[i]).ok();
    let num = |i: usize| field(i)?.parse::<u64>().ok();
    let hex = |i: usize| u64::from_str_radix(field(i)?, 16).ok();
    let raw = u32::try_from(hex(0)?).ok()?;
    let kind = FileKind::of(raw);
    // Unknown to the container (no /etc/passwd entry): the number says more.
    let named = |i: usize| {
        if fields[i] == b"UNKNOWN" { String::new() } else { String::from_utf8_lossy(fields[i]).into_owned() }
    };
    let device = if matches!(kind, FileKind::CharDevice | FileKind::BlockDevice) { hex(6).zip(hex(7)) } else { None };
    let entry = Entry {
        name: name.to_vec(),
        kind,
        mode: raw & 0o7777,
        links: num(1)?,
        uid: num(2)?,
        gid: num(3)?,
        user: named(8),
        group: named(9),
        size: num(4)?,
        modified: field(5)?.parse().ok()?,
        device,
        target: None,
        target_is_dir: false,
    };
    Some((entry, after + 1))
}

/// What a search printed: NUL-ended paths. Past `cap` bytes the search was cut, and a
/// path cut there is dropped.
fn parse_found(out: &[u8], cap: usize) -> (Vec<Vec<u8>>, bool) {
    let truncated = out.len() >= cap;
    let mut paths: Vec<Vec<u8>> = out.split(|&b| b == 0).map(<[u8]>::to_vec).collect();
    // The piece after the last NUL: empty, or a path cut short.
    paths.pop();
    paths.retain(|p| !p.is_empty());
    (paths, truncated)
}

/// The first bytes of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub bytes: Vec<u8>,
    /// The file goes on past them.
    pub truncated: bool,
}

impl Preview {
    /// At most `max` of `read` (which asked for one byte more, to tell a cut).
    fn of(mut read: Vec<u8>, max: usize) -> Self {
        let truncated = read.len() > max;
        read.truncate(max);
        Self { bytes: read, truncated }
    }

    /// The text, when it reads as UTF-8 without NUL bytes; a character cut at the end of a
    /// truncated preview is dropped. None for a binary file.
    pub fn text(&self) -> Option<&str> {
        if self.bytes.contains(&0) {
            return None;
        }
        match std::str::from_utf8(&self.bytes) {
            Ok(text) => Some(text),
            // Cut in the middle of a character: what comes before it.
            Err(e) if self.truncated && e.error_len().is_none() => {
                std::str::from_utf8(&self.bytes[..e.valid_up_to()]).ok()
            }
            Err(_) => None,
        }
    }

    /// What a binary file looks like, from its first bytes.
    pub fn kind(&self) -> &'static str {
        let b = self.bytes.as_slice();
        const MAGIC: [(&[u8], &str); 13] = [
            (b"\x7fELF", "ELF executable or library"),
            (b"\x1f\x8b", "gzip archive"),
            (b"PK\x03\x04", "zip archive (or jar)"),
            (b"\x89PNG", "PNG image"),
            (b"\xff\xd8\xff", "JPEG image"),
            (b"GIF8", "GIF image"),
            (b"%PDF", "PDF document"),
            (b"BZh", "bzip2 archive"),
            (b"\xfd7zXZ\0", "xz archive"),
            (b"\x28\xb5\x2f\xfd", "zstd archive"),
            (b"SQLite format 3\0", "SQLite database"),
            (b"\0asm", "WebAssembly module"),
            (b"\xca\xfe\xba\xbe", "Java class or Mach-O binary"),
        ];
        if let Some(kind) = MAGIC.iter().find_map(|(magic, kind)| b.starts_with(magic).then_some(*kind)) {
            return kind;
        }
        if b.get(257..262) == Some(b"ustar") {
            return "tar archive";
        }
        "binary data"
    }
}

/// What [`FileSource::open`] found at a path.
pub enum Opened {
    Folder(Vec<Entry>),
    File(Preview),
}

/// A search's results.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub paths: Vec<Vec<u8>>,
    /// More matched than were read.
    pub truncated: bool,
}

/// `path` as an absolute path without `.`, `..`, empty parts or a trailing `/`.
pub fn normalize(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return b"/".to_vec();
    }
    let mut out = Vec::with_capacity(path.len() + 1);
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    out
}

/// `name` in folder `dir`.
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    normalize(&[dir, b"/", name].concat())
}

/// The folder holding `path` (`/` for `/`).
pub fn parent(path: &[u8]) -> Vec<u8> {
    join(path, b"..")
}

/// The last part of `path` (empty for `/`).
pub fn base_name(path: &[u8]) -> &[u8] {
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    trimmed.rsplit(|&b| b == b'/').next().unwrap_or_default()
}

/// Where a symlink in folder `dir` leads: an absolute target as is, a relative one from `dir`.
pub fn resolve(dir: &[u8], target: &[u8]) -> Vec<u8> {
    if target.starts_with(b"/") { normalize(target) } else { join(dir, target) }
}

/// The folders down to `path`, for a breadcrumb: (name to show, path), `/` first.
pub fn crumbs(path: &[u8]) -> Vec<(String, Vec<u8>)> {
    let path = normalize(path);
    let mut out = vec![("/".to_string(), b"/".to_vec())];
    let mut at = Vec::new();
    for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        at.push(b'/');
        at.extend_from_slice(part);
        out.push((display_name(part), at.clone()));
    }
    out
}

/// For `tar -C parent -- member`: the folder to run in and the entry to archive. `/` is
/// archived whole, as `.`.
fn tar_source(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let path = normalize(path);
    if path == b"/" {
        return (b"/".to_vec(), b".".to_vec());
    }
    (parent(&path), base_name(&path).to_vec())
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, FileKind, Preview, base_name, crumbs, display_name, join, normalize, parent, parse_found, parse_listing,
        resolve, sh_arg, sort_entries, tar_source,
    };
    use proptest::prelude::*;

    /// A folder entry for [`listing`]: its name, the `stat` fields before the name, and a
    /// symlink's target and whether it leads to a folder.
    type Row<'a> = (&'a [u8], &'a str, Option<(&'a [u8], bool)>);

    fn row<'a>(name: &'a [u8], fields: &'a str) -> Row<'a> {
        (name, fields, None)
    }

    fn link<'a>(name: &'a [u8], target: &'a [u8], dir: bool) -> Row<'a> {
        (name, "a1ff 1 0 0 7 1700000000 0 0 root root", Some((target, dir)))
    }

    const FILE: &str = "81a4 1 0 0 1 1700000000 0 0 root root";

    /// What the listing script prints for `rows`.
    fn listing(rows: &[Row]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, _, _) in rows {
            out.extend_from_slice(name);
            out.push(0);
        }
        out.push(0);
        for (name, fields, _) in rows {
            out.extend_from_slice(fields.as_bytes());
            out.push(b' ');
            out.extend_from_slice(name);
            out.push(b'\n');
        }
        out.push(0);
        for (name, _, link) in rows {
            if let Some((target, dir)) = link {
                out.extend_from_slice(name);
                out.push(0);
                if *dir {
                    out.push(b'd');
                }
                out.push(0);
                out.extend_from_slice(target);
                out.extend_from_slice(b"\n\0");
            }
        }
        out
    }

    fn named<'a>(entries: &'a [Entry], name: &[u8]) -> &'a Entry {
        entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {}", display_name(name)))
    }

    fn names(entries: &[Entry]) -> Vec<Vec<u8>> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    /// `stat -c '%f %h %u %g %s %Y %t %T %U %G %n'` from GNU coreutils, in a Debian image.
    #[test]
    fn gnu_stat_lines() {
        let out = listing(&[
            row(b"etc", "41ed 52 0 0 4096 1717171717 0 0 root root"),
            row(b"hostname", "81a4 1 0 0 13 1717171718 0 0 root root"),
            link(b"localtime", b"/usr/share/zoneinfo/Etc/UTC", false),
            link(b"lib", b"usr/lib", true),
            row(b"null", "21b6 1 0 0 0 1717171720 1 3 root root"),
            row(b"sda", "61b0 1 0 6 0 1717171720 8 0 root disk"),
            row(b"tmp", "43ff 2 0 0 4096 1717171721 0 0 root root"),
            row(b"su", "89ed 1 0 0 0 1717171722 0 0 root root"),
            row(b"fifo", "11a4 1 1000 1000 0 -5 0 0 UNKNOWN UNKNOWN"),
            row(b"sock", "c1ed 1 0 0 0 1717171722 0 0 root root"),
        ]);
        let entries = parse_listing(&out).unwrap();
        assert_eq!(entries.len(), 10);
        let etc = named(&entries, b"etc");
        assert_eq!((etc.kind, etc.mode_string(), etc.links), (FileKind::Dir, "drwxr-xr-x".to_string(), 52));
        assert_eq!(etc.size_text(), "");
        let host = named(&entries, b"hostname");
        assert_eq!((host.kind, host.size, host.modified), (FileKind::File, 13, 1_717_171_718));
        assert_eq!((host.mode_string(), host.owner()), ("-rw-r--r--".to_string(), "root:root".to_string()));
        let tz = named(&entries, b"localtime");
        assert_eq!((tz.kind, tz.opens()), (FileKind::Symlink, false));
        assert_eq!(tz.target.as_deref(), Some(&b"/usr/share/zoneinfo/Etc/UTC"[..]));
        assert_eq!(tz.mode_string(), "lrwxrwxrwx");
        assert!(named(&entries, b"lib").opens());
        let null = named(&entries, b"null");
        assert_eq!((null.kind, null.device), (FileKind::CharDevice, Some((1, 3))));
        assert_eq!((null.size_text(), null.mode_string()), ("1, 3".to_string(), "crw-rw-rw-".to_string()));
        let sda = named(&entries, b"sda");
        assert_eq!((sda.kind, sda.device), (FileKind::BlockDevice, Some((8, 0))));
        assert_eq!(sda.owner(), "root:disk");
        // Sticky and setuid bits, with execute.
        assert_eq!(named(&entries, b"tmp").mode_string(), "drwxrwxrwt");
        assert_eq!(named(&entries, b"su").mode_string(), "-rwsr-xr-x");
        // Owners the container can't name, by number; a time before 1970.
        let fifo = named(&entries, b"fifo");
        assert_eq!((fifo.kind, fifo.modified), (FileKind::Fifo, -5));
        assert_eq!(fifo.owner(), "1000:1000");
        assert_eq!(named(&entries, b"sock").kind, FileKind::Socket);
    }

    /// The same format from busybox's `stat`, in a busybox or Alpine image: device numbers in
    /// hex, setgid without execute.
    #[test]
    fn busybox_stat_lines() {
        let out = listing(&[
            row(b"bin", "41ed 2 0 0 12288 1700000000 0 0 root root"),
            row(b"tty", "21b6 1 0 5 0 1700000000 5 0 root tty"),
            row(b"loop10", "61b0 1 0 6 0 1700000000 7 a root disk"),
            link(b"sh", b"busybox", false),
            row(b"shared", "45e0 2 0 0 4096 1700000000 0 0 root root"),
        ]);
        let entries = parse_listing(&out).unwrap();
        assert_eq!(named(&entries, b"tty").device, Some((5, 0)));
        assert_eq!(named(&entries, b"loop10").device, Some((7, 10)));
        assert_eq!(named(&entries, b"sh").target.as_deref(), Some(&b"busybox"[..]));
        assert_eq!(named(&entries, b"shared").mode_string(), "drwxr-S---");
    }

    /// Spaces, newlines, a name that looks like a `stat` line, `-n`, non-UTF-8 bytes and a
    /// symlink whose target has a newline all come back whole.
    #[test]
    fn odd_names_come_back_whole() {
        let fake = format!("x\n{FILE} y").into_bytes();
        let out = listing(&[
            row(b"a b", FILE),
            row(b"two\nlines", FILE),
            row(&fake, FILE),
            row(b"y", FILE),
            row(b"-n", FILE),
            row(b"latin1-\xe9", FILE),
            row(b" lead", FILE),
            row(b"trail\n", FILE),
            link(b"link", b"new\nline", false),
        ]);
        let entries = parse_listing(&out).unwrap();
        let want: [&[u8]; 9] =
            [b"a b", b"two\nlines", &fake, b"y", b"-n", b"latin1-\xe9", b" lead", b"trail\n", b"link"];
        assert_eq!(names(&entries), want.map(<[u8]>::to_vec));
        assert_eq!(named(&entries, b"link").target.as_deref(), Some(&b"new\nline"[..]));
        assert_eq!(display_name(b"two\nlines"), "two\\nlines");
        assert_eq!(display_name(b"latin1-\xe9"), "latin1-\u{fffd}");
        assert_eq!(display_name(b"bell\x07"), "bell\\u{7}");
    }

    /// A name gone between the listing and `stat` has no line: it is left out, the others kept.
    #[test]
    fn a_name_gone_meanwhile_is_left_out() {
        let mut out = listing(&[row(b"a", FILE), row(b"c", FILE)]);
        // "b" was listed, then removed before stat ran.
        out.splice(2..2, b"b\0".iter().copied());
        assert_eq!(names(&parse_listing(&out).unwrap()), [b"a".to_vec(), b"c".to_vec()]);
        // An empty folder.
        assert!(parse_listing(b"\0\0").unwrap().is_empty());
        // Cut short, or a stat that printed something else: said, not an empty folder.
        assert!(parse_listing(b"a\0").is_err());
        assert!(parse_listing(b"a\0\0garbage\n\0").is_err());
    }

    #[test]
    fn folders_come_first() {
        let dir = "41ed 2 0 0 0 0 0 0 root root";
        let out = listing(&[
            row(b"b.txt", FILE),
            row(b"Zeta", dir),
            row(b"alpha", dir),
            row(b"A.txt", FILE),
            link(b"lib", b"usr/lib", true),
        ]);
        let mut entries = parse_listing(&out).unwrap();
        sort_entries(&mut entries);
        let shown: Vec<String> = entries.iter().map(Entry::display).collect();
        assert_eq!(shown, ["alpha", "lib", "Zeta", "A.txt", "b.txt"]);
    }

    #[test]
    fn paths() {
        assert_eq!(normalize(b""), b"/");
        assert_eq!(normalize(b"/etc/"), b"/etc");
        assert_eq!(normalize(b"etc//ssl/./certs/.."), b"/etc/ssl");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(join(b"/", b"etc"), b"/etc");
        assert_eq!(join(b"/etc", b"a b"), b"/etc/a b");
        assert_eq!(parent(b"/etc/ssl"), b"/etc");
        assert_eq!(parent(b"/etc"), b"/");
        assert_eq!(parent(b"/"), b"/");
        assert_eq!(base_name(b"/etc/ssl"), b"ssl");
        assert!(base_name(b"/").is_empty());
        assert_eq!(resolve(b"/lib", b"../usr/lib"), b"/usr/lib");
        assert_eq!(resolve(b"/etc", b"/usr/share/zoneinfo/UTC"), b"/usr/share/zoneinfo/UTC");
        assert_eq!(tar_source(b"/var/log"), (b"/var".to_vec(), b"log".to_vec()));
        assert_eq!(tar_source(b"/etc"), (b"/".to_vec(), b"etc".to_vec()));
        assert_eq!(tar_source(b"/"), (b"/".to_vec(), b".".to_vec()));
        let shown: Vec<(String, Vec<u8>)> = crumbs(b"/var/log/a\nb");
        let want: [(&str, &[u8]); 4] =
            [("/", b"/"), ("var", b"/var"), ("log", b"/var/log"), ("a\\nb", b"/var/log/a\nb")];
        assert_eq!(shown, want.map(|(s, p)| (s.to_string(), p.to_vec())));
    }

    /// Under a root, the shell follows symlinks as the container under it sees them: an
    /// absolute target from the root, a relative one from its folder, `..` no higher than
    /// the root, and a loop is said rather than followed for ever.
    #[cfg(unix)]
    #[test]
    fn paths_resolve_under_the_root() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("recouvrance-in-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["run/secrets", "var", "usr/lib"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        symlink("/run", root.join("var/run")).unwrap();
        symlink("../usr/lib", root.join("var/lib")).unwrap();
        symlink("/../../usr", root.join("up")).unwrap();
        symlink("loop", root.join("loop")).unwrap();
        let r = root.to_str().unwrap().to_string();
        let resolve = |path: &str| {
            let script = format!("R={}\n{}in_root \"$1\"", super::super::shell_quote(&r), super::IN_ROOT);
            let out = std::process::Command::new("sh").args(["-c", &script, "sh", path]).output().unwrap();
            out.status.success().then(|| String::from_utf8(out.stdout).unwrap())
        };
        assert_eq!(resolve("/var/run/secrets/token"), Some(format!("{r}/run/secrets/token")));
        assert_eq!(resolve("/var/lib"), Some(format!("{r}/usr/lib")));
        assert_eq!(resolve("/up/lib/../lib"), Some(format!("{r}/usr/lib")));
        assert_eq!(resolve("/var/../../missing"), Some(format!("{r}/missing")));
        assert_eq!(resolve("/"), Some(r.clone()));
        assert_eq!(resolve("/loop"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn previews() {
        let text = Preview::of(b"hello\nworld".to_vec(), 100);
        assert_eq!((text.text(), text.truncated), (Some("hello\nworld"), false));
        // One byte past the cap: cut, and "é" cut in half is dropped.
        let cut = Preview::of("abé".as_bytes().to_vec(), 3);
        assert_eq!((cut.text(), cut.truncated), (Some("ab"), true));
        // Invalid UTF-8 inside, or a NUL: binary.
        assert_eq!(Preview::of(b"ab\xffcd".to_vec(), 100).text(), None);
        let elf = Preview::of(b"\x7fELF\x02\x01\x01\0\0\0".to_vec(), 100);
        assert_eq!((elf.text(), elf.kind()), (None, "ELF executable or library"));
        let mut tar = vec![0u8; 300];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(Preview::of(tar, 1000).kind(), "tar archive");
        assert_eq!(Preview::of(vec![0, 1, 2], 10).kind(), "binary data");
    }

    #[test]
    fn found_paths() {
        let (paths, cut) = parse_found(b"/etc/a\0/etc/b c\0", 1000);
        assert_eq!((paths, cut), (vec![b"/etc/a".to_vec(), b"/etc/b c".to_vec()], false));
        // Cut at the cap: the last path, cut short, is dropped.
        let (paths, cut) = parse_found(b"/etc/a\0/etc/lo", 14);
        assert_eq!((paths, cut), (vec![b"/etc/a".to_vec()], true));
        assert!(parse_found(b"", 10).0.is_empty());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(super::human_size(0), "0 B");
        assert_eq!(super::human_size(1023), "1023 B");
        assert_eq!(super::human_size(1536), "1.5 KiB");
        assert_eq!(super::human_size(5 << 30), "5.0 GiB");
    }

    /// `printf %b`'s escapes, decoded as the shell does.
    fn decode(arg: &str) -> Vec<u8> {
        let b = arg.as_bytes();
        let (mut out, mut i) = (Vec::new(), 0);
        while i < b.len() {
            if b[i] == b'\\' && b.get(i + 1) == Some(&b'0') {
                out.push(u8::from_str_radix(&arg[i + 2..i + 5], 8).unwrap());
                i += 5;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        out
    }

    proptest! {
        /// Any path but one with a NUL gets through `printf %b` unchanged, as plain ASCII.
        #[test]
        fn any_path_gets_through_printf(path in prop::collection::vec(1u8..=255, 0..64)) {
            let arg = sh_arg(&path);
            prop_assert!(arg.is_ascii());
            prop_assert_eq!(decode(&arg), path);
        }

        /// Any names, however odd, come back from a listing whole and in order.
        #[test]
        fn any_names_list_back(set in prop::collection::btree_set(prop::collection::vec(1u8..=255, 1..12), 0..8)) {
            let listed: Vec<Vec<u8>> = set.into_iter().filter(|n| !n.contains(&b'/')).collect();
            let rows: Vec<Row> = listed.iter().map(|n| row(n, FILE)).collect();
            let entries = parse_listing(&listing(&rows)).unwrap();
            prop_assert_eq!(names(&entries), listed);
        }
    }
}
