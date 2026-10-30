//! Text the mouse can select and Ctrl+C copy, on gpui-kit's window-wide text selection.
//!
//! - [`SelectableLine`]: styled lines with highlights (colors, find matches), as in the logs.
//!   A drag selects across lines, copied in their `order`.
//! - [`selectable`]: plain text on a page (a detail page's fields, lists, conditions). A drag
//!   from one to another selects both and every text drawn between them, copied in the order
//!   they are drawn, like a web page.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::base::{
    TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, TextSelectionSnapshot,
};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
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

/// Plain text the mouse can select along with the rest of the page; see [`selectable`].
pub struct Selectable {
    text: SharedString,
    styled: StyledText,
}

/// `text`, drawn as a plain string child would be (it takes the font, color, wrapping and
/// ellipsis of its parent), that a drag selects and Ctrl+C copies.
///
/// It needs no id: a text keeps its selection from one frame to the next by its content and
/// how many times the same text came before it in the window. Its place in the selection
/// is the order it is drawn in, so a drag from a label to a value further down selects the
/// texts between them, as on a web page. Text cut short with an ellipsis is selected, and
/// copied, whole. Text off screen takes no part unless it is selected, so a long page costs
/// the selection only what is on screen.
pub fn selectable(text: impl Into<SharedString>) -> Selectable {
    let text = text.into();
    Selectable { styled: StyledText::new(text.clone()), text }
}

/// A right-click menu with "Copy" for the text selected in the window (disabled when there
/// is none), for `context_menu` on a page of selectable text.
pub fn copy_menu(menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let text = TextSelection::selected_text(window, cx).trim().to_string();
    let none = text.is_empty();
    menu.item(
        PopupMenuItem::new("Copy")
            .icon(Lucide::Copy)
            .disabled(none)
            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))),
    )
}

/// Whether a click on something that holds selectable text (a link) ended a drag that
/// selected text: then it is not a click.
pub fn dragged(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            let (down, up) = (click.down.position, click.up.position);
            (up.x - down.x).abs() > px(3.) || (up.y - down.y).abs() > px(3.)
        }
        _ => false,
    }
}

impl IntoElement for Selectable {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Selectable {
    type RequestLayoutState = ();
    /// The text's participant and its place in the window's order; None when it takes no
    /// part this frame (off screen and not selected).
    type PrepaintState = Option<(Participant, u64)>;

    fn id(&self) -> Option<ElementId> {
        None
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
    ) -> (LayoutId, ()) {
        self.styled.request_layout(global_id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<(Participant, u64)> {
        self.styled.prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let (participant, order) = participant(&self.text, window, cx);
        // Every participant is looked at on each change of the selection, for each other
        // participant: text off screen stays out unless it is selected (a drag that
        // scrolled past it), so pages with hundreds of rows cost what is on screen.
        if !window.content_mask().bounds.intersects(&bounds) && participant.handle.snapshot(cx).is_none() {
            return None;
        }
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration =
            TextSelectionRegistration::new(hitbox, bounds).with_document_order(order).with_text_bounds(vec![bounds]);
        participant.handle.register(registration, window, cx);
        Some((participant, order))
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        state: &mut Option<(Participant, u64)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((participant, order)) = state {
            participant.paint(&self.text, self.styled.layout(), bounds, *order, window, cx);
        }
        self.styled.paint(global_id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// A selectable text's part in the window's selection, and what it copies.
#[derive(Clone)]
pub struct Participant {
    handle: TextSelectionHandle,
    /// What the text copies: the part of it that is selected, worked out as it is painted.
    copied: Rc<RefCell<String>>,
}

impl Participant {
    fn new(cx: &mut App) -> Self {
        let handle = TextSelectionHandle::new(String::new(), cx);
        let copied = Rc::new(RefCell::new(String::new()));
        let text = copied.clone();
        handle.copy_with(move |_| text.borrow().clone(), cx);
        Self { handle, copied }
    }

    /// Paint the selection behind `text` and note what it copies.
    fn paint(
        &self,
        text: &SharedString,
        layout: &TextLayout,
        bounds: Bounds<Pixels>,
        order: u64,
        window: &mut Window,
        cx: &mut App,
    ) {
        let selection = self.handle.snapshot(cx);
        // Cut short with an ellipsis: what is drawn is not the text. Comparing the texts
        // allocates, so only while there is a selection to draw.
        let shortened = layout.len() != text.len() || (selection.is_some() && layout.text() != **text);
        let drawn: SharedString = if shortened && selection.is_some() { layout.text().into() } else { text.clone() };
        let run = TextSelectionRun::new(drawn, layout.clone(), bounds).with_document_order(order);
        let projection = self.handle.update_runs(&[run], cx);
        let color = cx.theme().selection;
        let mut copied = self.copied.borrow_mut();
        copied.clear();
        if shortened {
            if selection.is_some_and(|s| covers(&s, bounds)) {
                window.paint_quad(fill(bounds, color));
                copied.push_str(text);
            }
        } else {
            for range in projection.ranges().iter().flatten() {
                paint_selection(layout, range.clone(), color, window);
                copied.push_str(&text[range.clone()]);
            }
        }
    }
}

/// Whether a selection that reaches a text drawn at `bounds` covers some of it: the lines it
/// spans cross the text, and it is not a click (both ends at the same point). A participant
/// can be in a selection's order without being under it (another pane's text).
fn covers(selection: &TextSelectionSnapshot, bounds: Bounds<Pixels>) -> bool {
    let Some(points) = selection.window_points() else { return false };
    let (anchor, cursor) = (points.anchor(), points.cursor());
    anchor != cursor && bounds.bottom() > anchor.y.min(cursor.y) && bounds.top() <= anchor.y.max(cursor.y)
}

/// Each window's selectable texts, so that a text keeps its participant from one frame to
/// the next: by its content, then by how many times the same text came before it.
#[derive(Default)]
struct Texts(HashMap<WindowId, Frame>);

impl Global for Texts {}

#[derive(Default)]
struct Frame {
    /// The participants of the last frame, by text, in the order they were drawn.
    last: HashMap<SharedString, VecDeque<Participant>>,
    /// Those of the frame being drawn.
    this: HashMap<SharedString, VecDeque<Participant>>,
    /// The next text's place in the window's order.
    order: u64,
    /// A frame is being drawn: its end is already scheduled.
    open: bool,
}

/// `text`'s participant (last frame's, when it was drawn then) and its place in the order.
fn participant(text: &SharedString, window: &mut Window, cx: &mut App) -> (Participant, u64) {
    let id = window.window_handle().window_id();
    let frame = cx.default_global::<Texts>().0.entry(id).or_default();
    let started = !std::mem::replace(&mut frame.open, true);
    let order = frame.order;
    frame.order += 1;
    let kept = frame.last.get_mut(text).and_then(VecDeque::pop_front);
    if started {
        window.defer(cx, move |_, cx| end_frame(id, cx));
    }
    let participant = kept.unwrap_or_else(|| Participant::new(cx));
    let frame = cx.default_global::<Texts>().0.entry(id).or_default();
    frame.this.entry(text.clone()).or_default().push_back(participant.clone());
    (participant, order)
}

/// After a frame: what it drew is what the next one looks up; texts it did not draw are
/// dropped, and so are the windows that closed.
fn end_frame(id: WindowId, cx: &mut App) {
    let open: HashSet<WindowId> = cx.windows().iter().map(AnyWindowHandle::window_id).collect();
    let texts = cx.default_global::<Texts>();
    texts.0.retain(|window, _| open.contains(window));
    if let Some(frame) = texts.0.get_mut(&id) {
        frame.last = std::mem::take(&mut frame.this);
        frame.order = 0;
        frame.open = false;
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
        AppContext as _, ClickEvent, Context, FocusHandle, HighlightStyle, InteractiveElement as _, IntoElement,
        Modifiers, MouseButton, MouseClickEvent, MouseDownEvent, MouseUpEvent, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, point, px, red,
    };

    use super::{SelectableLine, dragged, selectable};

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

    /// Fields as a detail page draws them, a label above its value, then a value cut short.
    struct Fields(FocusHandle);

    impl Render for Fields {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let line = |text: &'static str| div().h(px(20.)).child(selectable(text));
            div()
                .track_focus(&self.0)
                .size_full()
                .flex()
                .flex_col()
                .child(line("Hostname"))
                .child(line("node-1"))
                .child(line("InternalIP"))
                .child(line("10.0.0.1"))
                .child(
                    div()
                        .w(px(80.))
                        .h(px(20.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(selectable("an-annotation-cut-short")),
                )
        }
    }

    /// The fields in a window as the app opens them (a Root, which copies on Ctrl+C), focused.
    fn fields(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(gpui_kit::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let fields = cx.new(|cx| Fields(cx.focus_handle()));
            view = Some(fields.clone());
            gpui_kit::base::Root::new(fields, window, cx)
        });
        let view = view.expect("the window was built");
        cx.update(|window, cx| {
            let focus = view.read(cx).0.clone();
            focus.focus(window, cx);
            drop(window.draw(cx));
        });
        cx
    }

    fn drag(cx: &mut VisualTestContext, from: (f32, f32), to: (f32, f32)) -> String {
        let (from, to) = (point(px(from.0), px(from.1)), point(px(to.0), px(to.1)));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            drop(window.draw(cx));
            TextSelection::selected_text(window, cx)
        })
    }

    #[gpui_kit::test]
    fn a_drag_over_a_value_selects_it_and_ctrl_c_copies_it(cx: &mut TestAppContext) {
        let cx = fields(cx);
        assert_eq!(drag(cx, (1., 30.), (190., 30.)), "node-1");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-c" } else { "ctrl-c" });
        assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("node-1"));
    }

    #[gpui_kit::test]
    fn a_drag_selects_the_fields_between_its_ends_in_order(cx: &mut TestAppContext) {
        let cx = fields(cx);
        assert_eq!(drag(cx, (1., 10.), (190., 70.)), "Hostname\nnode-1\nInternalIP\n10.0.0.1");
    }

    #[gpui_kit::test]
    fn text_cut_short_is_selected_whole(cx: &mut TestAppContext) {
        let cx = fields(cx);
        assert_eq!(drag(cx, (1., 90.), (70., 90.)), "an-annotation-cut-short");
    }

    #[gpui_kit::test]
    fn a_click_selects_nothing(cx: &mut TestAppContext) {
        let cx = fields(cx);
        assert_eq!(drag(cx, (7., 30.), (7., 30.)), "");
        assert_eq!(drag(cx, (7., 90.), (7., 90.)), "");
    }

    // TEMPORARY: measures what selectable text costs a frame; removed before review.
    struct Many(usize, bool);

    impl Render for Many {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (n, sel) = (self.0, self.1);
            div().size_full().child(TextSelectionLayer).flex().flex_wrap().children((0..n).map(move |i| {
                let cell = div().w(px(60.)).h(px(16.));
                if sel { cell.child(selectable(format!("t{i}"))) } else { cell.child(format!("t{i}")) }
            }))
        }
    }

    #[gpui_kit::test]
    fn zz_measure_cost(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut report = String::new();
        for n in [100usize, 300, 600] {
            for sel in [false, true] {
                let (_, vcx) = cx.add_window_view(move |_, _| Many(n, sel));
                let vcx: &mut VisualTestContext = vcx;
                vcx.update(|window, cx| drop(window.draw(cx)));
                let time = |vcx: &mut VisualTestContext| {
                    let start = std::time::Instant::now();
                    for _ in 0..10 {
                        vcx.update(|window, cx| {
                            window.refresh();
                            drop(window.draw(cx));
                        });
                    }
                    start.elapsed() / 10
                };
                let idle = time(vcx);
                vcx.simulate_mouse_down(point(px(1.), px(5.)), MouseButton::Left, Modifiers::default());
                vcx.simulate_mouse_move(point(px(500.), px(400.)), Some(MouseButton::Left), Modifiers::default());
                vcx.simulate_mouse_up(point(px(500.), px(400.)), MouseButton::Left, Modifiers::default());
                let selected = time(vcx);
                let chars = vcx.update(|window, cx| TextSelection::selected_text(window, cx).len());
                report.push_str(&format!(
                    "n={n} selectable={sel}: frame {idle:?}, with a selection {selected:?} ({chars} bytes)\n"
                ));
            }
        }
        panic!("MEASURE\n{report}");
    }

    #[test]
    fn a_click_is_not_a_drag() {
        let click = |from: f32, to: f32| {
            ClickEvent::Mouse(MouseClickEvent {
                down: MouseDownEvent { position: point(px(from), px(10.)), ..Default::default() },
                up: MouseUpEvent { position: point(px(to), px(10.)), ..Default::default() },
            })
        };
        assert!(!dragged(&click(10., 10.)));
        assert!(!dragged(&click(10., 12.)));
        assert!(dragged(&click(10., 40.)));
    }
}
