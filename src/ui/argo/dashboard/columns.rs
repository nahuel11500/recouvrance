//! A comparison of two states in two aligned columns: both sides named at the top, a line
//! removed in red with `−` on the left, a line added in green with `+` on the right, those that
//! replace each other on the same row, unchanged lines on both sides.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::history::DiffLine;
use crate::ui::selectable::selectable;

/// One side of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cell<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
    /// Nothing on this side: the other side's line is new, or gone.
    Empty,
    /// Unchanged lines left out.
    Gap,
}

/// The rows of `lines` side by side: removed lines paired with the added ones right after
/// them, row by row, the longer run padded with empty cells.
pub(super) fn rows(lines: &[DiffLine]) -> Vec<(Cell<'_>, Cell<'_>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match &lines[i] {
            DiffLine::Same(t) => {
                out.push((Cell::Same(t), Cell::Same(t)));
                i += 1;
            }
            DiffLine::Gap => {
                out.push((Cell::Gap, Cell::Gap));
                i += 1;
            }
            DiffLine::Removed(_) | DiffLine::Added(_) => {
                let mut removed = Vec::new();
                while let Some(DiffLine::Removed(t)) = lines.get(i) {
                    removed.push(t.as_str());
                    i += 1;
                }
                let mut added = Vec::new();
                while let Some(DiffLine::Added(t)) = lines.get(i) {
                    added.push(t.as_str());
                    i += 1;
                }
                for n in 0..removed.len().max(added.len()) {
                    let left = removed.get(n).map_or(Cell::Empty, |t| Cell::Removed(t));
                    let right = added.get(n).map_or(Cell::Empty, |t| Cell::Added(t));
                    out.push((left, right));
                }
            }
        }
    }
    out
}

/// `lines` in two columns, `names` (before, after) at their top.
pub(super) fn block(lines: &[DiffLine], names: (&str, &str), cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let cell = |c: Cell, cx: &App| {
        let theme = cx.theme();
        let (sign, bg, fg, text) = match c {
            Cell::Same(t) => (" ", None, theme.muted_foreground, t),
            Cell::Removed(t) => ("−", Some(theme.danger.opacity(0.12)), theme.danger, t),
            Cell::Added(t) => ("+", Some(theme.success.opacity(0.12)), theme.success, t),
            Cell::Empty => (" ", Some(theme.muted.opacity(0.5)), theme.muted_foreground, ""),
            Cell::Gap => ("", None, theme.muted_foreground, "⋯"),
        };
        h_flex()
            .flex_1()
            .flex_basis(px(0.))
            .min_w_0()
            .items_start()
            .px_2()
            .when_some(bg, Styled::bg)
            .text_color(fg)
            .child(div().flex_none().w(px(14.)).child(sign))
            .child(div().min_w_0().child(selectable(text.to_string())))
    };
    let heading = |text: &str| {
        div()
            .flex_1()
            .flex_basis(px(0.))
            .min_w_0()
            .px_2()
            .py_0p5()
            .truncate()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .child(text.to_string())
    };
    v_flex()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .child(
            h_flex()
                .items_stretch()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .child(heading(names.0))
                .child(div().flex_none().w(px(1.)).bg(theme.border))
                .child(heading(names.1)),
        )
        .children(rows(lines).into_iter().map(|(left, right)| {
            h_flex()
                .items_stretch()
                .child(cell(left, cx))
                .child(div().flex_none().w(px(1.)).bg(theme.border))
                .child(cell(right, cx))
        }))
}

#[cfg(test)]
mod tests {
    use super::{Cell, rows};
    use crate::history::DiffLine;

    fn same(t: &str) -> DiffLine {
        DiffLine::Same(t.into())
    }
    fn removed(t: &str) -> DiffLine {
        DiffLine::Removed(t.into())
    }
    fn added(t: &str) -> DiffLine {
        DiffLine::Added(t.into())
    }

    /// A change is one row; extra lines on one side face an empty cell; unchanged lines are on
    /// both sides.
    #[test]
    fn lines_are_aligned() {
        let lines = [same("a"), removed("b"), removed("c"), added("B"), same("d"), DiffLine::Gap, added("e")];
        assert_eq!(
            rows(&lines),
            [
                (Cell::Same("a"), Cell::Same("a")),
                (Cell::Removed("b"), Cell::Added("B")),
                (Cell::Removed("c"), Cell::Empty),
                (Cell::Same("d"), Cell::Same("d")),
                (Cell::Gap, Cell::Gap),
                (Cell::Empty, Cell::Added("e")),
            ]
        );
    }

    /// Every line shows once on its side, in its order.
    #[test]
    fn no_line_is_lost() {
        let lines = [added("x"), removed("y"), added("z"), same("s"), removed("w")];
        let rows = rows(&lines);
        let left: Vec<Cell> = rows.iter().map(|r| r.0).filter(|c| *c != Cell::Empty).collect();
        let right: Vec<Cell> = rows.iter().map(|r| r.1).filter(|c| *c != Cell::Empty).collect();
        assert_eq!(left, [Cell::Removed("y"), Cell::Same("s"), Cell::Removed("w")]);
        assert_eq!(right, [Cell::Added("x"), Cell::Added("z"), Cell::Same("s")]);
    }
}
