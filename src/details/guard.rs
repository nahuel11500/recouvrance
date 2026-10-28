//! What a confirmation asks before its button works, as set in Settings → General: a name
//! typed (the object's, the count, a protected context's), or a short wait. With a wait, a
//! dialog that would ask for several names waits once.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::{AlertDialog, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The confirmation one dialog asks for, besides its own fields.
#[derive(Clone)]
pub enum Guard {
    /// A protected context's name, typed in `input`.
    Type { input: Entity<InputState>, context: String },
    /// A wait before the button works.
    Wait(Rc<Wait>),
}

/// A wait of some seconds, counted from when the dialog is first shown.
pub struct Wait {
    secs: u64,
    /// The protected contexts the dialog writes to, said in it (none for a delete that waits
    /// only because of its kind's setting).
    protected: Vec<String>,
    shown: Rc<Cell<Option<Instant>>>,
    /// Redraws the countdown each second; dropped, and so stopped, with the dialog.
    _tick: Option<Task<()>>,
}

impl Wait {
    #[cfg(test)]
    fn new(secs: u64, protected: Vec<String>) -> Self {
        Self { secs, protected, shown: Rc::default(), _tick: None }
    }

    /// Seconds left at `now`, starting the clock when it has not started.
    fn left_from(&self, now: Instant) -> u64 {
        if self.shown.get().is_none() {
            self.shown.set(Some(now));
        }
        left_of(self.shown.get(), self.secs, now).unwrap_or(self.secs)
    }

    /// Whether the wait is over at `now`. Never before it was shown.
    fn over_at(&self, now: Instant) -> bool {
        left_of(self.shown.get(), self.secs, now) == Some(0)
    }
}

/// Whole seconds left of a wait of `secs` shown at `shown` (rounded up, so the button says
/// 1 until it works), or none when it has not been shown.
fn left_of(shown: Option<Instant>, secs: u64, now: Instant) -> Option<u64> {
    let end = shown? + Duration::from_secs(secs);
    Some(end.saturating_duration_since(now).as_millis().div_ceil(1000) as u64)
}

/// Whether what was typed is the name asked for.
pub fn typed_matches(typed: &str, name: &str) -> bool {
    typed.trim() == name
}

impl Guard {
    /// On a protected context, what writing to it asks for: its name typed, or a wait.
    pub fn protected(context: &str, window: &mut Window, cx: &mut App) -> Option<Self> {
        crate::settings::is_protected(context).then(|| match crate::settings::confirm_wait() {
            Some(secs) => Self::wait(secs, vec![context.to_string()], cx),
            None => Self::typed(context, window, cx),
        })
    }

    fn typed(context: &str, window: &mut Window, cx: &mut App) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(context.to_string()));
        Self::Type { input, context: context.to_string() }
    }

    /// A wait of `secs`, whose countdown redraws each second while the guard lives.
    fn wait(secs: u64, protected: Vec<String>, cx: &mut App) -> Self {
        let shown: Rc<Cell<Option<Instant>>> = Rc::default();
        let clock = shown.clone();
        let tick = cx.spawn(async move |cx: &mut AsyncApp| {
            let mut last = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let left = left_of(clock.get(), secs, Instant::now());
                if left != last {
                    last = left;
                    cx.update(App::refresh_windows);
                }
                if left == Some(0) {
                    break;
                }
            }
        });
        Self::Wait(Rc::new(Wait { secs, protected, shown, _tick: Some(tick) }))
    }

    /// Whether the button may run now: the name typed, or the wait over.
    pub fn ok(&self, cx: &App) -> bool {
        match self {
            Self::Type { input, context } => typed_matches(&input.read(cx).value(), context),
            Self::Wait(w) => w.over_at(Instant::now()),
        }
    }

    /// Seconds before the button works (starting the wait when first asked), or none for a
    /// name to type.
    fn seconds_left(&self) -> Option<u64> {
        match self {
            Self::Type { .. } => None,
            Self::Wait(w) => Some(w.left_from(Instant::now())),
        }
    }

    /// What the dialog shows for it: the input for the name, or which contexts are protected.
    pub fn render(&self, cx: &App) -> AnyElement {
        let warning = cx.theme().warning;
        match self {
            Self::Type { input, context } => v_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(warning)
                        .child(format!("{context} is protected. Type its name to confirm:")),
                )
                .child(Input::new(input))
                .into_any_element(),
            Self::Wait(w) => match w.protected.as_slice() {
                [] => div().into_any_element(),
                [one] => div().text_sm().text_color(warning).child(format!("{one} is protected.")).into_any_element(),
                many => div()
                    .text_sm()
                    .text_color(warning)
                    .child(format!("{} are protected.", many.join(", ")))
                    .into_any_element(),
            },
        }
    }
}

/// What a dialog asks for: whether its own name (or count) is still typed, and its guards. A
/// dialog asks for the name when `name_typed` (its kind's delete setting), and for the name
/// of each `protected` context it writes to. With a wait, that is all one wait.
pub fn confirmation(name_typed: bool, protected: &[String], window: &mut Window, cx: &mut App) -> (bool, Vec<Guard>) {
    match crate::settings::confirm_wait() {
        None => (name_typed, protected.iter().map(|c| Guard::typed(c, window, cx)).collect()),
        Some(secs) => {
            let waits = name_typed || !protected.is_empty();
            (false, waits.then(|| Guard::wait(secs, protected.to_vec(), cx)).into_iter().collect())
        }
    }
}

/// Whether the guard, if any, lets the button run now.
pub fn guard_ok(guard: Option<&Guard>, cx: &App) -> bool {
    guard.is_none_or(|g| g.ok(cx))
}

/// The OK and Cancel buttons of a confirmation, with its guard: while it waits, the OK button
/// is disabled and says the seconds left ("Delete (3)").
pub trait ConfirmButtons {
    fn confirm_buttons(self, ok: impl Into<SharedString>, danger: bool, guard: Option<&Guard>) -> Self;
}

impl ConfirmButtons for AlertDialog {
    fn confirm_buttons(self, ok: impl Into<SharedString>, danger: bool, guard: Option<&Guard>) -> Self {
        let ok = ok.into();
        let alert = self.show_cancel(true).ok_text(ok.clone()).when(danger, |a| a.ok_variant(ButtonVariant::Danger));
        match guard.and_then(Guard::seconds_left).filter(|left| *left > 0) {
            None => alert,
            Some(left) => alert.footer(
                DialogFooter::new()
                    .child(
                        Button::new("confirm-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm-wait")
                            .label(format!("{ok} ({left})"))
                            .when(danger, ButtonVariants::danger)
                            .when(!danger, ButtonVariants::primary)
                            .disabled(true),
                    ),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Wait, left_of, typed_matches};
    use std::time::{Duration, Instant};

    #[test]
    fn a_wait_counts_down_from_when_it_is_shown() {
        let t0 = Instant::now();
        let s = Duration::from_millis;
        // Not shown yet: no count, and never over.
        assert_eq!(left_of(None, 3, t0), None);
        let w = Wait::new(3, vec![]);
        assert!(!w.over_at(t0 + Duration::from_secs(60)));
        // Shown: rounded up, so it says 1 until it is over.
        assert_eq!(w.left_from(t0), 3);
        assert_eq!(w.left_from(t0 + s(1)), 3);
        assert_eq!(w.left_from(t0 + s(1000)), 2);
        assert_eq!(w.left_from(t0 + s(2999)), 1);
        assert!(!w.over_at(t0 + s(2999)));
        assert_eq!(w.left_from(t0 + s(3000)), 0);
        assert!(w.over_at(t0 + s(3000)));
        assert!(w.over_at(t0 + s(9000)));
        // The clock starts once: asking again later does not restart it.
        assert_eq!(w.left_from(t0 + s(500)), 3);
        assert!(w.over_at(t0 + s(3000)));
    }

    #[test]
    fn a_typed_name_must_match() {
        assert!(typed_matches(" prod-eu ", "prod-eu"));
        assert!(!typed_matches("", "prod-eu"));
        assert!(!typed_matches("prod", "prod-eu"));
        assert!(!typed_matches("PROD-EU", "prod-eu"));
    }
}
