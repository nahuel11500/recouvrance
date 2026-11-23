//! Theme, accent color and zoom, saved between runs.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeColor, ThemeMode, WindowExt as _};
use gpui_kit::*;

/// Apply a theme mode and remember it as the user's choice (no longer following the desktop).
pub fn set_theme(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    apply_theme(mode, window, cx);
    save_theme_choice(mode);
    cx.set_global(FollowSystem(false));
}

/// Whether the theme follows the desktop: no Light or Dark choice is saved.
struct FollowSystem(bool);

impl Global for FollowSystem {}

pub fn follows_system(cx: &App) -> bool {
    cx.try_global::<FollowSystem>().is_some_and(|f| f.0)
}

/// The theme for a desktop appearance.
pub fn system_theme(appearance: WindowAppearance) -> ThemeMode {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
    }
}

/// Apply a theme mode with the app's typography on top, without saving it.
pub fn apply_theme(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(mode, window, cx);
    keep_bold_ui_font(cx);
    let z = zoom(cx);
    let theme = Theme::global_mut(cx);
    theme.font_size = px((13. * z).round());
    theme.mono_font_size = px((12. * z).round());
    let t = &mut theme.colors;
    paint(t, mode.is_dark());
    // Accent (brass by default) from Settings → Appearance: selection, primary buttons, focus.
    let accent = crate::settings::with(|s| s.accent.clone());
    let (_, dark, light) =
        crate::settings::ACCENTS.iter().find(|(n, ..)| *n == accent).copied().unwrap_or(crate::settings::ACCENTS[0]);
    let a = c(if mode.is_dark() { dark } else { light });
    t.list_active = a.opacity(if mode.is_dark() { 0.24 } else { 0.18 });
    t.table_active = t.list_active;
    t.ring = a;
    t.caret = a;
    t.selection = a.opacity(0.3);
    t.primary = a;
    t.primary_hover = a.opacity(0.9);
    t.primary_active = a.opacity(0.8);
    t.primary_foreground = c(if mode.is_dark() { 0x111c2e } else { 0x10233f });
    t.sidebar_primary = a;
    t.sidebar_primary_foreground = c(0x10233f);
    t.button_primary = t.primary;
    t.button_primary_hover = t.primary_hover;
    t.button_primary_active = t.primary_active;
    t.button_primary_foreground = t.primary_foreground;
    // Components read the derived tokens; rebuild them from the edited colors.
    theme.tokens = component::theme::ThemeTokens::from(&theme.colors);
    theme.radius = px(7.);
    theme.radius_lg = px(10.);
    cx.refresh_windows();
}

/// A `0xRRGGBB` color.
fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// The strongest tint of a status color that text of that color sits on: chips, a status
/// button under the pointer, a picked count on Problems. The tests check that the text reads.
pub const STATUS_FILL: f32 = 0.15;

/// Success, warning and danger colors.
#[derive(Clone, Copy)]
pub struct StatusColors {
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
}

/// The bright status colors, which read on navy: the dark theme's, and the sidebar's in both
/// themes, since it is navy in both. Dark text reads on them as fills (the sidebar's badges, a
/// find match in logs).
pub fn bright_status() -> StatusColors {
    StatusColors { success: c(0x5cb88f), warning: c(0xe3a23b), danger: c(0xe58178) }
}

/// The app's colors over gpui's defaults, the accent apart: "Harbour", after the schooner
/// Recouvrance. A deep navy hull for the sidebar, sail-canvas pages, brass fittings for
/// selection and primary actions, and sea and signal colors for status.
fn paint(t: &mut ThemeColor, dark: bool) {
    if dark {
        t.background = c(0x111c2e);
        t.foreground = c(0xe8e1d3);
        t.muted_foreground = c(0x8f9aad);
        t.border = c(0x22314a);
        t.secondary = c(0x16223a);
        t.secondary_foreground = c(0xe8e1d3);
        t.popover = c(0x121d31);
        t.input = c(0x2a3a55);
        t.title_bar = c(0x0b1322);
        t.title_bar_border = c(0x1c2a42);
        t.sidebar = c(0x0b1526);
        t.sidebar_foreground = c(0xe8e1d3);
        t.sidebar_border = c(0x1c2a42);
        t.sidebar_accent = c(0x1a2943);
        t.sidebar_accent_foreground = c(0xf4efe4);
        t.list_hover = c(0x16233b);
        t.table = c(0x111c2e);
        t.list = c(0x111c2e);
        t.tab_bar = c(0x111c2e);
        t.tab_active = c(0x16223a);
        t.accent = c(0x1a2943);
        t.accent_foreground = c(0xe8e1d3);
        t.muted = c(0x16223a);
        t.table_head = c(0x111c2e);
        t.table_hover = c(0x142037);
        t.table_even = c(0x131f33);
        t.table_row_border = c(0x1a2740);
        t.blue = c(0x8db8e8);
        t.link = c(0x8db8e8);
        let s = bright_status();
        (t.success, t.warning, t.danger) = (s.success, s.warning, s.danger);
    } else {
        t.background = c(0xfbf8f2);
        t.foreground = c(0x1b2533);
        // Also the color of what is done (Completed, Succeeded): 4.5:1 on chips and stripes.
        t.muted_foreground = c(0x585e69);
        t.border = c(0xe7dfcf);
        t.secondary = c(0xf2ece0);
        t.secondary_foreground = c(0x1b2533);
        t.popover = c(0xfffdf8);
        t.input = c(0xddd3bf);
        t.title_bar = c(0xf4eee3);
        t.title_bar_border = c(0xe7dfcf);
        t.sidebar = c(0x10233f);
        t.sidebar_foreground = c(0xe8e1d3);
        t.sidebar_border = c(0x0b1a30);
        t.sidebar_accent = c(0x1b3457);
        t.sidebar_accent_foreground = c(0xffffff);
        t.list_hover = c(0xf1eadc);
        t.table = c(0xfbf8f2);
        t.list = c(0xfbf8f2);
        t.tab_bar = c(0xfbf8f2);
        t.tab_active = c(0xfffdf8);
        t.accent = c(0xf1eadc);
        t.accent_foreground = c(0x1b2533);
        t.muted = c(0xf2ece0);
        t.table_head = c(0xfbf8f2);
        t.table_hover = c(0xf4eee2);
        t.table_even = c(0xf8f4ec);
        t.table_row_border = c(0xefe8da);
        t.blue = c(0x1f5f99);
        t.link = c(0x1f5f99);
        // Deep enough to read as text on every surface of the pages and on their own tints
        // (WCAG AA, 4.5:1): the bright ones read on navy only.
        t.success = c(0x2d684d);
        t.warning = c(0x874f0f);
        t.danger = c(0xa13a33);
    }
    // Text and icons on a status fill (gpui's tags, the window's close button on hover): the
    // page's color reads on the light theme's deep fills and on the dark theme's bright ones.
    t.success_foreground = t.background;
    t.warning_foreground = t.background;
    t.danger_foreground = t.background;
    // gpui derived its status buttons from its own palette when the theme loaded ("writes on"
    // was yellow on pale yellow, 1.5:1): derive them from these, on tints their text reads on.
    // Pressed goes a little further, for a moment.
    let button = |color: Hsla| (color.opacity(0.1), color.opacity(STATUS_FILL), color.opacity(0.2), color);
    (t.button_success, t.button_success_hover, t.button_success_active, t.button_success_foreground) =
        button(t.success);
    (t.button_warning, t.button_warning_hover, t.button_warning_active, t.button_warning_foreground) =
        button(t.warning);
    (t.button_danger, t.button_danger_hover, t.button_danger_active, t.button_danger_foreground) = button(t.danger);
}

/// Families with a bold face of their own, tried in order when the UI font has none.
const STATIC_UI_FONTS: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell", "Adwaita Sans"];

/// On Linux, gpui's text system (cosmic-text) loads a variable font as one face at its default
/// instance and never sets its `wght` axis, so every weight of a variable family is drawn regular.
/// The UI font is then never bold: with Ubuntu or Ubuntu Sans, nothing in the app is. When bold
/// resolves to the same face as regular, switch the theme to an installed family that has a bold
/// face. Other platforms have their own text systems and are left alone.
fn keep_bold_ui_font(cx: &mut App) {
    if !cfg!(any(target_os = "linux", target_os = "freebsd")) {
        return;
    }
    let current = cx.theme().font_family.clone();
    if draws_bold(&current, cx) {
        return;
    }
    let installed = cx.text_system().all_font_names();
    let Some(family) = bold_capable_family(&current, STATIC_UI_FONTS, &installed, |f| draws_bold(f, cx)) else {
        return;
    };
    let family = SharedString::from(family.to_string());
    Theme::update(cx, |theme| theme.font_family = family);
}

/// Whether bold text in `family` is drawn with another face than regular text.
fn draws_bold(family: &str, cx: &App) -> bool {
    let regular = font(SharedString::from(family.to_string()));
    let bold = Font { weight: FontWeight::BOLD, ..regular.clone() };
    let text = cx.text_system();
    text.resolve_font(&regular) != text.resolve_font(&bold)
}

/// The family to use instead of `current` so bold is drawn bold: `None` when `current` already
/// is, else the first installed candidate that is.
fn bold_capable_family<'a>(
    current: &str,
    candidates: &[&'a str],
    installed: &[String],
    draws_bold: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    if draws_bold(current) {
        return None;
    }
    candidates
        .iter()
        .copied()
        .filter(|c| *c != current && installed.iter().any(|name| name == c))
        .find(|c| draws_bold(c))
}

/// UI zoom factor (1.0 = 13px base text).
pub(super) struct Zoom(f32);

impl Global for Zoom {}

pub fn zoom(cx: &App) -> f32 {
    cx.try_global::<Zoom>().map_or(1.0, |z| z.0)
}

pub(super) fn zoom_path() -> std::path::PathBuf {
    ui_prefs_path().with_file_name("zoom")
}

/// Saved zoom, applied before the first theme.
pub fn load_zoom(cx: &mut App) {
    let z = std::fs::read_to_string(zoom_path()).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1.0f32);
    cx.set_global(Zoom(z.clamp(0.7, 2.0)));
}

#[cfg(feature = "dev")]
pub fn set_zoom_global(z: f32, cx: &mut App) {
    cx.set_global(Zoom(z));
}

pub fn set_zoom(z: f32, window: &mut Window, cx: &mut App) {
    let z = (z * 10.).round() / 10.;
    let z = z.clamp(0.7, 2.0);
    cx.set_global(Zoom(z));
    let _ = std::fs::write(zoom_path(), z.to_string());
    let mode = cx.theme().mode;
    apply_theme(mode, Some(window), cx);
    window.push_notification(Notification::info(format!("Zoom {:.0}%", z * 100.)), cx);
}

pub(super) fn ui_prefs_path() -> std::path::PathBuf {
    crate::paths::config_dir().join("theme")
}

/// Follow the desktop's appearance again.
pub fn forget_theme_choice() {
    let _ = std::fs::remove_file(ui_prefs_path());
}

pub(super) fn save_theme_choice(mode: ThemeMode) {
    let path = ui_prefs_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, if mode.is_dark() { "dark" } else { "light" });
}

/// The saved choice, else the system appearance (and then follow it while the app runs).
pub fn initial_theme(cx: &mut App) -> ThemeMode {
    let saved = match std::fs::read_to_string(ui_prefs_path()).as_deref().map(str::trim) {
        Ok("dark") => Some(ThemeMode::Dark),
        Ok("light") => Some(ThemeMode::Light),
        _ => None,
    };
    cx.set_global(FollowSystem(saved.is_none()));
    saved.unwrap_or_else(|| system_theme(cx.window_appearance()))
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::ThemeColor;
    use gpui_kit::{Hsla, Rgba};

    // Not `super::*`: gpui's `test` macro, glob-imported there, would shadow the built-in one.
    use super::{STATIC_UI_FONTS, STATUS_FILL, bold_capable_family, bright_status, paint};

    /// WCAG 2's relative luminance of an opaque color.
    fn luminance(color: Hsla) -> f32 {
        let c = Rgba::from(color);
        let linear = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }

    /// WCAG 2's contrast ratio of two opaque colors, from 1 to 21.
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn contrast_follows_wcag() {
        let (black, white) = (gpui_kit::black(), gpui_kit::white());
        assert!((contrast(black, white) - 21.).abs() < 0.01);
        assert!((contrast(white, white) - 1.).abs() < 0.001);
        // #767676 is the lightest gray that reads on white at 4.5:1.
        let gray: Hsla = gpui_kit::rgb(0x767676).into();
        assert!((contrast(gray, white) - 4.54).abs() < 0.01, "{}", contrast(gray, white));
    }

    /// Every status color against what it is drawn on, in both themes, at what WCAG AA asks of
    /// text this size: 4.5:1. Icons, dots and bars need 3:1, so they pass with the text.
    #[test]
    fn status_colors_read_at_wcag_aa() {
        let mut low = Vec::new();
        for dark in [false, true] {
            let mut t = ThemeColor::default();
            paint(&mut t, dark);
            let theme = if dark { "dark" } else { "light" };
            let mut need = |what: String, text: Hsla, under: Hsla| {
                let ratio = contrast(text, under);
                if ratio < 4.5 {
                    low.push(format!("{theme}: {what}: {ratio:.2}:1"));
                }
            };
            let surfaces = [
                ("the page", t.background),
                ("striped rows", t.table_even),
                ("a hovered row", t.table_hover),
                ("chips", t.secondary),
                ("muted panels", t.muted),
                ("popovers", t.popover),
                ("the active tab", t.tab_active),
                ("the title bar", t.title_bar),
                ("a hovered item", t.accent),
                ("a hovered list row", t.list_hover),
            ];
            // Where a tint of a status color carries text of that color: chips, status
            // buttons, counts.
            let tinted = [
                ("the page", t.background),
                ("striped rows", t.table_even),
                ("popovers", t.popover),
                ("the title bar", t.title_bar),
            ];
            let statuses =
                [("success", t.success), ("warning", t.warning), ("danger", t.danger), ("muted", t.muted_foreground)];
            for (status, color) in statuses {
                for (surface, under) in surfaces {
                    need(format!("{status} text on {surface}"), color, under);
                }
                for (surface, under) in tinted {
                    let tint = under.blend(color.opacity(STATUS_FILL));
                    need(format!("{status} text on its tint over {surface}"), color, tint);
                }
            }
            let buttons = [
                ("success", t.button_success_foreground, t.button_success, t.button_success_hover),
                ("warning", t.button_warning_foreground, t.button_warning, t.button_warning_hover),
                ("danger", t.button_danger_foreground, t.button_danger, t.button_danger_hover),
            ];
            for (status, text, fill, hovered) in buttons {
                for (surface, under) in tinted {
                    need(format!("a {status} button over {surface}"), text, under.blend(fill));
                    need(format!("a hovered {status} button over {surface}"), text, under.blend(hovered));
                }
            }
            let fills = [
                ("success", t.success_foreground, t.success),
                ("warning", t.warning_foreground, t.warning),
                ("danger", t.danger_foreground, t.danger),
            ];
            for (status, text, fill) in fills {
                need(format!("text on a {status} fill"), text, fill);
            }
            // The sidebar is navy in both themes: its status text, and its badges (the
            // sidebar's color on a bright fill), take the bright colors.
            let bright = bright_status();
            for (status, color) in [("success", bright.success), ("warning", bright.warning), ("danger", bright.danger)]
            {
                need(format!("{status} on the sidebar"), color, t.sidebar);
                need(format!("{status} on the sidebar's selected row"), color, t.sidebar_accent);
            }
        }
        assert!(low.is_empty(), "below WCAG AA (4.5:1):\n{}", low.join("\n"));
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// Ubuntu, Ubuntu Sans and Cantarell ship as variable fonts: bold resolves to their regular face.
    fn static_bold(family: &str) -> bool {
        !family.starts_with("Ubuntu") && family != "Cantarell"
    }

    #[test]
    fn keeps_a_family_that_draws_bold() {
        let installed = names(&["Noto Sans", "Ubuntu Sans"]);
        assert_eq!(bold_capable_family("Noto Sans", STATIC_UI_FONTS, &installed, static_bold), None);
    }

    #[test]
    fn replaces_a_variable_family_with_the_first_installed_static_one() {
        let installed = names(&["Ubuntu", "Ubuntu Sans", "DejaVu Sans", "Liberation Sans"]);
        assert_eq!(bold_capable_family("Ubuntu Sans", STATIC_UI_FONTS, &installed, static_bold), Some("DejaVu Sans"));
    }

    #[test]
    fn skips_candidates_that_are_variable_too() {
        let installed = names(&["Ubuntu", "Cantarell", "Adwaita Sans"]);
        assert_eq!(bold_capable_family("Ubuntu", STATIC_UI_FONTS, &installed, static_bold), Some("Adwaita Sans"));
    }

    #[test]
    fn keeps_the_family_when_no_candidate_draws_bold() {
        let installed = names(&["Ubuntu", "Ubuntu Sans", "Cantarell"]);
        assert_eq!(bold_capable_family("Ubuntu", STATIC_UI_FONTS, &installed, static_bold), None);
        assert_eq!(bold_capable_family("Ubuntu", STATIC_UI_FONTS, &[], static_bold), None);
    }

    #[test]
    fn never_checks_a_family_that_is_not_installed() {
        // A missing family resolves to gpui's fallback stack, which says nothing about its bold.
        let installed = names(&["Ubuntu", "Liberation Sans"]);
        let checked = std::cell::RefCell::new(Vec::new());
        let found = bold_capable_family("Ubuntu", STATIC_UI_FONTS, &installed, |f| {
            checked.borrow_mut().push(f.to_string());
            static_bold(f)
        });
        assert_eq!(found, Some("Liberation Sans"));
        assert_eq!(*checked.borrow(), ["Ubuntu", "Liberation Sans"]);
    }
}
