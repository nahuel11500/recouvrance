//! What's new: release notes in a dialog, once after an update and from Settings → About.
//! They come with the binary (see `crate::updates`): nothing is fetched to show them.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::updates::{self, Notes};

/// After an update, the notes of the releases since the version run before, once, in
/// `window`: the next window to open finds them seen.
pub fn show_once(window: AnyWindowHandle, cx: &mut App) {
    let notes = updates::take_due();
    if !notes.is_empty() {
        let _ = window.update(cx, |_, window, cx| open(&notes, window, cx));
    }
}

/// The notes of the version running (Settings → About).
pub fn open_own(window: &mut Window, cx: &mut App) {
    if let Some(notes) = updates::own_notes() {
        open(std::slice::from_ref(notes), window, cx);
    }
}

/// What's new as it shows after an update from `last_seen` (`RECOUVRANCE_ACT=whats-new:0.7.0`).
#[cfg(feature = "dev")]
pub fn open_after(last_seen: &str, window: &mut Window, cx: &mut App) {
    open(&updates::notes_after(last_seen), window, cx);
}

fn open(notes: &[Notes], window: &mut Window, cx: &mut App) {
    let title: SharedString = format!("What's new in {}", updates::VERSION).into();
    let description: SharedString = match notes {
        [one] if !one.date.is_empty() => format!("Released on {}.", one.date).into(),
        [_] | [] => SharedString::default(),
        several => format!("The {} releases since the version you ran before.", several.len()).into(),
    };
    let text: SharedString = updates::markdown(notes).into();
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .title(title.clone())
            .when(!description.is_empty(), |alert| alert.description(description.clone()))
            .width(px(680.))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        TextView::markdown("whats-new", text.clone())
                            .selectable(true)
                            // The changelog's links lead to its pull requests on GitHub.
                            .on_link_click(|url, _, _, _| {
                                if url.starts_with("https://") {
                                    crate::forwards::open_url(url);
                                }
                            }),
                    )
                    .child(
                        div().child(
                            Button::new("whats-new-all")
                                .xsmall()
                                .ghost()
                                .icon(Lucide::ExternalLink)
                                .label("All release notes")
                                .on_click(|_, _, _| crate::forwards::open_url(updates::RELEASES)),
                        ),
                    ),
            )
            .ok_text("Close")
    });
}
