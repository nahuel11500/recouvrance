//! The timeline on disk: one file per context, `<data dir>/timeline/<context>-<hash>.jsonl`,
//! a header line naming the context, then one record a line, oldest first. New records are
//! appended in batches by one writer thread; the file is rewritten whole (atomically) only
//! when records fall out of the size or age bound, which leaves room for many appends.
//!
//! A file that doesn't read (cut short by a crash, edited, another format) is kept aside
//! as `<name>.bad-<unix time>` and recording starts over: never lose it silently, never
//! stop recording because of it.

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::Record;

/// The file format; another one is kept aside, not read.
const FORMAT: u32 = 1;

/// The first line of a file: which context it records (several names can share a file
/// name once made safe for the file system).
#[derive(Serialize, Deserialize)]
struct Header {
    timeline: u32,
    context: String,
}

/// How much of a context's timeline is kept.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The records' lines, in bytes (the file is this and its header).
    pub max_bytes: usize,
    /// Records older than this many seconds are dropped.
    pub max_age_secs: i64,
}

impl Limits {
    pub fn from_settings() -> Self {
        crate::settings::with(|s| Self {
            max_bytes: s.timeline.max_mb() as usize * 1024 * 1024,
            max_age_secs: s.timeline.days() as i64 * 86_400,
        })
    }
}

/// The records of one context, oldest first, as the file has them.
#[derive(Default)]
pub struct Log {
    pub records: VecDeque<Arc<Record>>,
    /// Each record's line in the file, in bytes (its newline included).
    sizes: VecDeque<usize>,
    bytes: usize,
}

impl Log {
    /// Add a record whose line takes `size` bytes, dropping older ones to stay within
    /// `limits`. A record already past the age bound is not added.
    pub fn push(&mut self, r: Arc<Record>, size: usize, limits: Limits, now: jiff::Timestamp) -> Pushed {
        if r.time.as_second() < cutoff(limits, now) {
            return Pushed::TooOld;
        }
        self.records.push_back(r);
        self.sizes.push_back(size);
        self.bytes += size;
        if self.trim(limits, now) { Pushed::Trimmed } else { Pushed::Added }
    }

    /// Drop the records past the age bound at the front, and, once over the size bound, the
    /// oldest ones down to 3/4 of it: the next records are appended again for a while
    /// instead of rewriting the file each time.
    fn trim(&mut self, limits: Limits, now: jiff::Timestamp) -> bool {
        let cutoff = cutoff(limits, now);
        let mut dropped = false;
        while self.records.front().is_some_and(|r| r.time.as_second() < cutoff) {
            self.pop();
            dropped = true;
        }
        if self.bytes > limits.max_bytes {
            let target = limits.max_bytes / 4 * 3;
            while self.bytes > target && !self.records.is_empty() {
                self.pop();
            }
            dropped = true;
        }
        dropped
    }

    fn pop(&mut self) {
        if let Some(size) = self.sizes.pop_front() {
            self.bytes -= size;
            self.records.pop_front();
        }
    }

    /// The records' lines, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// What [`Log::push`] did, and so what the file needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// Past the age bound: not added, nothing to write.
    TooOld,
    /// Added: append its line.
    Added,
    /// Added, and older records dropped: rewrite the file.
    Trimmed,
}

/// Records older than this (unix seconds) are dropped.
fn cutoff(limits: Limits, now: jiff::Timestamp) -> i64 {
    now.as_second().saturating_sub(limits.max_age_secs)
}

/// A record's line in the file, newline included.
pub fn line(r: &Record) -> Option<String> {
    serde_json::to_string(r).ok().map(|mut s| {
        s.push('\n');
        s
    })
}

fn header_line(context: &str) -> String {
    let mut s = serde_json::to_string(&Header { timeline: FORMAT, context: context.to_string() }).unwrap_or_default();
    s.push('\n');
    s
}

/// The file name for a context: readable, and a hash of the name so two contexts that
/// read the same once made safe (`a/b`, `a:b`) never share a file.
pub fn file_name(context: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let readable: String = context
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .take(80)
        .collect();
    let digest = Sha256::digest(context.as_bytes());
    let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{readable}-{hash}.jsonl")
}

/// What a file held.
#[derive(Default)]
pub struct Loaded {
    pub log: Log,
    /// Why the file was kept aside, and where it went.
    pub set_aside: Option<String>,
    /// Records were dropped (too old, over the size): the file should be rewritten.
    pub rewrite: bool,
}

/// Read a context's file. Missing: nothing recorded yet. Not readable as a whole (cut
/// short, a line that doesn't parse, another context's or format's header): kept aside
/// and empty, so recording starts over.
pub fn load(path: &Path, context: &str, limits: Limits, now: jiff::Timestamp) -> Loaded {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Loaded::default(),
        Err(e) => return set_aside(path, &e.to_string()),
    };
    match parse(&bytes, context) {
        Ok(records) => {
            let mut log = Log::default();
            let read = records.len();
            let mut dropped = false;
            for (r, size) in records {
                dropped |= log.push(Arc::new(r), size, limits, now) == Pushed::Trimmed;
            }
            Loaded { rewrite: dropped || log.records.len() != read, log, set_aside: None }
        }
        Err(why) => set_aside(path, &why),
    }
}

fn set_aside(path: &Path, why: &str) -> Loaded {
    let msg = format!("{}: {why}; recording starts over ({})", path.display(), crate::settings::keep_aside(path));
    eprintln!("{msg}");
    Loaded { set_aside: Some(msg), ..Loaded::default() }
}

/// A file's records with the size of each line, or why it doesn't read.
fn parse(bytes: &[u8], context: &str) -> Result<Vec<(Record, usize)>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    // Every line ends with a newline: one without was cut short.
    let Some(text) = text.strip_suffix('\n') else {
        return Err("cut short (the last line is incomplete)".into());
    };
    let mut lines = text.split('\n');
    let header: Header =
        serde_json::from_str(lines.next().unwrap_or_default()).map_err(|e| format!("no header: {e}"))?;
    if header.timeline != FORMAT {
        return Err(format!("format {} (this version reads {FORMAT})", header.timeline));
    }
    if header.context != context {
        return Err(format!("records context {:?}", header.context));
    }
    lines
        .enumerate()
        .map(|(i, l)| {
            let r: Record = serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 2))?;
            Ok((r, l.len() + 1))
        })
        .collect()
}

/// Add lines at the end of a context's file (a new file starts with its header).
pub fn append(path: &Path, context: &str, lines: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::private_files::create_dir(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
    let mut out = String::with_capacity(lines.len() + 64);
    if f.metadata()?.len() == 0 {
        out.push_str(&header_line(context));
    }
    out.push_str(lines);
    f.write_all(out.as_bytes())
}

/// Replace a context's file with these records (atomically: a crash leaves the old file
/// or the new one, never half of one).
pub fn rewrite<'a>(path: &Path, context: &str, records: impl IntoIterator<Item = &'a Arc<Record>>) -> io::Result<()> {
    let mut out = header_line(context);
    for r in records {
        out.push_str(&line(r).unwrap_or_default());
    }
    crate::private_files::write_atomic(path, out.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::What;
    use proptest::prelude::*;

    fn record(i: usize, age_secs: i64, summary_len: usize, now: jiff::Timestamp) -> Record {
        Record {
            time: now - jiff::SignedDuration::from_secs(age_secs),
            resource: "configmaps".into(),
            kind: "ConfigMap".into(),
            namespace: Some("default".into()),
            name: format!("cm-{i}"),
            uid: format!("uid-{i}"),
            what: What::Updated,
            summary: "x".repeat(summary_len),
            before: Some(serde_json::value::to_raw_value(&serde_json::json!({"data": {"k": i}})).unwrap()),
            after: None,
        }
    }

    fn text(records: impl IntoIterator<Item = impl std::borrow::Borrow<Record>>) -> Vec<String> {
        records.into_iter().map(|r| line(r.borrow()).unwrap()).collect()
    }

    /// Push records as the recorder does, writing the file the same way.
    fn push_all(path: &Path, log: &mut Log, records: Vec<Record>, limits: Limits, now: jiff::Timestamp) {
        for r in records {
            let l = line(&r).unwrap();
            match log.push(Arc::new(r), l.len(), limits, now) {
                Pushed::Trimmed => rewrite(path, "ctx", &log.records).unwrap(),
                Pushed::Added => append(path, "ctx", &l).unwrap(),
                Pushed::TooOld => {}
            }
        }
    }

    const BIG: Limits = Limits { max_bytes: 1 << 30, max_age_secs: 1 << 40 };

    #[test]
    fn file_names_never_collide() {
        assert_ne!(file_name("a/b"), file_name("a:b"));
        assert!(file_name("arn:aws:eks:eu-west-1:1:cluster/prod").starts_with("arn_aws_eks_eu-west-1_1_cluster_prod-"));
        assert!(!file_name("../../etc").contains('/'));
    }

    #[test]
    fn another_context_s_file_is_kept_aside() {
        let dir = crate::mcp::test_dir();
        let path = dir.join("t.jsonl");
        let now = jiff::Timestamp::now();
        append(&path, "other", &line(&record(0, 1, 3, now)).unwrap()).unwrap();
        let loaded = load(&path, "ctx", BIG, now);
        assert!(loaded.set_aside.is_some() && loaded.log.records.is_empty());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// What is written reads back the same, in the same order.
        #[test]
        fn records_round_trip(sizes in prop::collection::vec(0usize..300, 0..40)) {
            let dir = crate::mcp::test_dir();
            let path = dir.join("t.jsonl");
            let now = jiff::Timestamp::now();
            let records: Vec<Record> = sizes.iter().enumerate().map(|(i, &n)| record(i, i as i64, n, now)).collect();
            let mut log = Log::default();
            push_all(&path, &mut log, records.clone(), BIG, now);
            let loaded = load(&path, "ctx", BIG, now);
            prop_assert!(loaded.set_aside.is_none());
            prop_assert!(!loaded.rewrite);
            prop_assert_eq!(text(loaded.log.records.iter().map(|r| &**r)), text(&records));
            prop_assert_eq!(loaded.log.bytes(), log.bytes());
            std::fs::remove_dir_all(dir).unwrap();
        }

        /// The file never holds more than the size bound (and its header), keeps the newest
        /// records, and reads back as the recorder holds them.
        #[test]
        fn the_size_bound_holds(sizes in prop::collection::vec(0usize..400, 1..80), max in 200usize..4000) {
            let dir = crate::mcp::test_dir();
            let path = dir.join("t.jsonl");
            let now = jiff::Timestamp::now();
            let limits = Limits { max_bytes: max, max_age_secs: 1 << 40 };
            let records: Vec<Record> = sizes.iter().enumerate().map(|(i, &n)| record(i, 0, n, now)).collect();
            let mut log = Log::default();
            for r in records.clone() {
                push_all(&path, &mut log, vec![r], limits, now);
                prop_assert!(log.bytes() <= max);
                let on_disk = std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
                prop_assert!(on_disk <= max + header_line("ctx").len());
            }
            // The newest are kept, in order.
            let kept = text(log.records.iter().map(|r| &**r));
            let all = text(&records);
            prop_assert_eq!(&all[all.len() - kept.len()..], &kept[..]);
            let loaded = load(&path, "ctx", limits, now);
            prop_assert_eq!(text(loaded.log.records.iter().map(|r| &**r)), kept);
            std::fs::remove_dir_all(dir).unwrap();
        }

        /// Records past the age bound are not kept, on push or when read back.
        #[test]
        fn the_age_bound_holds(ages in prop::collection::vec(0i64..1000, 0..40), max_age in 1i64..1000, later in 0i64..1000) {
            let dir = crate::mcp::test_dir();
            let path = dir.join("t.jsonl");
            let now = jiff::Timestamp::now();
            let mut sorted = ages.clone();
            sorted.sort_unstable_by(|a, b| b.cmp(a));
            let records: Vec<Record> = sorted.iter().enumerate().map(|(i, &a)| record(i, a, 5, now)).collect();
            let limits = Limits { max_bytes: 1 << 30, max_age_secs: max_age };
            let mut log = Log::default();
            push_all(&path, &mut log, records, limits, now);
            for r in &log.records {
                prop_assert!(r.time.as_second() >= now.as_second() - max_age);
            }
            // Read back later: what has aged out since is dropped too, and the file rewritten.
            let then = now + jiff::SignedDuration::from_secs(later);
            let loaded = load(&path, "ctx", limits, then);
            for r in &loaded.log.records {
                prop_assert!(r.time.as_second() >= then.as_second() - max_age);
            }
            let aged = log.records.iter().any(|r| r.time.as_second() < then.as_second() - max_age);
            prop_assert_eq!(loaded.rewrite, aged);
            std::fs::remove_dir_all(dir).unwrap();
        }

        /// A file cut short or damaged anywhere is kept aside and recording starts over; one
        /// cut at a line's end reads as the records before the cut.
        #[test]
        fn a_damaged_file_is_kept_aside(n in 1usize..12, cut in any::<prop::sample::Index>(), damage in any::<bool>()) {
            let dir = crate::mcp::test_dir();
            let path = dir.join("t.jsonl");
            let now = jiff::Timestamp::now();
            let records: Vec<Record> = (0..n).map(|i| record(i, 0, 10, now)).collect();
            push_all(&path, &mut Log::default(), records.clone(), BIG, now);
            let mut bytes = std::fs::read(&path).unwrap();
            let at = cut.index(bytes.len());
            if damage {
                // A NUL byte is never valid JSON, in a string or out of one.
                bytes.insert(at, 0);
            } else {
                bytes.truncate(at);
            }
            std::fs::write(&path, &bytes).unwrap();
            let loaded = load(&path, "ctx", BIG, now);
            let read = text(loaded.log.records.iter().map(|r| &**r));
            if loaded.set_aside.is_some() {
                prop_assert!(read.is_empty());
                prop_assert!(!path.exists(), "kept aside");
                let aside = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().contains(".bad-")).count();
                prop_assert_eq!(aside, 1);
                // Recording starts over in a new file.
                let next = record(99, 0, 3, now);
                append(&path, "ctx", &line(&next).unwrap()).unwrap();
                let again = load(&path, "ctx", BIG, now);
                prop_assert!(again.set_aside.is_none());
                prop_assert_eq!(text(again.log.records.iter().map(|r| &**r)), text([next]));
            } else {
                prop_assert!(!damage);
                let all = text(&records);
                prop_assert_eq!(&all[..read.len()], &read[..]);
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
