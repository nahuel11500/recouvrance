//! Styled text the mouse can select and Ctrl+C copy: gpui-kit's `SelectableText`, with
//! highlights (colors, find matches). A drag selects across lines, copied in their `order`.

use std::ops::Range;

use gpui_kit::base::{TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

pub struct SelectableLine {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    order: u64,
}

impl SelectableLine {
    /// `text` as line `order`; `id` must stay the same from one frame to the next.
    pub fn new(
        id: impl Into<ElementId>,
        order: u64,
        text: impl Into<SharedString>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Self {
        let text = text.into();
        Self { id: id.into(), styled: StyledText::new(text.clone()).with_highlights(highlights), text, order }
    }
}

impl IntoElement for SelectableLine {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableLine {
    /// The line's selection participant, kept from frame to frame in the element's state.
    type RequestLayoutState = TextSelectionHandle;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, TextSelectionHandle) {
        let id = global_id.expect("a SelectableLine has an id");
        let handle = window.with_element_state(id, |kept: Option<TextSelectionHandle>, _| {
            let handle = kept.unwrap_or_else(|| TextSelectionHandle::new(self.text.clone(), cx));
            (handle.clone(), handle)
        });
        let (layout, ()) = self.styled.request_layout(global_id, inspector_id, window, cx);
        (layout, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut TextSelectionHandle,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled.prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox, bounds)
            .with_document_order(self.order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(handle, window, cx);
        handle.register(registration, window, cx);
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut TextSelectionHandle,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let before = TextSelection::selected_text(window, cx);
        let run = TextSelectionRun::new(self.text.clone(), layout.clone(), bounds).with_document_order(self.order);
        let projection = handle.update_runs(&[run], cx);
        if before != TextSelection::selected_text(window, cx) {
            window.refresh();
        }
        let color = cx.theme().selection;
        for range in projection.ranges().iter().flatten() {
            paint_selection(&layout, range.clone(), color, window);
        }
        self.styled.paint(global_id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// The selection's background behind `range`, over as many wrapped lines as it spans.
fn paint_selection(layout: &TextLayout, range: Range<usize>, color: Hsla, window: &mut Window) {
    let (Some(start), Some(end)) = (layout.position_for_index(range.start), layout.position_for_index(range.end))
    else {
        return;
    };
    let (bounds, line) = (layout.bounds(), layout.line_height());
    let mut quads = Vec::with_capacity(3);
    if start.y == end.y {
        quads.push(Bounds::from_corners(start, point(end.x, end.y + line)));
    } else {
        quads.push(Bounds::from_corners(start, point(bounds.right(), start.y + line)));
        if end.y > start.y + line {
            quads.push(Bounds::from_corners(point(bounds.left(), start.y + line), point(bounds.right(), end.y)));
        }
        quads.push(Bounds::from_corners(point(bounds.left(), end.y), point(end.x, end.y + line)));
    }
    for quad in quads {
        window.paint_quad(fill(quad, color));
    }
}

#[cfg(all(test, feature = "dev"))]
mod tests {
    use gpui_kit::base::{TextSelection, TextSelectionLayer};
    use gpui_kit::{
        Context, HighlightStyle, IntoElement, Modifiers, MouseButton, ParentElement as _, Render, Styled as _,
        TestAppContext, Window, div, point, px, red,
    };

    use super::SelectableLine;

    struct Lines;

    impl Render for Lines {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let line = |i: usize, text: &'static str| {
                let red = HighlightStyle { color: Some(red()), ..Default::default() };
                div().h(px(20.)).child(SelectableLine::new(("line", i), i as u64, text, vec![(0..2, red)]))
            };
            let texts = ["alpha one", "beta two"];
            div().size_full().child(TextSelectionLayer).child(
                div().w(px(300.)).h(px(200.)).child(
                    gpui_kit::uniform_list("lines", 2, move |range, _, _| {
                        range.map(|i| line(i, texts[i]).into_any_element()).collect()
                    })
                    .size_full(),
                ),
            )
        }
    }

    #[gpui_kit::test]
    fn a_drag_selects_across_lines(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (_, cx) = cx.add_window_view(|_, _| Lines);
        cx.update(|window, cx| drop(window.draw(cx)));
        cx.simulate_mouse_down(point(px(1.), px(10.)), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(point(px(290.), px(30.)), Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(point(px(290.), px(30.)), MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            drop(window.draw(cx));
            assert_eq!(TextSelection::selected_text(window, cx), "alpha one\nbeta two");
        });
    }
}
