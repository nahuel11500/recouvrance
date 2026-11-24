//! The approval banners at the top of the window, kept so a click never lands on something
//! that moved under the pointer.
//!
//! A banner that goes (answered here or elsewhere, refused, its agent gone) stays in place a
//! moment, its buttons off, before the page moves up into its space: a click aimed at its
//! Approve hits the inert banner, not the page's header (Save view, Delete...). A banner
//! that just showed, or one that just moved because one above it went, takes no click for
//! a moment either: the click was aimed at what was there before.

use std::time::{Duration, Instant};

/// How long a banner that went stays in place, inert.
pub const LINGER: Duration = Duration::from_millis(800);
/// How long a banner that just showed or moved ignores clicks.
pub const ARM: Duration = Duration::from_millis(500);

struct Banner<T> {
    id: u64,
    item: T,
    /// Clicks count from then on.
    armed_at: Instant,
    /// When it stopped waiting (it lingers until `gone_at + LINGER`).
    gone_at: Option<Instant>,
}

pub struct Banners<T> {
    list: Vec<Banner<T>>,
}

impl<T> Default for Banners<T> {
    fn default() -> Self {
        Self { list: Vec::new() }
    }
}

impl<T> Banners<T> {
    /// A new request: its banner goes under the others, which do not move.
    pub fn push(&mut self, id: u64, item: T, now: Instant) {
        self.list.push(Banner { id, item, armed_at: now + ARM, gone_at: None });
    }

    /// The request no longer waits: its banner stays, inert, for `LINGER`; then
    /// [`Banners::drop_gone`] takes it away.
    pub fn resolve(&mut self, id: u64, now: Instant) {
        if let Some(b) = self.list.iter_mut().find(|b| b.id == id && b.gone_at.is_none()) {
            b.gone_at = Some(now);
        }
    }

    /// Take away banner `id` once it has lingered. The banners under it move up into its
    /// space, so they ignore clicks for `ARM`. Returns whether it went.
    pub fn drop_gone(&mut self, id: u64, now: Instant) -> bool {
        let Some(i) = self.list.iter().position(|b| b.id == id && b.gone_at.is_some_and(|g| now >= g + LINGER)) else {
            return false;
        };
        self.list.remove(i);
        for b in &mut self.list[i..] {
            b.armed_at = b.armed_at.max(now + ARM);
        }
        true
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

    /// When the next banner that showed or moved takes clicks, to draw its buttons on then.
    pub fn next_armed(&self, now: Instant) -> Option<Instant> {
        self.list.iter().filter(|b| b.gone_at.is_none() && b.armed_at > now).map(|b| b.armed_at).min()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{ARM, Banners, LINGER};

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
        assert!(!b.drop_gone(1, t + LINGER - MS), "not before it lingered");
        assert_eq!(b.shown().count(), 1);
        assert!(b.drop_gone(1, t + LINGER));
        assert_eq!(b.shown().count(), 0);
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
        assert_eq!(b.next_armed(t0), Some(t0 + ARM));
        assert_eq!(b.next_armed(t0 + ARM), None);
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
        assert!(b.drop_gone(2, gone));
        assert!(b.takes_clicks(1, gone), "the one above did not move");
        assert!(!b.takes_clicks(3, gone), "the one under moved up");
        assert!(b.takes_clicks(3, gone + ARM));
        // A new one under them moves nothing.
        b.push(4, "fourth", gone + ARM);
        assert!(b.takes_clicks(1, gone + ARM) && b.takes_clicks(3, gone + ARM));
        assert_eq!(b.waiting().copied().collect::<Vec<_>>(), ["first", "third", "fourth"]);
    }

    /// Resolving twice (here, then the hub's event) keeps the first time it went.
    #[test]
    fn resolving_again_does_not_extend_the_linger() {
        let t0 = Instant::now();
        let mut b = Banners::default();
        b.push(1, (), t0);
        b.resolve(1, t0);
        b.resolve(1, t0 + LINGER);
        assert!(b.drop_gone(1, t0 + LINGER));
        assert!(!b.drop_gone(1, t0 + LINGER), "gone already");
    }
}
