//! Two lanes to the API servers: the page on screen first, then background work (the
//! sidebar's problem count, a list that is hidden). Background work holds at most a few of
//! the slots it shares with the page, so the page never waits behind it.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::{Notify, Semaphore, SemaphorePermit};

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
    /// The lane of the work running, when a `Raisable` set one. The tasks
    /// it starts share it, so raising it moves them all.
    static LANE: Arc<State>;
}

/// A lane that can be raised to the foreground while its work runs.
#[derive(Debug)]
struct State {
    background: AtomicBool,
    /// Wakes the requests waiting for a background slot when the lane is raised.
    raised: Notify,
}

impl State {
    fn lane(&self) -> Lane {
        if self.background.load(Ordering::Acquire) { Lane::Background } else { Lane::Foreground }
    }
}

impl Lane {
    /// The lane of the work running now: foreground unless inside the `run` of a
    /// `Raisable` not raised yet.
    pub fn current() -> Self {
        LANE.try_with(|state| state.lane()).unwrap_or_default()
    }
}

/// The lane of some work, which can be raised to the foreground while it runs: a scan the
/// badge started in the background that the page on screen then waits for. The requests
/// sent after go in the foreground, and those waiting for a background slot stop waiting.
#[derive(Clone, Debug)]
pub struct Raisable(Arc<State>);

impl Raisable {
    pub fn new(lane: Lane) -> Self {
        Self(Arc::new(State { background: AtomicBool::new(lane == Lane::Background), raised: Notify::new() }))
    }

    /// Run `work` in this lane, and the tasks it starts with `k8s::run`.
    pub fn run<F: Future>(&self, work: F) -> impl Future<Output = F::Output> + use<F> {
        LANE.scope(self.0.clone(), work)
    }

    /// Move the work to the foreground, from now on.
    pub fn raise(&self) {
        if self.0.background.swap(false, Ordering::AcqRel) {
            self.0.raised.notify_waiters();
        }
    }

    #[cfg(test)]
    pub fn lane(&self) -> Lane {
        self.0.lane()
    }
}

/// `work` in the lane of the work running now, for a task it starts: a task-local doesn't
/// follow a spawned task on its own. The task shares the lane, so a raise reaches it too.
pub(super) fn carry<F: Future>(work: F) -> impl Future<Output = F::Output> {
    let state = LANE.try_with(Arc::clone).unwrap_or_else(|_| Raisable::new(Lane::Foreground).0);
    LANE.scope(state, work)
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
            Lane::Background => self.background_slot().await?,
            Lane::Foreground => None,
        };
        Ok(Slot { _all: self.all.acquire().await?, _background: background })
    }

    /// A slot of the background lane; none when the work's lane is raised meanwhile, so a
    /// request already waiting behind background work goes in the foreground.
    async fn background_slot(&self) -> Result<Option<SemaphorePermit<'_>>> {
        let state = match LANE.try_with(Arc::clone) {
            Ok(state) if state.lane() == Lane::Background => state,
            // Work in no raisable lane: a hidden list says background on its own.
            _ => return Ok(Some(self.background.acquire().await?)),
        };
        let raised = state.raised.notified();
        tokio::pin!(raised);
        // Listen before looking again, so a raise in between is not missed.
        raised.as_mut().enable();
        if state.lane() == Lane::Foreground {
            return Ok(None);
        }
        tokio::select! {
            biased;
            permit = self.background.acquire() => Ok(Some(permit?)),
            () = raised => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt as _;
    use proptest::prelude::*;

    use super::{Lane, Raisable, Slot, Slots};

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
    fn a_raised_lane_stops_waiting_for_background_slots() {
        let slots = Slots::new(4, 1);
        let lane = Raisable::new(Lane::Background);
        futures::executor::block_on(lane.run(async {
            let held = slots.acquire(Lane::current()).await.unwrap();
            let mut next = std::pin::pin!(slots.acquire(Lane::current()));
            assert!(futures::poll!(next.as_mut()).is_pending(), "the background lane is full");
            lane.raise();
            assert_eq!(Lane::current(), Lane::Foreground);
            let next = next.await.unwrap();
            assert!(next._background.is_none(), "the waiting request took a foreground slot");
            let more: Vec<Slot<'_>> = (0..2).filter_map(|_| take(&slots, Lane::current())).collect();
            assert_eq!(more.len(), 2, "and so do the requests after it");
            drop((held, next, more));
        }));
        assert_eq!(lane.lane(), Lane::Foreground);
    }

    #[test]
    fn a_raise_reaches_the_tasks_the_work_started() {
        let lane = Raisable::new(Lane::Background);
        let seen = futures::executor::block_on(lane.run(async {
            let before = crate::k8s::run(async { Ok(Lane::current()) }).await.unwrap();
            lane.raise();
            let after = crate::k8s::run(async { Ok(Lane::current()) }).await.unwrap();
            (before, after)
        }));
        assert_eq!(seen, (Lane::Background, Lane::Foreground));
    }

    #[test]
    fn the_lane_follows_work_onto_the_runtime() {
        let lane = || crate::k8s::run(async { Ok(Lane::current()) });
        assert_eq!(futures::executor::block_on(lane()).unwrap(), Lane::Foreground);
        let background = Raisable::new(Lane::Background);
        assert_eq!(futures::executor::block_on(background.run(lane())).unwrap(), Lane::Background);
        assert_eq!(Lane::current(), Lane::Foreground, "only inside `run`");
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
