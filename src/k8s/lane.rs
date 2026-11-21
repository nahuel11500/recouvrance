//! Two lanes to the API servers: the page on screen first, then background work (the
//! sidebar's problem count, a list that is hidden). Background work holds at most a few of
//! the slots it shares with the page, so the page never waits behind it.

use std::future::Future;

use anyhow::Result;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Which lane work goes in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lane {
    /// What the page on screen waits for.
    #[default]
    Foreground,
    /// What nobody is looking at: the problem count, a hidden list.
    Background,
}

tokio::task_local! {
    /// The lane of the work running, when `background` set one.
    static LANE: Lane;
}

impl Lane {
    /// The lane of the work running now: foreground unless inside `background`.
    pub fn current() -> Self {
        LANE.try_with(|lane| *lane).unwrap_or_default()
    }
}

/// Run `work` in the background lane, and the tasks it starts with `k8s::run`.
pub async fn background<F: Future>(work: F) -> F::Output {
    LANE.scope(Lane::Background, work).await
}

/// `work` in the lane of the work running now, for a task it starts: a task-local doesn't
/// follow a spawned task on its own.
pub(super) fn carry<F: Future>(work: F) -> impl Future<Output = F::Output> {
    LANE.scope(Lane::current(), work)
}

/// Requests at once, shared by the two lanes. Background work first takes a slot of its
/// own lane, so it holds `background` of the `all` slots at most: the page on screen
/// always has the others.
pub(super) struct Slots {
    all: Semaphore,
    background: Semaphore,
}

/// A slot taken, given back when dropped.
pub(super) struct Slot<'a> {
    _all: SemaphorePermit<'a>,
    _background: Option<SemaphorePermit<'a>>,
}

impl Slots {
    pub(super) const fn new(all: usize, background: usize) -> Self {
        Self { all: Semaphore::const_new(all), background: Semaphore::const_new(background) }
    }

    /// Wait for a slot in `lane`.
    pub(super) async fn acquire(&self, lane: Lane) -> Result<Slot<'_>> {
        let background = match lane {
            Lane::Background => Some(self.background.acquire().await?),
            Lane::Foreground => None,
        };
        Ok(Slot { _all: self.all.acquire().await?, _background: background })
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt as _;
    use proptest::prelude::*;

    use super::{Lane, Slot, Slots, background};

    /// A slot if one is free now; None when it would wait.
    fn take(slots: &Slots, lane: Lane) -> Option<Slot<'_>> {
        slots.acquire(lane).now_or_never().map(|slot| slot.expect("the slots are never closed"))
    }

    #[test]
    fn background_work_leaves_the_page_its_slots() {
        let slots = Slots::new(4, 2);
        let counting: Vec<Slot<'_>> = (0..2).filter_map(|_| take(&slots, Lane::Background)).collect();
        assert_eq!(counting.len(), 2);
        assert!(take(&slots, Lane::Background).is_none(), "the background lane is full");
        let page: Vec<Slot<'_>> = (0..2).filter_map(|_| take(&slots, Lane::Foreground)).collect();
        assert_eq!(page.len(), 2, "the others are the page's");
        assert!(take(&slots, Lane::Foreground).is_none(), "every slot is taken");
        drop(counting);
        // Without background work, the page can take every slot.
        let more: Vec<Slot<'_>> = (0..2).filter_map(|_| take(&slots, Lane::Foreground)).collect();
        assert_eq!(more.len(), 2);
        assert!(take(&slots, Lane::Background).is_none(), "none is free");
        drop((page, more));
        assert!(take(&slots, Lane::Background).is_some());
    }

    #[test]
    fn the_lane_follows_work_onto_the_runtime() {
        let lane = || crate::k8s::run(async { Ok(Lane::current()) });
        assert_eq!(futures::executor::block_on(lane()).unwrap(), Lane::Foreground);
        assert_eq!(futures::executor::block_on(background(lane())).unwrap(), Lane::Background);
        assert_eq!(Lane::current(), Lane::Foreground, "only inside `background`");
    }

    proptest! {
        /// Whatever is taken and given back, in any order: background work never holds more
        /// than its lane, and the page gets a slot whenever one is free.
        #[test]
        fn the_lanes_share_the_slots(ops in prop::collection::vec((any::<bool>(), any::<bool>(), 0..16usize), 0..64)) {
            const ALL: usize = 6;
            const BACKGROUND: usize = 2;
            let slots = Slots::new(ALL, BACKGROUND);
            let mut held: Vec<(Lane, Slot<'_>)> = Vec::new();
            for (taking, in_background, which) in ops {
                if taking {
                    let lane = if in_background { Lane::Background } else { Lane::Foreground };
                    let busy = held.iter().filter(|(l, _)| *l == Lane::Background).count();
                    let free = held.len() < ALL && (lane == Lane::Foreground || busy < BACKGROUND);
                    let slot = take(&slots, lane);
                    // Positional: `prop_assert_eq!` puts the message after its own with `concat!`.
                    prop_assert_eq!(slot.is_some(), free, "{:?} with {} taken, {} in the background", lane, held.len(), busy);
                    held.extend(slot.map(|s| (lane, s)));
                } else if !held.is_empty() {
                    drop(held.remove(which % held.len()));
                }
            }
        }
    }
}
