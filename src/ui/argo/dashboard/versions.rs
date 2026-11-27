//! The dashboard's Versions card: the product's images only, one chip per component with its
//! release version (red, with the newer one, when its registry has a newer release tag), then
//! one line, who released it and the previous release; and Roll back, to that previous release,
//! its dialog saying what changes (see `k8s::argo::release`).
//!
//! The previous release is the last deployment of the history whose product versions differ
//! from those running. A deployment's versions come from the images its sources pin, else from
//! the manifests Argo CD renders at its revision (read one deployment at a time, newest first,
//! until one differs). The registry is read for the environment types Settings → Argo CD says
//! (see `crate::registry`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{ArgoView, Dashboard, Key, card, with_tip};
use crate::details::{ArgoAction, ArgoExtra};
use crate::k8s;
use crate::k8s::argo::dashboard::{Cards, split_image};
use crate::k8s::argo::release::{self, Change, Chip, Names, Pick, Release};
use crate::k8s::argo::{api, environments, short_revision, sync_diff};
use crate::registry::{self, Status};
use crate::ui::selectable::selectable;

/// Deployments looked through for the previous release, at most.
const LOOK_BACK: usize = 10;

/// What the Versions card reads besides the cards: the images of earlier deployments, and the
/// registries' tags.
#[derive(Default)]
pub(super) struct State {
    /// The images each deployment ran, rendered at its revision: by Application, deployment id
    /// and the Application's sources (their hash).
    images: HashMap<(Key, i64, u64), Result<Vec<String>, String>>,
    /// The deployment whose images are being read.
    reading: Option<(Key, i64, u64)>,
    load: crate::latest::Latest,
    /// A check of the registries runs.
    checking: bool,
    registry: crate::latest::Latest,
}

/// The previous release, as far as known.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Previous {
    /// The deployment of this id, at this index of those before the newest, and what it ran.
    Found { id: i64, at: usize, release: Release },
    /// Being found: the images of the deployment of this id are to be read.
    Reading(i64),
    /// It can't be told: why.
    Unknown(String),
    /// Every deployment in the history runs these versions.
    None,
}

/// What the Versions card says.
struct Model {
    chips: Vec<Chip>,
    current: Release,
    previous: Previous,
    /// The deployment that released what runs (its id).
    released: Option<i64>,
}

/// The images running: those of the workloads, else every image Argo CD lists.
fn running(c: &Cards) -> Vec<String> {
    match &c.workloads {
        Some((live, _)) if !live.is_empty() => live.clone(),
        _ => {
            c.versions
                .images
                .iter()
                .map(|i| {
                    if i.tag.starts_with('@') {
                        format!("{}{}", i.repo, i.tag)
                    } else {
                        format!("{}:{}", i.repo, i.tag)
                    }
                })
                .collect()
        }
    }
}

/// The image a chip's version was read from: (repository, tag), its own repository first.
fn deployed(chip: &Chip) -> Option<(String, String)> {
    let mut found: Vec<(String, String)> = chip
        .refs
        .iter()
        .map(|r| split_image(r))
        .filter(|(_, t)| release::version_of(t).text == chip.version.text)
        .collect();
    found.sort_by_key(|(r, _)| *r != chip.repo);
    found.into_iter().next()
}

impl ArgoView {
    /// The product's images as the page guesses them: those of its environment known as the
    /// product's (see `own_images`), only those named after its project or product when some
    /// are (a third-party image whose tags differ between environments isn't one).
    fn product_guess(&self, d: &Dashboard, c: &Cards) -> HashSet<String> {
        let product = d.title.split('-').next().unwrap_or_default().to_string();
        let mut project = String::new();
        let mut own = HashSet::new();
        if let Some(k) = d.current() {
            if let Some(a) = self.find(k) {
                project.clone_from(&a.app.project);
            }
            own = self.own_images(k);
        }
        if own.is_empty() {
            own = c.versions.images.iter().map(|i| i.repo.clone()).collect();
        }
        let named_after = |r: &String| {
            environments::named_after(r, &product) || (!project.is_empty() && environments::named_after(r, &project))
        };
        let named: HashSet<String> = own.iter().filter(|r| named_after(r)).cloned().collect();
        if named.is_empty() && d.current().is_some() { own } else { named }
    }

    /// The chips, the versions running, and the previous release as far as known.
    fn versions_model(&self, d: &Dashboard, c: &Cards) -> Model {
        let names = crate::settings::with(|s| Names::parse(&s.argocd_image_names));
        let guess = self.product_guess(d, c);
        let images = running(c);
        let chips = release::chips(&images, &names, |r| guess.contains(r));
        let current = release::release_in(&chips, &images);
        let entries = &c.history.entries;
        let newest = entries.last().map(|e| e.id);
        let all_before = &entries[..entries.len().saturating_sub(1)];
        let before = &all_before[all_before.len().saturating_sub(LOOK_BACK)..];
        let spec = c.history.spec.hash;
        let mut known: Vec<Option<Release>> = Vec::new();
        let mut failed: HashMap<usize, String> = HashMap::new();
        for (i, e) in before.iter().enumerate() {
            if let Some(r) = release::pinned_release(&chips, &release::pinned_images(&e.sources)) {
                known.push(Some(r));
                continue;
            }
            match d.current().and_then(|k| d.versions.images.get(&(k.clone(), e.id, spec))) {
                Some(Ok(images)) => known.push(Some(release::release_in(&chips, images))),
                Some(Err(why)) => {
                    failed.insert(i, why.clone());
                    known.push(None);
                }
                None => known.push(None),
            }
        }
        let previous = if chips.is_empty() {
            Previous::Unknown("no image of the product is known".into())
        } else {
            match release::previous_release(&current, &known) {
                Pick::Found(i) => {
                    Previous::Found { id: before[i].id, at: i, release: known[i].clone().unwrap_or_default() }
                }
                Pick::Needs(i) => match failed.remove(&i) {
                    Some(why) => Previous::Unknown(why),
                    None if d.example.is_some() => Previous::Unknown("a made-up example: nothing is rendered".into()),
                    None => Previous::Reading(before[i].id),
                },
                Pick::None => Previous::None,
            }
        };
        let released = match &previous {
            Previous::Found { at, .. } => before.get(at + 1).map(|e| e.id).or(newest),
            Previous::None => before.first().map(|e| e.id).or(newest),
            _ => newest,
        };
        Model { chips, current, previous, released }
    }

    /// After a read of the tab shown: read the images of the next deployment the previous
    /// release is looked for in, and check the registry of the chips not checked lately.
    pub(super) fn ask_versions(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.dashboard else { return };
        if d.example.is_some() {
            return;
        }
        let Some((k, Ok(cards))) = &d.cards else { return };
        if Some(k) != d.current() {
            return;
        }
        let (k, cards) = (k.clone(), cards.clone());
        let m = self.versions_model(d, &cards);
        let check = crate::settings::with(|s| release::checks_tags(&s.argocd_tag_check_types, &d.kind));
        let wanted: Vec<(String, String)> = if check && !d.versions.checking {
            m.chips
                .iter()
                .filter(|c| c.version.numeric())
                .filter_map(deployed)
                .filter(|(r, t)| registry::known(r, t).1)
                .collect()
        } else {
            Vec::new()
        };
        let spec = cards.history.spec;
        let read = match m.previous {
            Previous::Reading(id) if d.versions.reading != Some((k.clone(), id, spec.hash)) => {
                let entry = cards.history.entries.iter().find(|e| e.id == id).cloned();
                Some((id, entry))
            }
            _ => None,
        };
        let found = self.find(&k).map(|a| (a.server.clone(), a.app.namespace.clone(), a.app.name.clone()));
        let Some(d) = &mut self.dashboard else { return };
        if !wanted.is_empty() {
            d.versions.checking = true;
            d.versions.registry.spawn(cx, registry::check(wanted), |this: &mut Self, (), cx| {
                if let Some(d) = &mut this.dashboard {
                    d.versions.checking = false;
                }
                cx.notify();
            });
        }
        let Some((id, entry)) = read else { return };
        let key = (k, id, spec.hash);
        let fail = |why: String| -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>>>> {
            Box::pin(async move { Err(why) })
        };
        let work: Pin<Box<dyn Future<Output = Result<Vec<String>, String>>>> = match (entry, found) {
            (Some(e), Some((server, ns, name))) => match (sync_diff::plan(spec, &e, &e), e.revision.clone()) {
                (Ok(_), Some(revision)) => Box::pin(api::images_at(server, ns, name, spec.hash, revision)),
                (Err(why), _) => fail(why),
                (Ok(_), None) => fail(format!("#{id} has no revision recorded")),
            },
            _ => fail("This Application is no longer listed.".into()),
        };
        d.versions.reading = Some(key.clone());
        d.versions.load.spawn(cx, work, move |this: &mut Self, images, cx| {
            if let Some(d) = &mut this.dashboard {
                if d.versions.reading.as_ref() == Some(&key) {
                    d.versions.reading = None;
                }
                d.versions.images.insert(key, images);
            }
            cx.notify();
            this.ask_versions(cx);
        });
    }

    /// The product's chips, one line (who released it, the previous release), and Roll back.
    pub(super) fn versions_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let m = self.versions_model(d, c);
        let check = crate::settings::with(|s| release::checks_tags(&s.argocd_tag_check_types, &d.kind));
        let chips: Vec<Stateful<Div>> =
            m.chips.iter().enumerate().map(|(i, chip)| version_chip(i, chip, check, cx)).collect();
        let mut line = Vec::new();
        let dot = m.released.and_then(|id| c.history.dots.iter().find(|x| x.id == Some(id)));
        if let Some(dot) = dot {
            let mut released = String::from("Released");
            if let Some(t) = dot.at {
                released.push_str(&format!(" {} ago", k8s::age(t)));
            }
            if let Some(by) = &dot.by {
                released.push_str(&format!(" by {by}"));
            }
            line.push(released);
        }
        match &m.previous {
            Previous::Found { release: before, .. } => {
                let text = release::previous_text(&release::changes(&m.chips, &m.current, before));
                if !text.is_empty() {
                    line.push(format!("previous: {text}"));
                }
            }
            Previous::Reading(_) => line.push("previous: reading…".into()),
            Previous::None if c.history.entries.len() > 1 => line.push("no previous release in the history".into()),
            _ => {}
        }
        let empty = if c.versions.images.is_empty() {
            "Argo CD lists no image."
        } else {
            "No image of the product is known: name them in Settings → Argo CD."
        };
        card("Versions", None, cx)
            .when(m.chips.is_empty(), |v| v.child(div().text_sm().text_color(muted).child(empty)))
            .when(!chips.is_empty(), |v| v.child(h_flex().flex_wrap().gap_1p5().children(chips)))
            .when(!line.is_empty(), |v| {
                // What git deployed, on hover: each source's revision and commit.
                let deployed: Vec<String> = c
                    .deployed
                    .iter()
                    .map(|(dep, commit)| {
                        let mut s = format!("git {}", short_revision(&dep.revision));
                        if let Some(Ok(commit)) = commit {
                            s.push_str(&format!(": {}", commit.message.lines().next().unwrap_or_default()));
                        }
                        s
                    })
                    .collect();
                let text = div().text_xs().text_color(muted).child(selectable(line.join(" · ")));
                if deployed.is_empty() {
                    v.child(text)
                } else {
                    v.child(with_tip("argo-dash-released", deployed.join("\n")).child(text))
                }
            })
            .child(h_flex().pt_1().child(self.rollback_button(d, c, &m, cx)))
    }

    /// Roll back to the previous release; disabled, saying why, when there is none (or
    /// auto-sync is on). When the previous release can't be told, the deployment before the
    /// newest.
    fn rollback_button(&self, d: &Dashboard, c: &Cards, m: &Model, cx: &mut Context<Self>) -> AnyElement {
        let fallback = c.versions.rollback_to;
        let (target, why): (Option<i64>, Option<String>) = if c.versions.auto_sync {
            (None, Some("Auto-sync is on: Argo CD allows no rollback.".into()))
        } else if c.history.entries.len() < 2 {
            (None, Some("No previous release: the history has no deployment before this one.".into()))
        } else {
            match &m.previous {
                Previous::Found { id, .. } => (Some(*id), None),
                Previous::Reading(_) => {
                    (None, Some("Finding the previous release: the versions of earlier deployments are read.".into()))
                }
                Previous::None => (
                    None,
                    Some(format!(
                        "No previous release: the last {} ran these versions.",
                        crate::plural::count(c.history.entries.len().min(LOOK_BACK + 1), "deployment")
                    )),
                ),
                Previous::Unknown(_) => (fallback, None),
            }
        };
        let label = match (&m.previous, target) {
            (Previous::Found { .. }, Some(_)) => "Roll back to the previous release…".to_string(),
            (_, Some(id)) => format!("Roll back to #{id}…"),
            _ => "Roll back…".to_string(),
        };
        let k = d.current().cloned();
        let button = Button::new("argo-dash-rollback")
            .small()
            .outline()
            .icon(Lucide::Undo2)
            .label(label)
            .disabled(target.is_none() || k.is_none() || d.example.is_some())
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(id) = target {
                    this.confirm_rollback(id, window, cx);
                }
            }));
        match why {
            Some(why) => with_tip("argo-dash-rollback-why", why).child(button).into_any_element(),
            None => button.into_any_element(),
        }
    }

    /// Confirm, then roll the tab shown back to the deployment `id`: its dialog says which
    /// deployment, what changes (each chip's version now and after), and what to know first.
    fn confirm_rollback(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.dashboard else { return };
        let Some(Ok(c)) = d.shown() else { return };
        let Some(k) = d.current().cloned() else { return };
        let Some(a) = self.find(&k).cloned() else { return };
        let m = self.versions_model(d, &c);
        let after = match &m.previous {
            Previous::Found { id: found, release, .. } if *found == id => Ok(release.clone()),
            Previous::Unknown(why) => Err(format!("The versions it ran can't be told: {why}.")),
            _ => Err("The versions it ran aren't known yet.".to_string()),
        };
        let changes = after.map(|r| release::changes(&m.chips, &m.current, &r));
        let dot = c.history.dots.iter().find(|x| x.id == Some(id));
        let mut intro = format!("Argo CD redeploys history #{id}");
        if let Some(dot) = dot {
            if !dot.revision.is_empty() {
                intro.push_str(&format!(": git {}", short_revision(&dot.revision)));
            }
            if let Some(t) = dot.at {
                intro.push_str(&format!(", deployed {} ago", k8s::age(t)));
            }
        }
        intro.push('.');
        let found = matches!(&m.previous, Previous::Found { id: f, .. } if *f == id);
        let title =
            if found { "Roll back to the previous release?".to_string() } else { format!("Roll back to #{id}?") };
        let typed = environments::is_production(&d.kind).then(|| d.title.clone());
        let body: Rc<dyn Fn(&App) -> AnyElement> = Rc::new(move |cx: &App| rollback_body(&changes, cx));
        let extra = ArgoExtra { title: Some(title), note: Some(intro), body: Some(body), typed };
        crate::details::confirm_api_action_with(
            &a.server,
            &a.app.namespace,
            &a.app.name,
            ArgoAction::Rollback(Some(id)),
            extra,
            window,
            cx,
        );
    }

    /// Each chip's version running and after a sync of the tab shown (what git renders): what
    /// the Sync confirmation shows. Empty when the managed resources weren't read.
    pub(super) fn sync_versions(&self) -> Vec<Change> {
        let Some(d) = &self.dashboard else { return Vec::new() };
        let Some(Ok(c)) = d.shown() else { return Vec::new() };
        let Some((_, target)) = &c.workloads else { return Vec::new() };
        let m = self.versions_model(d, &c);
        let after = release::release_in(&m.chips, target);
        release::changes(&m.chips, &m.current, &after)
    }
}

/// A chip: its short name and release version; red with the newer version when its registry
/// has a newer release tag. Its tooltip says what the registry said and the whole references;
/// a click copies them.
fn version_chip(i: usize, chip: &Chip, check: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let read = deployed(chip).filter(|_| check && chip.version.numeric()).map(|(r, t)| registry::known(&r, &t).0);
    let mut tip: Vec<String> = Vec::new();
    let mut newer = None;
    match &read {
        Some(Some(c)) => {
            match &c.status {
                Status::Newer { version, published, .. } => {
                    newer = Some(version.clone());
                    tip.push(match published {
                        Some(t) => format!("Not the latest tag: {version} was published {} ago.", k8s::age(*t)),
                        None => format!("Not the latest tag: {version} exists."),
                    });
                }
                Status::Latest => tip.push("The latest release tag.".into()),
                Status::NoCredentials(host) => {
                    tip.push(format!("Newer tags not checked: the Docker config has no credentials for {host}."));
                }
                Status::Failed(why) => tip.push(format!("Newer tags not checked: {why}.")),
                Status::NotAVersion => {}
            }
            if let Some(why) = &c.stale {
                tip.push(format!("The registry can't be read now ({why}): as read {} ago.", k8s::age(c.at)));
            }
        }
        Some(None) => tip.push("Checking the registry for a newer tag…".into()),
        None => {}
    }
    if chip.several {
        tip.push("It runs at several versions: the newest is shown.".into());
    }
    tip.push(chip.refs.join("\n"));
    tip.push("Click to copy".into());
    let copied = chip.refs.join("\n");
    let red = newer.is_some();
    let (name_color, version_color) =
        if red { (theme.danger, theme.danger) } else { (theme.muted_foreground, theme.foreground) };
    with_tip(("argo-dash-version", i), tip.join("\n"))
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .text_sm()
        .cursor_pointer()
        .border_color(if red { theme.danger.opacity(0.6) } else { theme.border })
        .bg(if red { theme.danger.opacity(0.08) } else { theme.secondary })
        .child(div().text_color(name_color).child(chip.name.clone()))
        .child(
            div()
                .font_family(theme.mono_font_family.clone())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(version_color)
                .child(chip.version.text.clone()),
        )
        .when_some(newer, |d, v| d.child(div().text_color(theme.danger).child(format!("→ {v}"))))
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copied.clone())))
}

/// A section's heading in a dialog.
fn heading(text: &str, cx: &App) -> Div {
    div()
        .pt_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
}

/// Each chip's version before → after; those unchanged said so.
pub(in crate::ui::argo) fn changes_table(changes: &[Change], cx: &App) -> Div {
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    v_flex().gap_0p5().text_sm().children(changes.iter().map(|ch| {
        let side = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".into());
        let row = h_flex()
            .gap_2()
            .min_w_0()
            .child(div().w(px(130.)).flex_none().truncate().font_weight(FontWeight::MEDIUM).child(ch.name.clone()));
        if ch.changed() {
            row.child(div().font_family(mono.clone()).text_color(theme.muted_foreground).child(side(&ch.before)))
                .child(div().text_color(theme.muted_foreground).child("→"))
                .child(
                    div()
                        .font_family(mono.clone())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.warning)
                        .child(side(&ch.after)),
                )
        } else {
            row.child(div().font_family(mono.clone()).child(side(&ch.before)))
                .child(div().text_xs().text_color(theme.muted_foreground).child("unchanged"))
        }
    }))
}

/// What the rollback dialog shows under its note: what changes, then what to know before.
fn rollback_body(changes: &Result<Vec<Change>, String>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let what = match changes {
        Ok(c) if !c.is_empty() => changes_table(c, cx),
        Ok(_) => div().text_sm().text_color(muted).child("No image of the product is known."),
        Err(why) => div().text_sm().text_color(theme.warning).child(why.clone()),
    };
    let points = [
        "It is temporary: keep auto-sync off. The Application is then OutOfSync with git.",
        "The next sync deploys git's versions again: revert them in git to keep this release.",
        "Nothing is pruned: what the release undone added stays.",
        "Hooks don't run: a database migration of the release undone is not undone, and the older images run \
         on the migrated schema.",
        "Then follow it in the Last sync card.",
    ];
    v_flex()
        .gap_1()
        .child(heading("What changes", cx))
        .child(what)
        .child(heading("Before you click", cx))
        .children(points.iter().map(|p| {
            h_flex()
                .gap_1p5()
                .items_start()
                .text_sm()
                .child(div().flex_none().text_color(muted).child("•"))
                .child(div().min_w_0().child(p.to_string()))
        }))
        .into_any_element()
}
