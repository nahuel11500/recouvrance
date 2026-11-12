//! Theme, accent color and zoom, saved between runs.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, WindowExt as _};
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
    let c = |hex: u32| -> Hsla { rgb(hex).into() };
    // "Harbour", after the schooner Recouvrance: a deep navy hull for the sidebar, sail-canvas
    // pages, brass fittings for selection and primary actions, and sea and signal colors for
    // status.
    let t = &mut theme.colors;
    if mode.is_dark() {
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
        t.success = c(0x5cb88f);
        t.warning = c(0xe3a23b);
        t.danger = c(0xe06c62);
    } else {
        t.background = c(0xfbf8f2);
        t.foreground = c(0x1b2533);
        t.muted_foreground = c(0x6b7280);
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
        t.success = c(0x3e8e6a);
        t.warning = c(0xc27c1e);
        t.danger = c(0xc2453d);
    }
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
    // Not `super::*`: gpui's `test` macro, glob-imported there, would shadow the built-in one.
    use super::{STATIC_UI_FONTS, bold_capable_family};

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
