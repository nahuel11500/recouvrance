//! The container picker of a pod's terminal, files and logs.

use gpui_kit::component::IndexPath;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::*;

pub type ContainerSelect = Entity<SelectState<SearchableVec<SharedString>>>;

/// A picker of `names` with `current` selected, and its subscription calling `on_pick` with
/// the container chosen. None for a pod with one container: there is nothing to pick.
pub fn container_select<V: 'static>(
    names: &[String],
    current: Option<&str>,
    window: &mut Window,
    cx: &mut Context<V>,
    on_pick: impl Fn(&mut V, String, &mut Window, &mut Context<V>) + 'static,
) -> Option<(ContainerSelect, Subscription)> {
    if names.len() < 2 {
        return None;
    }
    let ix = current.and_then(|c| names.iter().position(|n| n == c));
    let items: Vec<SharedString> = names.iter().cloned().map(Into::into).collect();
    let select =
        cx.new(|cx| SelectState::new(SearchableVec::new(items), ix.map(|ix| IndexPath::default().row(ix)), window, cx));
    let sub =
        cx.subscribe_in(&select, window, move |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
            if let SelectEvent::Confirm(Some(name)) = ev {
                on_pick(this, name.to_string(), window, cx);
            }
        });
    Some((select, sub))
}
