//! The title row of a page: an icon, the title, and a muted line saying what it shows.

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};
use gpui_kit::*;

/// `icon` (in `color`, else the text's), `title`, then `subtitle` muted. Buttons go after it:
/// `.child(div().flex_1()).child(…)`.
pub fn page_header(
    icon: IconName,
    color: Option<Hsla>,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let icon = Icon::new(icon).size_5();
    let icon = match color {
        Some(c) => icon.text_color(c),
        None => icon,
    };
    h_flex()
        .gap_3()
        .child(icon)
        .child(div().text_xl().font_weight(FontWeight::MEDIUM).child(title.into()))
        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(subtitle.into()))
}

/// The namespaces a page shows, as its header says it: "all namespaces" or "in a, b".
pub fn scope_text(namespaces: &[String]) -> String {
    match namespaces {
        [] => "all namespaces".to_string(),
        chosen => format!("in {}", chosen.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scope_text_names_the_namespaces() {
        assert_eq!(super::scope_text(&[]), "all namespaces");
        assert_eq!(super::scope_text(&["a".into(), "b".into()]), "in a, b");
    }
}
