//! Where each context of the set being viewed stands while it can't show rows: still
//! connecting, or failed with a short reason. The list shows these instead of an empty table
//! ("No resources" said nothing about a login waiting or a cluster down), and the context
//! picker and the Contexts page mark them.

use gpui_kit::SharedString;

/// A context of the set that has no rows to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    /// Its connect, discovery or login has not answered yet.
    Connecting,
    /// It failed: why, said without the context's name.
    Failed(SharedString),
}

/// The contexts of the set that are not connected, in the set's order. A context that
/// answered is not in it: what isn't here is connected (or not in the set).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnStates(Vec<(String, Conn)>);

impl ConnStates {
    /// A connect to `contexts` starts: each is connecting.
    pub fn connecting(contexts: &[String]) -> Self {
        let mut seen = std::collections::HashSet::new();
        Self((contexts.iter()).filter(|c| seen.insert(c.as_str())).map(|c| (c.clone(), Conn::Connecting)).collect())
    }

    /// `ctx` answered, or failed with `failure`. A context not in the set is ignored: the
    /// result of a connect the user replaced since.
    pub fn settle(&mut self, ctx: &str, failure: Option<&str>) {
        match failure {
            None => self.0.retain(|(c, _)| c != ctx),
            Some(why) => {
                if let Some((_, state)) = self.0.iter_mut().find(|(c, _)| c == ctx) {
                    *state = Conn::Failed(SharedString::from(why.to_string()));
                }
            }
        }
    }

    /// The connect gave up with nothing to show: each context of `set` that has no reason of
    /// its own yet fails with `why`, in the set's order.
    pub fn give_up(&mut self, set: &[String], why: &str) {
        let mut seen = std::collections::HashSet::new();
        let states = (set.iter())
            .filter(|c| seen.insert(c.as_str()))
            .map(|c| match self.get(c) {
                Some(Conn::Failed(r)) => (c.clone(), Conn::Failed(r.clone())),
                _ => (c.clone(), Conn::Failed(SharedString::from(why.to_string()))),
            })
            .collect();
        self.0 = states;
    }

    pub fn get(&self, ctx: &str) -> Option<&Conn> {
        self.0.iter().find(|(c, _)| c == ctx).map(|(_, s)| s)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, Conn)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether one is still connecting.
    pub fn any_connecting(&self) -> bool {
        self.0.iter().any(|(_, s)| *s == Conn::Connecting)
    }
}

/// `error` without the `ctx: ` its connect put in front of it.
pub(super) fn without_name<'a>(ctx: &str, error: &'a str) -> &'a str {
    error.strip_prefix(ctx).and_then(|rest| rest.strip_prefix(": ")).unwrap_or(error)
}

/// What the bar above the list still says about the connect once each context's state is
/// shown on its own (`states`): the errors that are not a failed context's own (a namespace
/// list that failed on a cluster that answered).
pub(super) fn notes(errors: &[String], states: &ConnStates) -> Vec<String> {
    let own = |e: &String| {
        states.iter().any(|(c, s)| {
            matches!(s, Conn::Failed(_)) && e.strip_prefix(c.as_str()).is_some_and(|r| r.starts_with(": "))
        })
    };
    errors.iter().filter(|e| !own(e)).cloned().collect()
}

/// `notes` as one line, each one's context (`ctx: ` in front, the longest of `contexts` that
/// fits: a name can start another) named by `name`: its short name on screen, as the other
/// per-context lines say it, the full one in the hover.
pub(super) fn notes_line(notes: &[String], contexts: &[&str], name: &dyn Fn(&str) -> String) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let mut contexts = contexts.to_vec();
    contexts.sort_by_key(|c| std::cmp::Reverse(c.len()));
    let named = |note: &String| {
        let ctx = contexts.iter().find(|c| note.strip_prefix(**c).is_some_and(|r| r.starts_with(": ")));
        match ctx {
            Some(c) => format!("{}{}", name(c), &note[c.len()..]),
            None => note.clone(),
        }
    };
    Some(notes.iter().map(named).collect::<Vec<_>>().join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn set(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn every_context_connects_until_it_answers_or_fails() {
        let all = set(&["a", "b", "c"]);
        let mut states = ConnStates::connecting(&all);
        assert!(states.any_connecting());
        states.settle("a", None);
        states.settle("b", Some("credentials rejected"));
        assert_eq!(states.get("a"), None, "a connected: nothing to say");
        assert_eq!(states.get("b"), Some(&Conn::Failed("credentials rejected".into())));
        assert_eq!(states.get("c"), Some(&Conn::Connecting));
        states.settle("c", None);
        assert!(!states.any_connecting());
        // A context of another connect changes nothing.
        states.settle("z", Some("gone"));
        assert_eq!(states.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    /// The default: one context, it answers. Nothing is left to show over the list.
    #[test]
    fn one_context_that_answers_leaves_nothing() {
        let mut states = ConnStates::connecting(&set(&["only"]));
        states.settle("only", None);
        assert!(states.is_empty());
    }

    #[test]
    fn giving_up_keeps_each_reason_and_fills_the_others() {
        let all = set(&["a", "b"]);
        let mut states = ConnStates::connecting(&all);
        states.settle("a", Some("unreachable"));
        states.settle("b", None);
        states.give_up(&all, "no API served");
        assert_eq!(
            states.iter().cloned().collect::<Vec<_>>(),
            [
                ("a".to_string(), Conn::Failed("unreachable".into())),
                ("b".to_string(), Conn::Failed("no API served".into()))
            ]
        );
    }

    #[test]
    fn the_name_in_front_of_an_error_goes() {
        assert_eq!(without_name("arn:aws:eks:x", "arn:aws:eks:x: connection refused"), "connection refused");
        assert_eq!(without_name("a", "ab: refused"), "ab: refused");
        assert_eq!(without_name("a", "refused"), "refused");
    }

    #[test]
    fn the_bar_keeps_only_what_no_context_says() {
        let mut states = ConnStates::connecting(&set(&["a", "ab"]));
        states.settle("a", Some("refused"));
        states.settle("ab", None);
        let errors = set(&["a: refused", "ab: listing namespaces: timeout"]);
        assert_eq!(notes(&errors, &states), ["ab: listing namespaces: timeout"]);
        assert!(notes(&errors[..1], &states).is_empty());
    }

    /// The bar names a context as the other per-context lines do (its short name), and only
    /// in front of its own note: the longest name that fits, never one inside the text.
    #[test]
    fn the_bar_names_contexts_by_their_short_name() {
        let eks = "arn:aws:eks:eu-west-1:1:cluster/shop";
        let short = |c: &str| if c == eks { "shop".to_string() } else { c.to_uppercase() };
        let notes =
            set(&[format!("{eks}: listing namespaces: timeout").as_str(), "ab: x: a: y", "kubeconfig unreadable"]);
        let line = notes_line(&notes, &["a", "ab", eks], &short);
        assert_eq!(line.as_deref(), Some("shop: listing namespaces: timeout; AB: x: a: y; kubeconfig unreadable"));
        let full = notes_line(&notes, &["a", "ab", eks], &|c| c.to_string());
        assert_eq!(full, Some(notes.join("; ")));
        assert_eq!(notes_line(&[], &["a"], &short), None);
    }

    proptest! {
        /// Whatever the order the contexts answer or fail in: only those of the set that did
        /// not answer are shown, once each, in the set's order, and a failed one keeps its
        /// reason.
        #[test]
        fn only_the_set_that_did_not_answer_shows(
            names in proptest::collection::vec(0u8..8, 0..8),
            events in proptest::collection::vec((0u8..10, proptest::option::of(0u8..3)), 0..20),
        ) {
            let all: Vec<String> = names.iter().map(|n| format!("c{n}")).collect();
            let mut states = ConnStates::connecting(&all);
            let mut answered = std::collections::HashSet::new();
            let mut last = std::collections::HashMap::new();
            for (ctx, failure) in &events {
                let ctx = format!("c{ctx}");
                let why = failure.map(|f| format!("reason {f}"));
                states.settle(&ctx, why.as_deref());
                match why {
                    None => { answered.insert(ctx.clone()); }
                    Some(w) if !answered.contains(&ctx) => { last.insert(ctx.clone(), w); }
                    Some(_) => {}
                }
            }
            let shown: Vec<&String> = states.iter().map(|(c, _)| c).collect();
            let mut seen = std::collections::HashSet::new();
            let expected: Vec<&String> =
                all.iter().filter(|c| !answered.contains(*c) && seen.insert(c.as_str())).collect();
            prop_assert_eq!(shown, expected);
            for (c, s) in states.iter() {
                let expected = last.get(c).map_or(Conn::Connecting, |w| Conn::Failed(w.clone().into()));
                prop_assert_eq!(s, &expected);
            }
        }
    }
}
