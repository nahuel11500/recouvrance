//! The approval banners at the top of the window, kept so a click never lands on something
//! that moved under the pointer.
//!
//! A banner that goes (answered here or elsewhere, refused, its agent gone) stays in place a
//! moment, its buttons off, before the page moves up into its space: a click aimed at its
//! Approve hits the inert banner, not the page's header (Save view, Delete...). While the
//! pointer is over the banners, one that went stays (up to `MAX_LINGER`). A banner that just
//! showed, or one drawn somewhere else than the last time (one above it went, its text
//! changed), takes no click for a moment either: the click was aimed at what was there before.
//!
//! Not covered: the page under the banners still moves when one goes, after its moment; a
//! click aimed at the page then may land on what moved there.
//!
//! The banners are drawn in the window's flow, under menus, popovers, toasts and the command
//! palette. While a dialog is open, their buttons are also drawn above it (its backdrop
//! would otherwise take their clicks): an open dialog doesn't keep a request from an answer.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::mcp::{Approval, Hub};

/// How long a banner that went stays in place, inert.
pub const LINGER: Duration = Duration::from_millis(800);
/// How long one that went stays at most while the pointer is over the banners.
pub const MAX_LINGER: Duration = Duration::from_secs(5);
/// How long a banner that just showed or moved ignores clicks.
pub const ARM: Duration = Duration::from_millis(500);
/// The button rows drawn over an open dialog: above dialogs (10, plus one per dialog open
/// over another), under menus and popovers (100).
const OVER_DIALOGS: usize = 30;

struct Banner<T> {
    id: u64,
    item: T,
    /// Clicks count from then on.
    armed_at: Instant,
    /// When it stopped waiting (it lingers from then).
    gone_at: Option<Instant>,
    /// Where it was last drawn.
    origin: Option<Point<Pixels>>,
}

pub struct Banners<T> {
    list: Vec<Banner<T>>,
    /// The pointer is over the banners: those that went stay (up to `MAX_LINGER`).
    hovered: bool,
}

impl<T> Default for Banners<T> {
    fn default() -> Self {
        Self { list: Vec::new(), hovered: false }
    }
}

impl<T> Banners<T> {
    /// A new request: its banner goes under the others, which do not move.
    pub fn push(&mut self, id: u64, item: T, now: Instant) {
        self.list.push(Banner { id, item, armed_at: now + ARM, gone_at: None, origin: None });
    }

    /// The request no longer waits: its banner stays, inert, for a moment (see
    /// [`Banners::drop_due`]).
    pub fn resolve(&mut self, id: u64, now: Instant) {
        if let Some(b) = self.list.iter_mut().find(|b| b.id == id && b.gone_at.is_none()) {
            b.gone_at = Some(now);
        }
    }

    pub fn set_hovered(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    /// When banner `b`, gone, is taken away.
    fn due(&self, gone_at: Instant) -> Instant {
        gone_at + if self.hovered { MAX_LINGER } else { LINGER }
    }

    /// Take away the banners that went and have lingered. Those under one move up into its
    /// space, so they ignore clicks for `ARM`. Returns whether any went.
    pub fn drop_due(&mut self, now: Instant) -> bool {
        let mut dropped = false;
        let mut i = 0;
        while i < self.list.len() {
            if self.list[i].gone_at.is_some_and(|g| now >= self.due(g)) {
                self.list.remove(i);
                for b in &mut self.list[i..] {
                    b.armed_at = b.armed_at.max(now + ARM);
                }
                dropped = true;
            } else {
                i += 1;
            }
        }
        dropped
    }

    /// Banner `id` was drawn at `origin`. Drawn elsewhere than the last time, it ignores
    /// clicks for `ARM`: returns true then.
    pub fn placed(&mut self, id: u64, origin: Point<Pixels>, now: Instant) -> bool {
        let Some(b) = self.list.iter_mut().find(|b| b.id == id) else { return false };
        let moved = b.origin.is_some_and(|o| o != origin);
        b.origin = Some(origin);
        if moved && b.gone_at.is_none() {
            b.armed_at = b.armed_at.max(now + ARM);
        }
        moved
    }

    /// Whether a click on banner `id`'s buttons counts now: it still waits, and it has been
    /// where it is for `ARM`.
    pub fn takes_clicks(&self, id: u64, now: Instant) -> bool {
        self.list.iter().any(|b| b.id == id && b.gone_at.is_none() && now >= b.armed_at)
    }

    /// The banners to draw, in order, with whether each still waits.
    pub fn shown(&self) -> impl Iterator<Item = (&T, bool)> {
        self.list.iter().map(|b| (&b.item, b.gone_at.is_none()))
    }

    /// The requests that still wait.
    pub fn waiting(&self) -> impl Iterator<Item = &T> {
        self.list.iter().filter(|b| b.gone_at.is_none()).map(|b| &b.item)
    }

    /// How many requests still wait.
    pub fn waiting_count(&self) -> usize {
        self.waiting().count()
    }

    /// When something changes next: a banner that went is taken away, or one that showed
    /// or moved takes clicks (its buttons are drawn on then).
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        self.list
            .iter()
            .filter_map(|b| match b.gone_at {
                Some(g) => Some(self.due(g).max(now)),
                None => (b.armed_at > now).then_some(b.armed_at),
            })
            .min()
    }
}

/// An answer to a request, from its banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Deny,
    Approve,
    /// Approve, and allow the same for an hour.
    Hour,
    /// Approve, and allow exactly this from now on.
    Always,
}

/// The approval banners of a window.
pub struct ApprovalStrip {
    banners: Banners<Arc<Approval>>,
    hub: Arc<Hub>,
    /// Sends an answer (the hub; a test records them).
    answer: Rc<dyn Fn(&Hub, u64, bool)>,
    /// Wakes when a banner that went is to be taken away, or one turns its buttons on.
    wake: Option<Task<()>>,
}

impl ApprovalStrip {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self {
            banners: Banners::default(),
            hub,
            answer: Rc::new(|hub, id, yes| hub.resolve_approval(id, yes)),
            wake: None,
        }
    }

    /// The clock: the executor's, so tests can move it.
    fn now(cx: &App) -> Instant {
        cx.background_executor().now()
    }

    pub fn requested(&mut self, a: Arc<Approval>, cx: &mut Context<Self>) {
        self.banners.push(a.id, a, Self::now(cx));
        self.schedule(cx);
        cx.notify();
    }

    pub fn resolved(&mut self, id: u64, cx: &mut Context<Self>) {
        self.banners.resolve(id, Self::now(cx));
        self.schedule(cx);
        cx.notify();
    }

    pub fn waiting_count(&self) -> usize {
        self.banners.waiting_count()
    }

    /// One task wakes at the next change ([`Banners::next_wake`]), takes away what is due,
    /// redraws and sets itself for the change after.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        let now = Self::now(cx);
        let Some(at) = self.banners.next_wake(now) else {
            self.wake = None;
            return;
        };
        let timer = cx.background_executor().timer(at.saturating_duration_since(now));
        self.wake = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                this.banners.drop_due(Self::now(cx));
                this.schedule(cx);
                cx.notify();
            });
        }));
    }

    /// A click on a banner's button. It counts only while the request waits and the banner
    /// has been where it is for a moment (it is checked again now: it may have gone or moved
    /// since it was drawn).
    fn choose(&mut self, a: &Approval, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        if !self.banners.takes_clicks(a.id, Self::now(cx)) {
            return;
        }
        let rule = match choice {
            Choice::Deny | Choice::Approve => None,
            Choice::Hour => a.as_rule_for_an_hour(),
            Choice::Always => a.as_rule(),
        };
        // A request that no longer waits (its agent gone, or timed out) adds no rule.
        if let Some(rule) = rule
            && self.hub.approval_pending(a.id)
            && let Err(e) = self.hub.update_agent(&a.agent_id, |agent| agent.policy.rules.push(rule))
        {
            window.push_notification(Notification::error(format!("Rule not saved: {e:#}")), cx);
        }
        (self.answer)(&self.hub, a.id, choice != Choice::Deny);
    }
}

impl Render for ApprovalStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Self::now(cx);
        let over_dialog = window.has_active_dialog(cx);
        let view = cx.entity().downgrade();
        let theme = cx.theme().clone();
        let rows: Vec<AnyElement> = self
            .banners
            .shown()
            .map(|(a, waiting)| {
                let id = a.id;
                let clickable = self.banners.takes_clicks(id, now);
                let button = |name: &'static str, label: &'static str, choice: Choice| {
                    let a = a.clone();
                    div().debug_selector(move || format!("{name}-{id}")).child(
                        Button::new((name, id as usize))
                            .small()
                            .label(label)
                            .disabled(!clickable)
                            .on_click(cx.listener(move |this, _, window, cx| this.choose(&a, choice, window, cx))),
                    )
                };
                let mut buttons = h_flex().flex_none().gap_3().items_center().occlude();
                // Only offered when a rule can cover exactly this request (not exec, for one).
                if a.as_rule().is_some() {
                    buttons = buttons.child(button("hour", "Allow for 1h", Choice::Hour)).child(button(
                        "always",
                        "Always allow this",
                        Choice::Always,
                    ));
                }
                let buttons = buttons
                    // An unanswered request is refused: say when. One that went says so while it
                    // stays. A fixed width, so the text changing doesn't move the buttons.
                    .child(
                        div()
                            .flex_none()
                            .w(px(112.))
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if waiting {
                                super::chrome::refused_in(a.time_left(Instant::now()))
                            } else {
                                "no longer waiting".into()
                            }),
                    )
                    .child(button("deny", "Deny", Choice::Deny))
                    .child(button("approve", "Approve", Choice::Approve));
                // Over an open dialog, the buttons are drawn above it, in their place.
                let buttons = if over_dialog {
                    deferred(buttons).with_priority(OVER_DIALOGS).into_any_element()
                } else {
                    buttons.into_any_element()
                };
                // Where the banner is drawn: drawn elsewhere than last time, it takes no click
                // for a moment.
                let placed = {
                    let view = view.clone();
                    canvas(
                        move |bounds, _, cx| {
                            let now = Self::now(cx);
                            let moved =
                                view.update(cx, |this, _| this.banners.placed(id, bounds.origin, now)).unwrap_or(false);
                            if moved {
                                cx.defer(move |cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.schedule(cx);
                                        cx.notify();
                                    });
                                });
                            }
                        },
                        |_, (), _, _| {},
                    )
                    .absolute()
                    .size_full()
                };
                h_flex()
                    .id(("approval", id as usize))
                    .relative()
                    .occlude()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .bg(theme.warning.opacity(0.15))
                    .border_b_1()
                    .border_color(theme.warning)
                    .when(!waiting, |d| d.opacity(0.5))
                    .child(placed)
                    .child(Icon::new(Lucide::Bot).size_4())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(format!("{} wants to run {}: ", a.agent, a.tool))
                            // Patches and manifests are shown whole: what is approved is what runs.
                            // What the call will change comes right under the request, a
                            // manifest's YAML after.
                            .child(
                                v_flex()
                                    .id(("approval-summary", id as usize))
                                    .max_h(px(300.))
                                    .overflow_y_scroll()
                                    .overflow_x_hidden()
                                    .gap_1()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .child(a.summary.clone())
                                    .when_some(a.preview.as_ref(), |d, p| d.child(crate::details::preview_block(p, cx)))
                                    .when_some(a.detail.clone(), ParentElement::child),
                            ),
                    )
                    .child(buttons)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .id("approval-banners")
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.banners.set_hovered(*hovered);
                this.schedule(cx);
            }))
            .children(rows)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::{point, px};

    use super::{ARM, Banners, LINGER, MAX_LINGER};

    const MS: Duration = Duration::from_millis(1);

    /// A banner that goes keeps its place (and its buttons off) for a moment: a click aimed
    /// at its Approve never reaches the page that would move up under it.
    #[test]
    fn a_banner_that_goes_keeps_its_place_a_moment_and_takes_no_click() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, "patch", t0);
        let t = t0 + ARM;
        assert!(b.takes_clicks(1, t));
        b.resolve(1, t);
        assert_eq!(b.shown().count(), 1, "still drawn, in its place");
        assert_eq!(b.waiting_count(), 0, "but no longer waiting");
        assert!(!b.takes_clicks(1, t + MS));
        assert_eq!(b.next_wake(t), Some(t + LINGER));
        assert!(!b.drop_due(t + LINGER - MS), "not before it lingered");
        assert_eq!(b.shown().count(), 1);
        assert!(b.drop_due(t + LINGER));
        assert_eq!(b.shown().count(), 0);
        assert_eq!(b.next_wake(t + LINGER), None);
    }

    /// While the pointer is over the banners, one that went stays, up to `MAX_LINGER`; when
    /// the pointer leaves, it goes at once if it has lingered.
    #[test]
    fn a_banner_that_goes_stays_while_the_pointer_is_over_it() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, "patch", t0);
        b.resolve(1, t0);
        b.set_hovered(true);
        assert!(!b.drop_due(t0 + LINGER));
        assert_eq!(b.next_wake(t0 + LINGER), Some(t0 + MAX_LINGER));
        b.set_hovered(false);
        assert_eq!(b.next_wake(t0 + LINGER * 2), Some(t0 + LINGER * 2), "due now");
        assert!(b.drop_due(t0 + LINGER * 2));
        // And never past the cap.
        b.push(2, "patch", t0);
        b.resolve(2, t0);
        b.set_hovered(true);
        assert!(b.drop_due(t0 + MAX_LINGER));
    }

    /// A click on a banner that just showed was aimed at what was there before it.
    #[test]
    fn a_new_banner_takes_clicks_only_after_a_moment() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, "patch", t0);
        assert!(!b.takes_clicks(1, t0));
        assert!(!b.takes_clicks(1, t0 + ARM - MS));
        assert!(b.takes_clicks(1, t0 + ARM));
        assert_eq!(b.next_wake(t0), Some(t0 + ARM));
        assert_eq!(b.next_wake(t0 + ARM), None);
    }

    /// When a banner is taken away, those under it move up into its place: a click aimed at
    /// the one that went must not approve the next one.
    #[test]
    fn banners_that_move_up_take_no_click_for_a_moment() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, "first", t0);
        b.push(2, "second", t0);
        b.push(3, "third", t0 + ARM);
        let t = t0 + ARM * 4;
        b.resolve(2, t);
        assert!(b.takes_clicks(1, t) && b.takes_clicks(3, t));
        let gone = t + LINGER;
        assert!(b.drop_due(gone));
        assert!(b.takes_clicks(1, gone), "the one above did not move");
        assert!(!b.takes_clicks(3, gone), "the one under moved up");
        assert!(b.takes_clicks(3, gone + ARM));
        // A new one under them moves nothing.
        b.push(4, "fourth", gone + ARM);
        assert!(b.takes_clicks(1, gone + ARM) && b.takes_clicks(3, gone + ARM));
        assert_eq!(b.waiting().copied().collect::<Vec<_>>(), ["first", "third", "fourth"]);
    }

    /// A banner drawn somewhere else than last time (its text grew, the window was
    /// resized) takes no click for a moment; drawn in the same place, it is left alone.
    #[test]
    fn a_banner_drawn_elsewhere_takes_no_click_for_a_moment() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, "patch", t0);
        let t = t0 + ARM;
        assert!(!b.placed(1, point(px(0.), px(40.)), t0), "the first time is not a move");
        assert!(!b.placed(1, point(px(0.), px(40.)), t));
        assert!(b.takes_clicks(1, t));
        assert!(b.placed(1, point(px(0.), px(52.)), t));
        assert!(!b.takes_clicks(1, t));
        assert_eq!(b.next_wake(t), Some(t + ARM));
        assert!(b.takes_clicks(1, t + ARM));
    }

    /// Resolving twice (here, then the hub's event) keeps the first time it went.
    #[test]
    fn resolving_again_does_not_extend_the_linger() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, (), t0);
        b.resolve(1, t0);
        b.resolve(1, t0 + LINGER);
        assert!(b.drop_due(t0 + LINGER));
        assert!(!b.drop_due(t0 + LINGER), "gone already");
    }
}

#[cfg(all(test, feature = "dev"))]
mod view_tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    // Not `gpui_kit::*`: its `test` macro would shadow the `#[test]` that `#[gpui_kit::test]` expands to.
    use gpui_kit::{AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, point, px};

    use super::{ARM, ApprovalStrip, LINGER};
    use crate::mcp::{Approval, Hub};

    fn approval(id: u64) -> Arc<Approval> {
        Arc::new(Approval {
            id,
            agent_id: "a".into(),
            agent: "Claude".into(),
            tool: "patch_resource".into(),
            summary: format!("request {id}"),
            detail: None,
            preview: None,
            access: crate::mcp::Access::default(),
            dry_run: false,
            once_only: false,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(120),
        })
    }

    /// The banners in a window, with the answers they send.
    fn strip(
        cx: &mut TestAppContext,
    ) -> (Entity<ApprovalStrip>, Rc<RefCell<Vec<(u64, bool)>>>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let answers = Rc::new(RefCell::new(Vec::new()));
        let sent = answers.clone();
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let strip = cx.new(|_| {
                let mut s = ApprovalStrip::new(Hub::for_test(Vec::new()));
                s.answer = Rc::new(move |_, id, yes| sent.borrow_mut().push((id, yes)));
                s
            });
            view = Some(strip.clone());
            gpui_kit::base::Root::new(strip, window, cx)
        });
        (view.expect("the window was built"), answers, cx)
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.update(|window, cx| drop(window.draw(cx)));
        let at = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).center();
        cx.simulate_click(at, Modifiers::default());
    }

    /// A click on Approve right as the banner shows was aimed at what was there before: it
    /// leaves the request waiting. After a moment, it answers.
    #[gpui_kit::test]
    fn approve_counts_only_once_the_banner_has_been_there_a_moment(cx: &mut TestAppContext) {
        let (view, answers, cx) = strip(cx);
        view.update(cx, |s, cx| s.requested(approval(1), cx));
        click(cx, "approve-1");
        assert!(answers.borrow().is_empty(), "a click as it showed answers nothing");
        cx.executor().advance_clock(ARM);
        cx.run_until_parked();
        click(cx, "approve-1");
        assert_eq!(*answers.borrow(), [(1, true)]);
    }

    /// The banner above goes: the one under moves up into its place, and a click there right
    /// then (aimed at the one that went) doesn't approve it.
    #[gpui_kit::test]
    fn a_banner_that_moved_up_takes_no_click_at_once(cx: &mut TestAppContext) {
        let (view, answers, cx) = strip(cx);
        view.update(cx, |s, cx| {
            s.requested(approval(1), cx);
            s.requested(approval(2), cx);
        });
        cx.executor().advance_clock(ARM);
        cx.run_until_parked();
        cx.update(|window, cx| drop(window.draw(cx)));
        let first = cx.debug_bounds("approve-1").expect("drawn").center();
        view.update(cx, |s, cx| s.resolved(1, cx));
        // The first one stays a moment, inert: a click on it answers nothing.
        click(cx, "approve-1");
        assert!(answers.borrow().is_empty(), "the banner that went answers nothing");
        // While the pointer is over the banners, the one that went stays; it leaves.
        cx.simulate_mouse_move(point(px(5.), px(2000.)), None, Modifiers::default());
        cx.executor().advance_clock(LINGER);
        cx.run_until_parked();
        cx.update(|window, cx| drop(window.draw(cx)));
        let second = cx.debug_bounds("approve-2").expect("drawn").center();
        assert_eq!(second, first, "the second banner moved up into the first one's place");
        cx.simulate_click(second, Modifiers::default());
        assert!(answers.borrow().is_empty(), "the banner that moved up takes no click at once");
        cx.executor().advance_clock(ARM);
        cx.run_until_parked();
        click(cx, "approve-2");
        assert_eq!(*answers.borrow(), [(2, true)]);
    }
}
