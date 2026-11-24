//! What's new and updates. The release notes are this binary's own `CHANGELOG.md`, embedded
//! when it was built, so showing them fetches nothing. How to update follows from how this
//! copy was installed, read from where it runs. The one request, for the latest release on
//! GitHub, is sent only when Check for updates is clicked.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as _, Result};

/// Every version's release notes, as release-plz writes them.
const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// The version running.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every release, with its files to download.
pub const RELEASES: &str = "https://github.com/nahuel11500/recouvrance/releases";

/// GitHub's latest release: the newest one published, never a draft or a pre-release.
const LATEST_RELEASE: &str = "https://api.github.com/repos/nahuel11500/recouvrance/releases/latest";

/// Where the changelog's pull request numbers lead.
const PULLS: &str = "https://github.com/nahuel11500/recouvrance/pull";

/// The command that builds the newest code from source.
const FROM_SOURCE: &str = "cargo install --locked --git https://github.com/nahuel11500/recouvrance";

/// What dpkg keeps for the installed .deb.
const DPKG_RECORD: &str = "/var/lib/dpkg/info/recouvrance.list";

/// One release's notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    pub version: String,
    /// The day it was released (`2026-11-19`); empty when the changelog has none.
    pub date: String,
    /// Its section below the heading, in Markdown.
    pub body: String,
}

/// A version as numbers, so that `0.10.0` comes after `0.9.0`. A leading `v` and a
/// pre-release or build suffix (`-rc.1`, `+abc`) are left out. None when it isn't
/// `major.minor.patch`.
pub fn version_key(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim();
    let core = v.strip_prefix('v').unwrap_or(v).split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let key = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(key)
}

/// Whether `a` is a newer version than `b`; false when either isn't a version.
pub fn is_newer(a: &str, b: &str) -> bool {
    version_key(a).zip(version_key(b)).is_some_and(|(a, b)| a > b)
}

/// The releases of a changelog laid out as release-plz does (`## [0.9.0](link) - 2026-11-19`,
/// then the notes), newest first as it lists them. `[Unreleased]` is not one.
fn releases(changelog: &str) -> Vec<Notes> {
    let mut out: Vec<Notes> = Vec::new();
    // Whether the lines read belong to the last release found (not to `[Unreleased]`).
    let mut in_release = false;
    for line in changelog.lines() {
        if let Some(heading) = line.strip_prefix("## [") {
            let (version, rest) = heading.split_once(']').unwrap_or((heading, ""));
            in_release = version_key(version).is_some();
            if in_release {
                let date = rest.rsplit_once(" - ").map_or("", |(_, d)| d.trim());
                out.push(Notes { version: version.to_string(), date: date.to_string(), body: String::new() });
            }
        } else if in_release && let Some(release) = out.last_mut() {
            release.body.push_str(line);
            release.body.push('\n');
        }
    }
    for release in &mut out {
        release.body = release.body.trim().to_string();
    }
    out
}

/// The notes What's new shows: those of every release newer than `last_seen` up to `running`,
/// newest first, or `running`'s alone when the version last seen is unknown (a settings file
/// from before What's new). None for the same version or an older one (after a downgrade),
/// nor when the changelog has no notes for `running`.
fn notes_since(changelog: &str, last_seen: Option<&str>, running: &str) -> Vec<Notes> {
    let Some(now) = version_key(running) else { return Vec::new() };
    let seen = last_seen.and_then(version_key);
    if seen.is_some_and(|s| s >= now) {
        return Vec::new();
    }
    let all = releases(changelog);
    if !all.iter().any(|n| version_key(&n.version) == Some(now)) {
        return Vec::new();
    }
    all.into_iter()
        .filter(|n| version_key(&n.version).is_some_and(|k| k <= now && seen.map_or(k == now, |s| k > s)))
        .collect()
}

/// At start: the notes to show, and whether `running` becomes the version seen. It does when
/// it is newer than the one seen, or that one is unknown; never after a downgrade, so the
/// newer version's notes don't show again when it comes back.
fn on_start(changelog: &str, last_seen: Option<&str>, running: &str) -> (Vec<Notes>, bool) {
    let seen = version_key(running).is_some_and(|now| last_seen.and_then(version_key).is_none_or(|s| now > s));
    (notes_since(changelog, last_seen, running), seen)
}

/// The notes to show at start, once: the version running becomes the one seen. None while
/// `settings.json` can't be read, since that couldn't be remembered: they would show at every
/// start.
pub fn take_due() -> Vec<Notes> {
    if crate::settings::load_error().is_some() {
        return Vec::new();
    }
    let last_seen = crate::settings::with(|s| s.last_seen_version.clone());
    let (notes, seen) = on_start(CHANGELOG, last_seen.as_deref(), VERSION);
    if seen {
        crate::settings::update(|s| s.last_seen_version = Some(VERSION.to_string()));
    }
    notes
}

/// The notes of the version running, for About. None for a build the changelog has no notes
/// for.
pub fn own_notes() -> Option<&'static Notes> {
    static OWN: LazyLock<Option<Notes>> =
        LazyLock::new(|| releases(CHANGELOG).into_iter().find(|n| n.version == VERSION));
    Option::as_ref(&OWN)
}

/// What What's new shows after an update from `last_seen` (`RECOUVRANCE_ACT=whats-new:0.7.0`).
#[cfg(feature = "dev")]
pub fn notes_after(last_seen: &str) -> Vec<Notes> {
    notes_since(CHANGELOG, Some(last_seen), VERSION)
}

/// The notes as one Markdown text, with pull request numbers (`(#826)`) linked to the pull
/// requests. With several releases, each goes under its version and date.
pub fn markdown(notes: &[Notes]) -> String {
    let several = notes.len() > 1;
    let mut out = String::new();
    for release in notes {
        if several {
            out.push_str("## ");
            out.push_str(&release.version);
            if !release.date.is_empty() {
                out.push_str(" · ");
                out.push_str(&release.date);
            }
            out.push_str("\n\n");
        }
        out.push_str(&linked(&release.body));
        out.push_str("\n\n");
    }
    out.truncate(out.trim_end().len());
    out
}

/// `(#826)` as a link to pull request 826.
fn linked(text: &str) -> String {
    static NUMBER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\(#(\d+)\)").expect("a valid pattern"));
    NUMBER.replace_all(text, format!("([#$1]({PULLS}/$1))").as_str()).into_owned()
}

/// How this copy was installed, and so how to update it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The install script, which put `recouvrance-update` next to the binary.
    Script,
    /// The .deb: in /usr/bin, and known to dpkg.
    Deb,
    /// An `AppImage`.
    AppImage,
    /// A tarball, or built from source.
    Other,
}

/// How the running copy was installed, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub method: Method,
    /// The file an update replaces: the `AppImage`, else the binary. None when unknown.
    pub path: Option<PathBuf>,
}

/// How to update: how this copy was installed, what to do, and the command for it.
pub struct Advice {
    pub installed: &'static str,
    pub how: String,
    pub command: Option<String>,
}

impl Install {
    /// How to update this copy. `latest`, the newest version once checked, goes in the file
    /// names that hold one.
    pub fn advice(&self, latest: Option<&str>) -> Advice {
        let deb = format!("recouvrance_{}_amd64.deb", latest.unwrap_or("<version>"));
        match self.method {
            Method::Script => Advice {
                installed: "Installed with the install script",
                how: "To update, run this in a terminal:".into(),
                command: Some("recouvrance-update".into()),
            },
            Method::Deb => Advice {
                installed: "Installed from the .deb",
                how: format!("To update, download {deb} from the releases page and install it:"),
                command: Some(format!("sudo apt install ./{deb}")),
            },
            Method::AppImage => Advice {
                installed: "Running as an AppImage",
                how: "To update, download recouvrance-x86_64.AppImage from the releases page, put it in place \
                      of this one and make it executable:"
                    .into(),
                command: self
                    .path
                    .as_deref()
                    .map(|p| format!("chmod +x {}", crate::details::shell_word(&p.to_string_lossy()))),
            },
            Method::Other => Advice {
                installed: "Installed from a tarball, or built from source",
                how: "To update, put the recouvrance binary of the new tarball in place of this one, or build \
                      the new version from source:"
                    .into(),
                command: Some(FROM_SOURCE.into()),
            },
        }
    }
}

/// The updater the install script puts next to the binary (`.exe` on Windows).
const UPDATER: &str = if cfg!(windows) { "recouvrance-update.exe" } else { "recouvrance-update" };

/// How the binary at `exe` was installed. `appimage` and `appdir` are what an `AppImage`'s
/// runtime sets (`APPIMAGE`, `APPDIR`), and `exists` says whether a file is there.
fn detect(
    exe: Option<&Path>,
    appimage: Option<PathBuf>,
    appdir: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Install {
    // A program started from an AppImage (a terminal) inherits its variables: only a binary
    // inside that image's own folder runs from it.
    if let Some(image) = appimage
        && exe.zip(appdir).is_none_or(|(exe, dir)| exe.starts_with(dir))
    {
        return Install { method: Method::AppImage, path: Some(image) };
    }
    let method = match exe {
        Some(exe) if exe.parent().is_some_and(|dir| exists(&dir.join(UPDATER))) => Method::Script,
        // A binary copied to /usr/bin by hand, or another distribution's package, is not the .deb.
        Some(exe) if exe.starts_with("/usr/bin") && exists(Path::new(DPKG_RECORD)) => Method::Deb,
        _ => Method::Other,
    };
    Install { method, path: exe.map(Path::to_path_buf) }
}

/// How the running copy was installed, looked up once.
pub fn install() -> &'static Install {
    static INSTALL: LazyLock<Install> = LazyLock::new(|| {
        let exe = std::env::current_exe().ok();
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        detect(exe.as_deref(), var("APPIMAGE"), var("APPDIR").as_deref(), Path::exists)
    });
    &INSTALL
}

/// The newest version on GitHub: one unauthenticated request, sent only when the user asks.
pub async fn latest_release() -> Result<String> {
    let answer = crate::assistant::get_url(LATEST_RELEASE.into()).await?;
    latest_version(&answer)
}

/// The version of GitHub's latest release, from its answer (`"tag_name": "v0.10.0"`), as
/// numbers only: nothing else of the answer is shown or opened.
fn latest_version(answer: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = serde_json::from_str(answer).context("GitHub's answer is not a release")?;
    let (major, minor, patch) =
        version_key(&release.tag_name).context("GitHub's latest release has no version in its tag")?;
    Ok(format!("{major}.{minor}.{patch}"))
}

/// The page of a release, made from its version alone.
pub fn release_page(version: &str) -> String {
    format!("{RELEASES}/tag/v{version}")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        Install, Method, Notes, detect, is_newer, latest_version, linked, markdown, on_start, releases, version_key,
    };

    const LOG: &str = "# Changelog

## [Unreleased]

## [0.10.0](https://github.com/nahuel11500/recouvrance/compare/v0.9.0...v0.10.0) - 2026-11-25

### Added

- *(app)* What's new after an update (#900)

## [0.9.0](https://github.com/nahuel11500/recouvrance/compare/v0.8.0...v0.9.0) - 2026-11-19

### Fixed

- *(sidebar)* A fix (#826)

## [0.8.0](https://github.com/nahuel11500/recouvrance/compare/v0.7.1...v0.8.0) - 2026-11-18

- Older

## [0.0.1]

- The first
";

    fn versions(notes: &[Notes]) -> Vec<&str> {
        notes.iter().map(|n| n.version.as_str()).collect()
    }

    /// Whether a path is one of `files`.
    fn among<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |p: &Path| files.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.9.0", "0.9.0"));
        assert_eq!(version_key("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_key("0.10.0-rc.1"), Some((0, 10, 0)));
        for bad in ["Unreleased", "0.9", "0.9.0.1", "", "a.b.c", "0..1", "-1.0.0"] {
            assert_eq!(version_key(bad), None, "{bad}");
        }
        assert!(!is_newer("soon", "0.9.0") && !is_newer("0.9.0", "soon"));
    }

    #[test]
    fn the_changelog_reads_as_releases() {
        let all = releases(LOG);
        assert_eq!(versions(&all), ["0.10.0", "0.9.0", "0.8.0", "0.0.1"]);
        assert_eq!(all[0].date, "2026-11-25");
        assert_eq!(all[0].body, "### Added\n\n- *(app)* What's new after an update (#900)");
        assert_eq!((all[3].date.as_str(), all[3].body.as_str()), ("", "- The first"));
    }

    /// The binary's own notes are in it: release-plz bumps the version and writes its notes
    /// in the same pull request. Every release heading of the file reads.
    #[test]
    fn this_version_has_its_notes() {
        let own = super::own_notes().expect("CHANGELOG.md has notes for the version in Cargo.toml");
        assert!(!own.body.is_empty() && !own.date.is_empty());
        let headings =
            super::CHANGELOG.lines().filter(|l| l.starts_with("## [") && !l.starts_with("## [Unreleased]")).count();
        assert_eq!(releases(super::CHANGELOG).len(), headings);
    }

    #[test]
    fn whats_new_shows_the_releases_since_the_one_seen_once() {
        // An update from 0.8.0: both releases since, newest first; 0.10.0 is then the one seen.
        let (notes, seen) = on_start(LOG, Some("0.8.0"), "0.10.0");
        assert_eq!((versions(&notes), seen), (vec!["0.10.0", "0.9.0"], true));
        // The next start shows nothing.
        assert_eq!(on_start(LOG, Some("0.10.0"), "0.10.0"), (Vec::new(), false));
        // A settings file from before What's new: this version's notes alone.
        let (notes, seen) = on_start(LOG, None, "0.10.0");
        assert_eq!((versions(&notes), seen), (vec!["0.10.0"], true));
        // So does a version seen that doesn't read.
        assert_eq!(versions(&on_start(LOG, Some("soon"), "0.10.0").0), ["0.10.0"]);
        // After a downgrade, nothing, and the newer version stays the one seen.
        assert_eq!(on_start(LOG, Some("0.10.0"), "0.9.0"), (Vec::new(), false));
        // A build the changelog has no notes for shows nothing, and counts as seen.
        assert_eq!(on_start(LOG, Some("0.9.0"), "0.11.0"), (Vec::new(), true));
    }

    #[test]
    fn pull_request_numbers_become_links() {
        assert_eq!(linked("- A fix (#826)"), "- A fix ([#826](https://github.com/nahuel11500/recouvrance/pull/826))");
        assert_eq!(linked("(#abc) #826 (826)"), "(#abc) #826 (826)");
        let all = releases(LOG);
        // Several releases: each under its version and date.
        let md = markdown(&all[..2]);
        assert!(md.starts_with("## 0.10.0 · 2026-11-25\n\n### Added\n"), "{md}");
        assert!(md.contains("\n\n## 0.9.0 · 2026-11-19\n\n### Fixed\n"), "{md}");
        assert!(md.ends_with("pull/826))"), "{md}");
        // One: no heading of its own, the dialog's title has the version.
        assert!(markdown(&all[1..2]).starts_with("### Fixed"));
    }

    #[test]
    fn the_install_method_follows_where_the_binary_is() {
        let cargo_bin = Path::new("/home/u/.cargo/bin/recouvrance");
        let script = format!("/home/u/.cargo/bin/{}", super::UPDATER);
        let updater = [script.as_str()];
        assert_eq!(detect(Some(cargo_bin), None, None, among(&updater)).method, Method::Script);
        // `cargo install` puts it in the same folder, without the updater.
        let built = detect(Some(cargo_bin), None, None, among(&[]));
        assert_eq!(built, Install { method: Method::Other, path: Some(cargo_bin.into()) });
        // The .deb: /usr/bin, and dpkg has it. A binary copied there by hand is not one.
        let usr_bin = Path::new("/usr/bin/recouvrance");
        assert_eq!(detect(Some(usr_bin), None, None, among(&[super::DPKG_RECORD])).method, Method::Deb);
        assert_eq!(detect(Some(usr_bin), None, None, among(&[])).method, Method::Other);
        // An AppImage: the image is the file to replace.
        let image = PathBuf::from("/home/u/Apps/recouvrance-x86_64.AppImage");
        let mounted = Path::new("/tmp/.mount_RecouvX/usr/bin/recouvrance");
        let found = detect(Some(mounted), Some(image.clone()), Some(Path::new("/tmp/.mount_RecouvX")), among(&[]));
        assert_eq!(found, Install { method: Method::AppImage, path: Some(image) });
        // Started from another AppImage's terminal, whose variables it inherits: not one.
        let other = Some(PathBuf::from("/home/u/Apps/Terminal.AppImage"));
        let inherited = detect(Some(cargo_bin), other, Some(Path::new("/tmp/.mount_TermY")), among(&updater));
        assert_eq!(inherited.method, Method::Script);
        // Where it runs is unknown: a tarball or source build, no path.
        assert_eq!(detect(None, None, None, among(&[])), Install { method: Method::Other, path: None });
    }

    #[test]
    fn the_advice_fits_the_install() {
        let install = |method, path: &str| Install { method, path: Some(PathBuf::from(path)) };
        let script = install(Method::Script, "/home/u/.cargo/bin/recouvrance").advice(None);
        assert_eq!(script.command.as_deref(), Some("recouvrance-update"));
        // The .deb's name has the latest version once checked.
        let deb = install(Method::Deb, "/usr/bin/recouvrance");
        let checked = deb.advice(Some("0.10.0"));
        assert_eq!(checked.command.as_deref(), Some("sudo apt install ./recouvrance_0.10.0_amd64.deb"));
        assert!(checked.how.contains("recouvrance_0.10.0_amd64.deb"));
        assert!(deb.advice(None).how.contains("recouvrance_<version>_amd64.deb"));
        // The AppImage's own path, quoted for a shell.
        let image = install(Method::AppImage, "/home/u/My Apps/recouvrance-x86_64.AppImage").advice(None);
        assert_eq!(image.command.as_deref(), Some("chmod +x '/home/u/My Apps/recouvrance-x86_64.AppImage'"));
        let other = install(Method::Other, "/opt/recouvrance/recouvrance").advice(None);
        assert!(other.command.is_some_and(|c| c.starts_with("cargo install --locked --git ")));
        // Only the install script's copy is told to run recouvrance-update.
        for method in [Method::Deb, Method::AppImage, Method::Other] {
            let a = install(method, "/x/recouvrance").advice(None);
            assert!(!a.how.contains("recouvrance-update"), "{method:?}");
            assert!(!a.command.unwrap_or_default().contains("recouvrance-update"), "{method:?}");
        }
    }

    #[test]
    fn the_latest_release_is_read_from_its_tag() {
        let answer =
            r#"{"url": "x", "tag_name": "v0.10.0", "name": "v0.10.0", "draft": false, "assets": [{"name": "a"}]}"#;
        assert_eq!(latest_version(answer).unwrap(), "0.10.0");
        // Only the numbers are kept.
        assert_eq!(latest_version(r#"{"tag_name": "v1.2.3-beta+build"}"#).unwrap(), "1.2.3");
        assert!(latest_version(r#"{"tag_name": "nightly"}"#).is_err());
        assert!(latest_version(r#"{"message": "API rate limit exceeded"}"#).is_err());
        assert!(latest_version("<html>").is_err());
        assert_eq!(super::release_page("0.10.0"), "https://github.com/nahuel11500/recouvrance/releases/tag/v0.10.0");
    }
}

/// What's new over runs of any versions, updates and downgrades in any order.
#[cfg(test)]
mod properties {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::{on_start, version_key};

    type Key = (u64, u64, u64);

    fn version() -> impl Strategy<Value = Key> {
        (0..2u64, 0..12u64, 0..3u64)
    }

    fn name((major, minor, patch): Key) -> String {
        format!("{major}.{minor}.{patch}")
    }

    proptest! {
        /// Each start shows the releases newer than the version last seen, up to the one
        /// running (that one alone while the version seen is unknown): so no release twice,
        /// none newer than the version running, and nothing after a downgrade.
        #[test]
        fn each_release_shows_once_in_its_turn(
            released in prop::collection::btree_set(version(), 1..10),
            runs in prop::collection::vec(version(), 1..10),
            known in any::<bool>(),
        ) {
            let mut log = String::from("# Changelog\n\n## [Unreleased]\n\n");
            for release in released.iter().rev() {
                log.push_str(&format!("## [{}](https://example.com) - 2026-01-01\n\n- notes\n\n", name(*release)));
            }
            // A settings file from before What's new, or one that has a version.
            let mut seen: Option<Key> = known.then_some((0, 0, 0));
            let mut shown = BTreeSet::new();
            for run in runs {
                let (notes, record) = on_start(&log, seen.map(name).as_deref(), &name(run));
                let got: Vec<Key> = notes.iter().filter_map(|n| version_key(&n.version)).collect();
                let newer = seen.is_none_or(|s| run > s);
                let want: Vec<Key> = if newer && released.contains(&run) {
                    released.iter().rev().copied().filter(|k| *k <= run && seen.map_or(*k == run, |s| *k > s)).collect()
                } else {
                    Vec::new()
                };
                prop_assert_eq!(&got, &want);
                for release in got {
                    prop_assert!(shown.insert(release), "{:?} shown twice", release);
                }
                prop_assert_eq!(record, newer);
                if record {
                    seen = Some(run);
                }
            }
        }
    }
}
