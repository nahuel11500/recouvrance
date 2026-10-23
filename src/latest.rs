//! "The latest request wins" for a view's async work.
//!
//! A view that loads something for the current choice (a chart, a release, a range, a
//! search) keeps one `Latest` per kind of load. Starting a load replaces the one running,
//! which drops it: its result is never applied over the newer one. Unrelated loads get
//! their own `Latest`, so starting one never cancels another.

use std::future::Future;

use gpui_kit::*;

#[derive(Default)]
pub struct Latest(Option<Task<()>>);

impl Latest {
    /// Run `work`, then `apply` its result to the view, unless another load started since
    /// (or the view is gone).
    pub fn spawn<V: 'static, T: 'static>(
        &mut self,
        cx: &mut Context<V>,
        work: impl Future<Output = T> + 'static,
        apply: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
    ) {
        self.0 = Some(cx.spawn(async move |this, cx| {
            let out = work.await;
            let _ = this.update(cx, |view, cx| apply(view, out, cx));
        }));
    }

    /// The same, for results applied with the window (editors, notifications).
    pub fn spawn_in<V: 'static, T: 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<V>,
        work: impl Future<Output = T> + 'static,
        apply: impl FnOnce(&mut V, T, &mut Window, &mut Context<V>) + 'static,
    ) {
        self.0 = Some(cx.spawn_in(window, async move |this, cx| {
            let out = work.await;
            let _ = this.update_in(cx, |view, window, cx| apply(view, out, window, cx));
        }));
    }

    /// Drop the running load: its result will not be applied.
    pub fn cancel(&mut self) {
        self.0 = None;
    }
}

// View tests need the toolkit's test support, which the `dev` feature turns on.
#[cfg(all(test, feature = "dev"))]
mod tests {
    use super::Latest;
    use gpui_kit::{AppContext as _, Context, TestAppContext};

    #[derive(Default)]
    struct View {
        load: Latest,
        shown: Option<&'static str>,
    }

    impl View {
        fn load(&mut self, what: &'static str, delay_ms: u64, cx: &mut Context<Self>) {
            let timer = cx.background_executor().timer(std::time::Duration::from_millis(delay_ms));
            self.load.spawn(
                cx,
                async move {
                    timer.await;
                    what
                },
                |view, what, _| view.shown = Some(what),
            );
        }
    }

    /// A slow first load finishing after a fast second one must not overwrite it.
    #[gpui_kit::test]
    fn a_newer_load_wins_over_a_slower_older_one(cx: &mut TestAppContext) {
        let view = cx.new(|_| View::default());
        view.update(cx, |v, cx| v.load("old", 500, cx));
        view.update(cx, |v, cx| v.load("new", 10, cx));
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.shown), Some("new"));
    }
}
