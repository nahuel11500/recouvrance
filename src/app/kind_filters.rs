//! Each resource type keeps its own filter: a pod name typed on Pods is not left in the box
//! on Nodes, and is back when Pods is shown again.

use std::collections::HashMap;

use gpui_kit::*;

use super::AppView;
use crate::k8s::Kind;

/// The filter typed for each kind (by `Kind::key`), and the kind the box shows one for.
#[derive(Default)]
pub(super) struct KindFilters {
    by_kind: HashMap<String, String>,
    shown_for: Option<String>,
}

impl KindFilters {
    /// The kind shown is now `to`, the box holding `typed`: the text to put in the box when
    /// the kind changed (`typed` kept for the kind it was typed on), None when it did not.
    fn switch(&mut self, typed: &str, to: Option<String>) -> Option<String> {
        if to == self.shown_for {
            return None;
        }
        if let Some(from) = self.shown_for.take() {
            self.by_kind.insert(from, typed.to_string());
        }
        let text = to.as_ref().and_then(|k| self.by_kind.get(k)).cloned().unwrap_or_default();
        self.shown_for = to;
        Some(text)
    }

    /// The box, holding `typed`, is given a filter for `kind` on purpose (a saved view, a
    /// tab, a link): `typed` is kept for the kind it was typed on, when that is another.
    fn shown(&mut self, typed: &str, kind: Option<String>) {
        if kind != self.shown_for
            && let Some(from) = self.shown_for.take()
        {
            self.by_kind.insert(from, typed.to_string());
        }
        self.shown_for = kind;
    }

    /// `kind`'s list opens unfiltered next time (a link to all of its objects).
    pub(super) fn forget(&mut self, kind: &Kind) {
        self.by_kind.remove(&kind.key());
    }
}

impl AppView {
    /// Put `text` in the filter box and the table, as the filter of the kind shown.
    pub(super) fn put_filter(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.filter.read(cx).value().to_string();
        self.kind_filters.shown(&typed, self.kind.as_ref().map(Kind::key));
        self.filter.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_filter(text);
            cx.notify();
        });
        // `set_value` sends no change event: move the highlight here.
        self.follow_selection(cx);
    }

    /// When the kind shown changed, keep the filter typed for the old one and show the new
    /// one's. Called on render: kinds change where no window is at hand.
    pub(super) fn sync_kind_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.filter.read(cx).value().to_string();
        if let Some(text) = self.kind_filters.switch(&typed, self.kind.as_ref().map(Kind::key)) {
            self.put_filter(text, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KindFilters;

    #[test]
    fn each_kind_keeps_its_filter() {
        let mut f = KindFilters::default();
        // The first kind shown starts empty.
        assert_eq!(f.switch("", Some("pods".into())).as_deref(), Some(""));
        // Same kind: the box is left alone.
        assert_eq!(f.switch("prometheus", Some("pods".into())), None);
        // Another kind: empty, the pods filter kept.
        assert_eq!(f.switch("prometheus", Some("nodes".into())).as_deref(), Some(""));
        assert_eq!(f.switch("gpu", Some("pods".into())).as_deref(), Some("prometheus"));
        assert_eq!(f.switch("prometheus", Some("nodes".into())).as_deref(), Some("gpu"));
        // A filter given on purpose for the kind shown now (a saved view of Services, opened
        // from Nodes): the Nodes one is kept, the given one is not taken for it.
        f.shown("gpu-2", Some("services".into()));
        assert_eq!(f.switch("web", Some("services".into())), None);
        assert_eq!(f.switch("web", Some("nodes".into())).as_deref(), Some("gpu-2"));
        assert_eq!(f.switch("", Some("services".into())).as_deref(), Some("web"));
        // Given on purpose for the same kind: nothing to keep.
        f.shown("ignored", Some("services".into()));
        assert_eq!(f.switch("api", Some("pods".into())).as_deref(), Some("prometheus"));
        assert_eq!(f.switch("", Some("services".into())).as_deref(), Some("api"));
    }

    #[test]
    fn a_kind_forgotten_opens_unfiltered() {
        let mut f = KindFilters::default();
        f.switch("", Some("pods".into()));
        f.switch("web", Some("nodes".into()));
        f.by_kind.remove("pods");
        assert_eq!(f.switch("", Some("pods".into())).as_deref(), Some(""));
    }
}
