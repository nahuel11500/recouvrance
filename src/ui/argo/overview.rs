//! The Argo CD page's overview, shown instead of the list until the list is asked for: what
//! the environments the filter keeps are, counted per environment (not per Application); those
//! that need a look, production first; per project, the version of the product each type of
//! environment runs and the environments not on it; the last operations.
//!
//! Review environments (the type named `review`) of other people are left out unless asked
//! for: one is the user's when its name holds their Argo CD login as a word
//! (`shop-review-jdoe-cart` for `jdoe`).
//!
//! It is worked out when the rows or the filter change (`refilter`), not per frame.

use std::collections::{BTreeMap, HashMap};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::envs::{self, Env};
use super::{ArgoView, key};
use crate::k8s;
use crate::k8s::argo::environments::EnvTypes;
use crate::k8s::argo::{FleetApp, operation_tone};
use crate::status::Tone;

/// The type whose environments belong to one person each.
pub const REVIEW: &str = "review";
/// Environments that need a look, and operations, listed at most.
const LOOKS_SHOWN: usize = 12;
const RECENT_SHOWN: usize = 8;
/// Tags shown for one component in one type, at most.
const TAGS_SHOWN: usize = 5;

/// How many environments are in each state. One can be in several.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Synced, healthy, nothing running.
    pub fine: usize,
    pub out_of_sync: usize,
    pub degraded: usize,
    pub failed: usize,
    pub running: usize,
}

/// The environments of a type, and how many of them need a look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeCount {
    pub kind: usize,
    pub envs: usize,
    pub looks: usize,
}

/// An environment that needs a look, and why, worst first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub env: usize,
    pub why: Vec<(String, Tone)>,
}

/// The tags one component of the product runs in the environments of one type: each tag with
/// the environments on it, the most common first (on a tie, the greater tag: most often the newer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeTags {
    pub kind: usize,
    pub tags: Vec<(String, Vec<usize>)>,
}

/// One component of a project's product (an image whose tags differ between its
/// environments) across the types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub types: Vec<TypeTags>,
}

/// A project's versions: per type, the tag most of its environments run of the main component
/// (the one most environments run), then each component in detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectVersions {
    pub project: String,
    pub drift: Vec<(usize, String)>,
    pub components: Vec<Component>,
}

/// The last operation of an environment (its newest of its Applications'): when it ended (else started), its phase, who started it,
/// the revision it synced to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub app: usize,
    pub env: usize,
    pub at: jiff::Timestamp,
    pub phase: String,
    pub by: Option<String>,
    pub revision: Option<String>,
}

/// The overview of the environments kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overview {
    /// The environments counted, and the review environments of other people left out.
    pub envs: usize,
    pub reviews_hidden: usize,
    pub counts: Counts,
    pub types: Vec<TypeCount>,
    /// All that need a look, in the order shown.
    pub looks: Vec<Look>,
    pub versions: Vec<ProjectVersions>,
    pub recent: Vec<Recent>,
}

/// Whether the environment `name` is the review environment of the user logged in as `user`:
/// its name holds the login as a word (dots and underscores read as dashes).
pub fn mine(name: &str, user: Option<&str>) -> bool {
    let Some(user) = user.map(|u| u.trim().to_lowercase().replace(['.', '_'], "-")).filter(|u| !u.is_empty()) else {
        return false;
    };
    format!("-{}-", name.to_lowercase()).contains(&format!("-{user}-"))
}

/// Why `env` needs a look at `now`, worst first: a sync that failed, Degraded, a sync running
/// for long, out of sync with auto-sync on (by hand, out of sync is how it waits for a sync).
pub fn why(env: &Env, apps: &[FleetApp], now: jiff::Timestamp) -> Vec<(String, Tone)> {
    let mut out = Vec::new();
    if env.state.failed {
        out.push(("Sync failed".to_string(), Tone::Bad));
    }
    if env.state.degraded {
        out.push(("Degraded".to_string(), Tone::Bad));
    }
    let longest = env
        .members
        .iter()
        .filter_map(|(i, _)| Some(&apps.get(*i)?.app))
        .filter(|a| a.operation.as_ref().is_some_and(|(p, _, _)| operation_tone(p) == Tone::Busy))
        .filter_map(|a| a.operation_started.as_deref()?.parse::<jiff::Timestamp>().ok())
        .map(|t| now.as_second() - t.as_second())
        .max();
    if let Some(secs) = longest.filter(|&s| s >= k8s::argo::diagnosis::STUCK_AFTER) {
        out.push((format!("Syncing for {}", k8s::duration_text(secs)), Tone::Busy));
    }
    if env.members.iter().filter_map(|(i, _)| apps.get(*i)).any(|a| a.app.automated && a.app.sync == "OutOfSync") {
        out.push(("Out of sync, auto-sync on".to_string(), Tone::Busy));
    }
    out
}

/// The name of an environment without its project's: `prod-acme` for `shop-prod-acme`.
pub fn short_name<'a>(env: &'a str, project: &str) -> &'a str {
    env.strip_prefix(project).and_then(|r| r.strip_prefix('-')).filter(|r| !r.is_empty()).unwrap_or(env)
}

/// The overview of the environments `keep` keeps, as of `now`; `users` is who each one's Argo
/// CD is logged in as; reviews of other people counted only when `reviews` is on.
pub fn overview(
    apps: &[FleetApp],
    all: &[Env],
    keep: &[bool],
    types: &EnvTypes,
    users: &[Option<String>],
    reviews: bool,
    now: jiff::Timestamp,
) -> Overview {
    let review = |e: &Env| types.name(e.kind) == REVIEW;
    let mut out = Overview::default();
    let mut shown: Vec<usize> = Vec::new();
    for (e, env) in all.iter().enumerate() {
        if !keep.get(e).copied().unwrap_or(false) {
            continue;
        }
        if review(env) && !reviews && !mine(&env.name, users.get(e).and_then(Option::as_deref)) {
            out.reviews_hidden += 1;
            continue;
        }
        shown.push(e);
    }
    out.envs = shown.len();
    let mut by_type: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    let mut looks = Vec::new();
    for &e in &shown {
        let env = &all[e];
        let s = env.state;
        let c = &mut out.counts;
        c.out_of_sync += usize::from(s.out_of_sync);
        c.degraded += usize::from(s.degraded);
        c.failed += usize::from(s.failed);
        c.running += usize::from(s.running);
        c.fine += usize::from(!s.out_of_sync && !s.degraded && !s.failed && !s.running);
        let why = why(env, apps, now);
        let t = by_type.entry(env.kind).or_default();
        t.0 += 1;
        t.1 += usize::from(!why.is_empty());
        if !why.is_empty() {
            looks.push(Look { env: e, why });
        }
    }
    out.types = by_type.into_iter().map(|(kind, (envs, looks))| TypeCount { kind, envs, looks }).collect();
    // Production first (the types' order), then the worst, then by name.
    looks.sort_by(|a, b| {
        let rank = |l: &Look| l.why.first().map_or(2, |(_, t)| if *t == Tone::Bad { 0 } else { 1 });
        (all[a.env].kind, rank(a), &all[a.env].name).cmp(&(all[b.env].kind, rank(b), &all[b.env].name))
    });
    out.looks = looks;
    out.versions = versions(all, &shown.iter().copied().filter(|&e| !review(&all[e])).collect::<Vec<_>>());
    let mut recent: Vec<Recent> = shown
        .iter()
        .flat_map(|&e| all[e].members.iter().map(move |(i, _)| (e, *i)))
        .filter_map(|(env, i)| {
            let a = &apps.get(i)?.app;
            let (phase, _, finished) = a.operation.as_ref()?;
            let at = finished.as_deref().or(a.operation_started.as_deref())?.parse::<jiff::Timestamp>().ok()?;
            Some(Recent {
                app: i,
                env,
                at,
                phase: phase.clone(),
                by: a.operation_by.clone(),
                revision: a.operation_revision.clone(),
            })
        })
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse((r.at, r.app)));
    // One per environment: its Applications are synced together, the newest says it.
    let mut seen = std::collections::HashSet::new();
    recent.retain(|r| seen.insert(r.env));
    recent.truncate(RECENT_SHOWN);
    out.recent = recent;
    out
}

/// Per project, the tags each component of its product runs in each type of environment, of
/// the environments `shown`.
pub fn versions(all: &[Env], shown: &[usize]) -> Vec<ProjectVersions> {
    // project → component → type → tag → environments
    type Tags = BTreeMap<String, Vec<usize>>;
    let mut tree: BTreeMap<&str, BTreeMap<&str, BTreeMap<usize, Tags>>> = BTreeMap::new();
    for &e in shown {
        let env = &all[e];
        for (_, image) in env.images.iter().filter(|(own, _)| *own) {
            let tag = image.tags.join(", ");
            tree.entry(&env.project)
                .or_default()
                .entry(&image.component)
                .or_default()
                .entry(env.kind)
                .or_default()
                .entry(tag)
                .or_default()
                .push(e);
        }
    }
    tree.into_iter()
        .map(|(project, components)| {
            let components: Vec<Component> = components
                .into_iter()
                .map(|(name, by_type)| Component {
                    name: name.to_string(),
                    types: by_type
                        .into_iter()
                        .map(|(kind, tags)| {
                            let mut tags: Vec<(String, Vec<usize>)> = tags.into_iter().collect();
                            tags.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| b.0.cmp(&a.0)));
                            TypeTags { kind, tags }
                        })
                        .collect(),
                })
                .collect();
            // Components on the same tags everywhere (a back-end and its front-end built
            // together) are one line.
            let mut merged: Vec<Component> = Vec::new();
            for c in components {
                match merged.iter_mut().find(|m| m.types == c.types) {
                    Some(m) => {
                        m.name.push_str(", ");
                        m.name.push_str(&c.name);
                    }
                    None => merged.push(c),
                }
            }
            let mut components = merged;
            // The main component first: the one most environments run, then by name.
            let reach = |c: &Component| c.types.iter().flat_map(|t| &t.tags).map(|(_, e)| e.len()).sum::<usize>();
            components.sort_by(|a, b| reach(b).cmp(&reach(a)).then_with(|| a.name.cmp(&b.name)));
            let drift = components.first().map_or_else(Vec::new, |c| {
                c.types.iter().filter_map(|t| Some((t.kind, envs::short_tag(&t.tags.first()?.0)))).collect()
            });
            ProjectVersions { project: project.to_string(), drift, components }
        })
        .collect()
}

// MARK: View

impl ArgoView {
    /// The overview of what the filter keeps (see the module's doc).
    pub(super) fn overview_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let (Some(rows), Some(o)) = (&self.rows, &self.overview) else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let apps = &rows.fleet.apps;
        let types = &rows.types;
        let section = |title: &str| {
            div()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(title.to_uppercase())
        };
        let c = o.counts;
        type Flip = fn(&mut envs::Filter);
        let stats: [(&str, usize, Tone, Option<Flip>); 5] = [
            ("Synced and healthy", c.fine, Tone::Good, None),
            ("Out of sync", c.out_of_sync, Tone::Busy, Some(|f| f.out_of_sync = true)),
            ("Degraded", c.degraded, Tone::Bad, Some(|f| f.degraded = true)),
            ("Sync failed", c.failed, Tone::Bad, Some(|f| f.degraded = true)),
            ("Syncing", c.running, Tone::Busy, Some(|f| f.running = true)),
        ];
        let tiles =
            h_flex().gap_2().flex_wrap().children(stats.into_iter().enumerate().map(|(i, (label, n, tone, flip))| {
                let color = if n == 0 { muted } else { tone.color(cx) };
                v_flex()
                    .id(("argo-ov-stat", i))
                    .w(px(150.))
                    .px_3()
                    .py_2()
                    .gap_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if n > 0 && tone != Tone::Good { color.opacity(0.5) } else { theme.border })
                    .bg(theme.secondary)
                    .child(div().text_xl().font_weight(FontWeight::BOLD).text_color(color).child(n.to_string()))
                    .child(div().text_xs().text_color(muted).child(label))
                    .when_some(flip, |d, flip| {
                        d.cursor_pointer()
                            .hover(|s| s.border_color(color))
                            .on_click(cx.listener(move |this, _, _, cx| this.show_list_with(flip, cx)))
                    })
            }));
        let by_type = h_flex().gap_2().flex_wrap().children(o.types.iter().enumerate().map(|(i, t)| {
            let name = types.name(t.kind).to_string();
            let (text, tone) = if t.looks > 0 {
                (format!("{} to look at", t.looks), Tone::Bad)
            } else {
                ("OK".to_string(), Tone::Good)
            };
            h_flex()
                .id(("argo-ov-type", i))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, _, cx| {
                        let name = name.clone();
                        this.show_list_with(move |f| f.kind = Some(name), cx);
                    }
                }))
                .child(div().text_sm().child(name))
                .child(div().text_xs().text_color(muted).child(t.envs.to_string()))
                .child(div().text_xs().text_color(tone.color(cx)).child(text))
        }));
        let env_link = |id: (&'static str, usize), e: usize, cx: &mut Context<Self>| {
            let k = key(&apps[rows.envs[e].main]);
            h_flex()
                .id(id)
                .gap_2()
                .px_1()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.open_dashboard(&k, cx)))
        };
        let looks = v_flex().children(o.looks.iter().take(LOOKS_SHOWN).enumerate().map(|(i, l)| {
            let env = &rows.envs[l.env];
            env_link(("argo-ov-look", i), l.env, cx)
                .child(div().size(px(7.)).flex_none().rounded_full().bg(env.tone.color(cx)))
                .child(div().w(px(80.)).flex_none().text_xs().text_color(muted).child(types.name(env.kind).to_string()))
                .child(
                    div().w(px(260.)).flex_none().truncate().text_sm().text_color(theme.blue).child(env.name.clone()),
                )
                .children(l.why.iter().map(|(text, tone)| super::dashboard::chip(text, *tone, cx)))
        }));
        let more_looks = o.looks.len().saturating_sub(LOOKS_SHOWN);
        let mono = theme.mono_font_family.clone();
        let versions = v_flex().gap_3().children(o.versions.iter().map(|p| {
            let drift = p
                .drift
                .iter()
                .map(|(kind, tag)| format!("{} {tag}", types.name(*kind)))
                .collect::<Vec<_>>()
                .join(" · ");
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_3()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(p.project.clone()))
                        .child(div().text_xs().font_family(mono.clone()).text_color(muted).child(drift)),
                )
                .children(p.components.iter().map(|c| {
                    v_flex().pl_3().gap_0p5().child(div().text_xs().text_color(muted).child(c.name.clone())).children(
                        c.types.iter().map(|t| {
                            h_flex()
                                .pl_3()
                                .gap_1p5()
                                .flex_wrap()
                                .child(
                                    div()
                                        .w(px(90.))
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(types.name(t.kind).to_string()),
                                )
                                .children(t.tags.iter().take(TAGS_SHOWN).enumerate().map(|(n, (tag, on))| {
                                    let text = if n == 0 {
                                        format!("{} ×{}", envs::short_tag(tag), on.len())
                                    } else {
                                        let names: Vec<&str> =
                                            on.iter().map(|&e| short_name(&rows.envs[e].name, &p.project)).collect();
                                        format!("{} · {}", envs::short_tag(tag), names.join(", "))
                                    };
                                    let color = if n == 0 { theme.foreground } else { theme.warning };
                                    div()
                                        .flex_none()
                                        .px_1p5()
                                        .rounded_md()
                                        .text_xs()
                                        .font_family(mono.clone())
                                        .text_color(color)
                                        .bg(if n == 0 { theme.secondary } else { theme.warning.opacity(0.12) })
                                        .child(text)
                                }))
                                .when(t.tags.len() > TAGS_SHOWN, |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("+{}", t.tags.len() - TAGS_SHOWN)),
                                    )
                                })
                        }),
                    )
                }))
        }));
        let recent = v_flex().children(o.recent.iter().enumerate().map(|(i, r)| {
            let k = key(&apps[r.app]);
            let tone = operation_tone(&r.phase);
            h_flex()
                .id(("argo-ov-op", i))
                .gap_2()
                .px_1()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.open_dashboard(&k, cx)))
                .child(
                    div().w(px(70.)).flex_none().text_xs().text_color(muted).child(format!("{} ago", k8s::age(r.at))),
                )
                .child(
                    div()
                        .w(px(320.))
                        .flex_none()
                        .truncate()
                        .text_sm()
                        .text_color(theme.blue)
                        .child(apps[r.app].app.name.clone()),
                )
                .child(super::dashboard::chip(&r.phase, tone, cx))
                .children(r.by.clone().map(|by| div().text_xs().text_color(muted).child(format!("by {by}"))))
                .children(
                    r.revision
                        .clone()
                        .map(|rev| div().text_xs().font_family(mono.clone()).text_color(muted).child(rev)),
                )
        }));
        let reviews = (o.reviews_hidden > 0 || self.show_reviews).then(|| {
            let text = if self.show_reviews {
                "Hide other people's review environments".to_string()
            } else {
                format!(
                    "{} left out (other people's review environments)",
                    crate::plural::count(o.reviews_hidden, "environment")
                )
            };
            Button::new("argo-ov-reviews").xsmall().ghost().label(text).on_click(cx.listener(|this, _, _, cx| {
                this.show_reviews = !this.show_reviews;
                this.refilter(cx);
                cx.notify();
            }))
        });
        v_flex()
            .id("argo-overview")
            .size_full()
            .overflow_y_scroll()
            .px_2()
            .pb_4()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::plural::count(o.envs, "environment"))
                    .children(reviews),
            )
            .child(tiles)
            .when(!o.types.is_empty(), |d| d.child(section("By type")).child(by_type))
            .when(!o.looks.is_empty(), |d| {
                d.child(section("Needs a look")).child(looks).when(more_looks > 0, |d| {
                    d.child(
                        Button::new("argo-ov-more-looks")
                            .xsmall()
                            .ghost()
                            .label(format!("{more_looks} more in the list"))
                            .on_click(cx.listener(|this, _, _, cx| this.show_list_with(|f| f.needs_look = true, cx))),
                    )
                })
            })
            .when(!o.versions.is_empty(), |d| d.child(section("Versions by type")).child(versions))
            .when(!o.recent.is_empty(), |d| {
                d.child(section("Last operations")).child(recent).child(
                    h_flex().child(
                        Button::new("argo-ov-journal")
                            .xsmall()
                            .ghost()
                            .label("Sync journal")
                            .on_click(cx.listener(|this, _, window, cx| this.open_journal(None, window, cx))),
                    ),
                )
            })
            .into_any_element()
    }

    /// Show the list, with `change` made to its filter.
    fn show_list_with(&mut self, change: impl FnOnce(&mut envs::Filter), cx: &mut Context<Self>) {
        self.list_shown = true;
        self.change_filter(change, cx);
    }
}

/// Who each environment's Argo CD is logged in as, read once per server.
pub fn users(apps: &[FleetApp], all: &[Env]) -> Vec<Option<String>> {
    let mut by_server: HashMap<String, Option<String>> = HashMap::new();
    all.iter()
        .map(|e| {
            let server = &apps[e.root()].server;
            by_server.entry(server.name()).or_insert_with(|| server.user()).clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` attribute would shadow the standard one.
    use super::{Env, EnvTypes, RECENT_SHOWN, Tone, envs, mine, overview, short_name, versions};
    use crate::k8s::argo::environments::EnvImage;
    use envs::State;

    fn env(name: &str, project: &str, kind: usize, state: State, tag: &str) -> Env {
        Env {
            members: vec![(0, 0)],
            name: name.into(),
            project: project.into(),
            kind,
            tone: Tone::Good,
            state,
            on_tag: false,
            images: vec![
                (true, EnvImage { repo: format!("r/{project}/api"), component: "api".into(), tags: vec![tag.into()] }),
                (false, EnvImage { repo: "redis".into(), component: "redis".into(), tags: vec!["7".into()] }),
            ],
            last_sync: None,
            cluster: String::new(),
            namespace: String::new(),
            dest_written: String::new(),
            on: None,
            main: 0,
            search: name.into(),
        }
    }

    #[test]
    fn a_review_is_mine_when_its_name_holds_my_login() {
        assert!(mine("shop-review-jdoe-cart", Some("jdoe")));
        assert!(mine("shop-review-j-doe-cart", Some("j.doe")));
        assert!(mine("shop-review-jdoe", Some("JDoe")));
        assert!(!mine("shop-review-jdoe2-cart", Some("jdoe")));
        assert!(!mine("shop-review-jdoe-cart", None));
        assert!(!mine("shop-review-jdoe-cart", Some(" ")));
    }

    #[test]
    fn versions_name_the_tag_most_run_and_the_environments_off_it() {
        let ok = State::default();
        let all = vec![
            env("shop-prod-a", "shop", 0, ok, "1.4.2"),
            env("shop-prod-b", "shop", 0, ok, "1.4.2"),
            env("shop-prod-c", "shop", 0, ok, "1.4.1"),
            env("shop-staging-a", "shop", 4, ok, "main-77b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e"),
        ];
        let v = versions(&all, &[0, 1, 2, 3]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].drift, [(0, "1.4.2".to_string()), (4, "main-77b324b".to_string())]);
        let api = &v[0].components;
        assert_eq!(api.len(), 1, "third-party images are left out");
        assert_eq!(api[0].types[0].tags, [("1.4.2".to_string(), vec![0, 1]), ("1.4.1".to_string(), vec![2])]);
        assert_eq!(short_name("shop-prod-c", "shop"), "prod-c");
        assert_eq!(short_name("shop", "shop"), "shop");
    }

    #[test]
    fn counts_are_per_environment_and_other_peoples_reviews_are_left_out() {
        let types = EnvTypes::default();
        let review = types.of("shop-review-x", false);
        let oos = State { out_of_sync: true, ..Default::default() };
        let all = vec![
            env("shop-prod-a", "shop", 0, State::default(), "1"),
            env("shop-prod-b", "shop", 0, oos, "1"),
            env("shop-review-jdoe-x", "shop", review, State::default(), "2"),
            env("shop-review-ann-y", "shop", review, State::default(), "3"),
        ];
        let users = vec![Some("jdoe".to_string()); 4];
        let o = overview(&[], &all, &[true; 4], &types, &users, false, jiff::Timestamp::UNIX_EPOCH);
        assert_eq!((o.envs, o.reviews_hidden), (3, 1));
        assert_eq!((o.counts.fine, o.counts.out_of_sync), (2, 1));
        assert!(o.versions[0].components[0].types.iter().all(|t| t.kind != review), "no review in the versions");
        let o = overview(&[], &all, &[true, false, true, true], &types, &users, true, jiff::Timestamp::UNIX_EPOCH);
        assert_eq!((o.envs, o.reviews_hidden), (3, 0));
    }

    /// The made-up example (the developer tools'): its rows, as the page works them out.
    fn example(now: jiff::Timestamp) -> super::super::Rows {
        use crate::k8s::argo::{self, ApiRead, Summary, api};
        use std::sync::Arc;
        let endpoint = Arc::new(api::Endpoint {
            server: argo::server::Server::parse("https://argocd.example.com").expect("a valid address"),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Err("a made-up example".into()),
            on: true,
            settings_token: false,
            renewable: false,
        });
        let mut state = api::FeedState { listed: true, live: true, ..Default::default() };
        for v in argo::environments::example_apps(now) {
            let a = Arc::new(api::ApiApp { summary: Summary::of(&v), value: v });
            state.apps.insert((a.summary.namespace.clone(), a.summary.name.clone()), a);
        }
        let fleet = argo::from_api(vec![ApiRead { endpoint, state }], &[], &[], &Default::default());
        let mut plan = argo::windows::Planner::new(now);
        super::super::Rows::new(fleet, Arc::new(EnvTypes::default()), &mut plan)
    }

    #[test]
    fn what_needs_a_look_comes_production_first_and_waiting_for_a_hand_sync_is_not_it() {
        let now = jiff::Timestamp::now();
        let rows = example(now);
        let keep = vec![true; rows.envs.len()];
        let o = overview(&rows.fleet.apps, &rows.envs, &keep, &rows.types, &rows.users, false, now);
        let looks: Vec<(&str, Vec<&str>)> = o
            .looks
            .iter()
            .map(|l| (rows.envs[l.env].name.as_str(), l.why.iter().map(|(w, _)| w.as_str()).collect()))
            .collect();
        // shop-prod-globex is out of sync with auto-sync off: it waits for a sync by hand.
        assert_eq!(
            looks,
            [
                ("billing-preprod-acme", vec!["Degraded"]),
                ("shop-acc-globex", vec!["Syncing for 3h"]),
                ("shop-staging-acme", vec!["Sync failed"]),
            ]
        );
        assert_eq!(o.reviews_hidden, 1, "no login: every review is someone else's");
        let shop = o.versions.iter().find(|p| p.project == "shop").expect("shop's versions");
        let types = &rows.types;
        let drift: Vec<(&str, &str)> = shop.drift.iter().map(|(k, t)| (types.name(*k), t.as_str())).collect();
        assert_eq!(drift[0], ("prod", "1.4.2"), "{drift:?}");
        let names: Vec<&str> = shop.components.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["api, web"], "on the same tags: one line; redis and the exporter left out");
        assert_eq!(o.recent.len(), RECENT_SHOWN);
        assert!(o.recent.windows(2).all(|w| w[0].at >= w[1].at), "newest first");
        let envs: std::collections::HashSet<usize> = o.recent.iter().map(|r| r.env).collect();
        assert_eq!(envs.len(), o.recent.len(), "one per environment");
        let envs: std::collections::HashSet<usize> = o.recent.iter().map(|r| r.env).collect();
        assert_eq!(envs.len(), o.recent.len(), "one per environment");
    }
}
