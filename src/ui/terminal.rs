//! A terminal for `exec` into a container: vt100 emulation rendered with gpui.

use std::cell::Cell;
use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::ConfirmButtons as _;
use crate::k8s::{self, ExecInput, ExecOutput, TaskGuard};

const SCROLLBACK: usize = 5000;

pub struct TerminalView {
    /// A local shell for this context (and namespace) instead of a pod exec.
    local: Option<(String, Option<String>)>,
    /// A command to run instead of an interactive shell (plugins).
    command: Option<String>,
    /// Environment for the local command (the plugin's values).
    env: Vec<(String, String)>,
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    /// The pod's containers to choose from (default first, then debug ones), once it is read.
    containers: Vec<String>,
    /// Picks another container when the pod has several; a running session restarts in it.
    container_select: Option<Entity<SelectState<SearchableVec<SharedString>>>>,
    _container_sub: Option<Subscription>,
    /// Shown before connecting when the context looks like production/admin.
    risky: bool,
    /// Attach to the container's process instead of starting a shell.
    attach: bool,
    parser: vt100::Parser,
    input: Option<UnboundedSender<ExecInput>>,
    focus: FocusHandle,
    status: Option<SharedString>,
    size: (u16, u16),
    /// Scrolled part of a line not applied yet (trackpads scroll a few pixels at a time).
    scroll_rest: f32,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    session: Option<(TaskGuard, Task<()>)>,
    /// Exec and attach need write mode, and this context's name typed when it is protected.
    guard: Option<String>,
    /// The context name was typed once for this terminal (write mode is checked every time).
    authorized: bool,
    /// Its command ran once and can't be started again from here (a writing plugin).
    once: bool,
}

/// The session ended: the remote side exited or the user closed it.
pub struct SessionEnded;

impl EventEmitter<SessionEnded> for TerminalView {}

impl TerminalView {
    pub fn new(
        client: Client,
        ns: String,
        pod: String,
        container: Option<String>,
        risky: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            local: None,
            command: None,
            env: Vec::new(),
            client,
            ns,
            pod,
            container,
            containers: Vec::new(),
            container_select: None,
            _container_sub: None,
            risky,
            attach: false,
            parser: vt100::Parser::new(nz(24), nz(80), SCROLLBACK),
            input: None,
            focus: cx.focus_handle(),
            status: None,
            size: (80, 24),
            scroll_rest: 0.,
            bounds: Rc::default(),
            session: None,
            guard: None,
            authorized: false,
            once: false,
        }
    }

    /// Opening a shell or attaching (from its buttons) needs write mode, and on a
    /// protected `context` its name typed. Programmatic starts are already confirmed.
    pub fn guarded(mut self, context: String) -> Self {
        self.guard = Some(context);
        self
    }

    /// The context name was already typed in the dialog that made this terminal (node shell,
    /// debug container): don't ask for it again. Write mode is still checked on every start.
    pub fn preauthorized(mut self) -> Self {
        self.authorized = true;
        self
    }

    /// No Reconnect: the command runs once, and again only from where it was asked for.
    pub fn run_once(mut self) -> Self {
        self.once = true;
        self
    }

    /// The pod's containers as read, and its `default` one. With none chosen yet, the default
    /// one, as kubectl picks it: the API server refuses an exec naming no container in a pod
    /// that has several. A choice the pod still has stays.
    pub fn set_containers(
        &mut self,
        names: Vec<String>,
        default: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if names == self.containers {
            return;
        }
        let gone = self.container.as_ref().is_some_and(|c| !names.contains(c));
        if self.container.is_none() || (gone && self.session.is_none()) {
            self.container = default;
        }
        self.container_select = None;
        self._container_sub = None;
        if names.len() > 1 {
            let ix = self.container.as_ref().and_then(|c| names.iter().position(|n| n == c));
            let items: Vec<SharedString> = names.iter().cloned().map(Into::into).collect();
            let select = cx.new(|cx| {
                SelectState::new(SearchableVec::new(items), ix.map(|ix| IndexPath::default().row(ix)), window, cx)
            });
            self._container_sub = Some(cx.subscribe_in(
                &select,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(name)) = ev else { return };
                    this.choose_container(name.to_string(), window, cx);
                },
            ));
            self.container_select = Some(select);
        }
        self.containers = names;
        cx.notify();
    }

    /// Use `name` from now on; a running session ends and starts again in it, through the
    /// same guard as its button (write mode, the protected context's name).
    fn choose_container(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.container.as_deref() == Some(name.as_str()) {
            return;
        }
        let running = self.session.is_some();
        // The old container's session ends here, whether or not the new one may start.
        self.session = None;
        self.input = None;
        self.status = None;
        self.container = Some(name);
        if running {
            let attach = self.attach;
            self.request(attach, window, cx);
        }
        cx.notify();
    }

    /// A session is open and accepts input.
    pub fn connected(&self) -> bool {
        self.input.is_some()
    }

    /// Connect (or attach) from a button, after the guard when there is one.
    fn request(&mut self, attach: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.once {
            return;
        }
        let start = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            if attach {
                this.start_attach(window, cx);
            } else {
                this.connect(window, cx);
            }
        };
        let Some(context) = self.guard.clone() else {
            return start(self, window, cx);
        };
        // Write mode may have been turned off since the last session: check it every time.
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        if self.authorized || !crate::settings::is_protected(&context) {
            self.authorized = true;
            return start(self, window, cx);
        }
        let guard = crate::details::Guard::protected(&context, window, cx);
        let me = cx.entity().downgrade();
        let title = if attach { "Attach to this container?" } else { "Open a shell in this container?" };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (me, guard) = (me.clone(), guard.clone());
            alert
                .title(title)
                .description("Commands you type run in the container with its rights.")
                .child(crate::details::confirm_extras("", guard.as_ref(), cx))
                .confirm_buttons(if attach { "Attach" } else { "Open shell" }, false, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !crate::details::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !crate::details::writes_enabled(cx) {
                        return true;
                    }
                    let _ = me.update(cx, |this, cx| {
                        this.authorized = true;
                        start(this, window, cx);
                    });
                    true
                })
        });
    }

    /// A local shell with kubectl pointed at `context`; starts right away.
    pub fn local(
        client: Client,
        context: String,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(client, namespace.clone().unwrap_or_default(), String::new(), None, false, cx);
        this.local = Some((context, namespace));
        this.connect(window, cx);
        this
    }

    /// Run `command` on this machine with kubectl pointed at the context and `env` set (plugins).
    pub fn local_command(
        client: Client,
        context: String,
        namespace: Option<String>,
        command: String,
        env: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(client, namespace.clone().unwrap_or_default(), String::new(), None, false, cx);
        this.local = Some((context, namespace));
        this.command = Some(command);
        this.env = env;
        this.connect(window, cx);
        this
    }

    #[cfg(feature = "dev")]
    pub fn type_text(&self, text: &str) {
        self.send(text.as_bytes().to_vec());
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    /// End the session (the remote shell gets a hangup) and go back to the start screen.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.input = None;
        self.status = None;
        self.attach = false;
        cx.emit(SessionEnded);
        cx.notify();
    }

    pub fn start_attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attach = true;
        self.connect(window, cx);
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.parser = vt100::Parser::new(nz(self.size.1), nz(self.size.0), SCROLLBACK);
        self.status = Some("Connecting…".into());
        let k8s::Session { guard, input, mut output } = match &self.local {
            Some((ctx, ns)) => k8s::local_shell(ctx.clone(), ns.clone(), self.command.clone(), self.env.clone()),
            None if self.attach => {
                k8s::attach_container(self.client.clone(), self.ns.clone(), self.pod.clone(), self.container.clone())
            }
            None => k8s::exec_shell(self.client.clone(), self.ns.clone(), self.pod.clone(), self.container.clone()),
        };
        let _ = input.unbounded_send(ExecInput::Resize { cols: self.size.0, rows: self.size.1 });
        self.input = Some(input);
        let task = cx.spawn(async move |this, cx| {
            while let Some(msg) = output.next().await {
                let alive = this.update(cx, |this, cx| {
                    match msg {
                        ExecOutput::Data(bytes) => {
                            this.status = None;
                            this.parser.process(&bytes);
                        }
                        ExecOutput::Exit(reason) => {
                            this.status = Some(reason.into());
                            this.input = None;
                            cx.emit(SessionEnded);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self.session = Some((guard, task));
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Feed output without a session, to check rendering off-screen.
    #[cfg(feature = "dev")]
    pub fn demo(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let task = Task::ready(());
        let guard = k8s::Session::disabled().guard;
        self.session = Some((guard, task));
        self.parser.process(bytes);
        cx.notify();
    }

    fn send(&self, bytes: Vec<u8>) {
        if let Some(input) = &self.input {
            let _ = input.unbounded_send(ExecInput::Bytes(bytes));
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // Paste with ctrl-shift-v, like most Linux terminals.
        if ks.modifiers.control && ks.modifiers.shift && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.parser.screen_mut().set_scrollback(0);
                self.send(paste_bytes(&text, self.parser.screen().bracketed_paste()));
            }
            cx.stop_propagation();
            return;
        }
        let app_cursor = self.parser.screen().application_cursor();
        if let Some(bytes) = key_bytes(ks, app_cursor) {
            self.parser.screen_mut().set_scrollback(0);
            self.send(bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = match ev.delta {
            ScrollDelta::Lines(d) => d.y,
            ScrollDelta::Pixels(d) => f32::from(d.y) / 18.,
        };
        let lines = whole_lines(&mut self.scroll_rest, delta);
        let current = self.parser.screen().scrollback() as i32;
        let next = (current + lines).clamp(0, SCROLLBACK as i32) as usize;
        self.parser.screen_mut().set_scrollback(next);
        cx.notify();
    }

    /// Match the remote pty to the visible area (one frame behind layout).
    fn sync_size(&mut self, cell: Size<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds.get() else {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
            return;
        };
        let cols = (f32::from(bounds.size.width) / f32::from(cell.width)).floor().max(10.) as u16;
        let rows = (f32::from(bounds.size.height) / f32::from(cell.height)).floor().max(4.) as u16;
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.parser.screen_mut().set_size(nz(rows), nz(cols));
            if let Some(input) = &self.input {
                let _ = input.unbounded_send(ExecInput::Resize { cols, rows });
            }
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let font_size = px(12.);
        let base_font = font(mono.clone());
        let font_id = window.text_system().resolve_font(&base_font);
        let cell_w = window.text_system().advance(font_id, font_size, 'm').map_or(px(7.2), |s| s.width);
        let cell = size(cell_w, (font_size * 1.35).round());

        if self.session.is_none() && self.once {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div().text_sm().text_color(theme.muted_foreground).child("Run the plugin again from its button."),
                )
                .into_any_element();
        }
        if self.session.is_none() {
            let (warning, muted) = (theme.warning, theme.muted_foreground);
            let target = match &self.local {
                Some((ctx, ns)) => format!(
                    "this machine, kubectl on {ctx}{}",
                    ns.as_ref().map(|n| format!(" / {n}")).unwrap_or_default()
                ),
                None => format!(
                    "{}/{}{}",
                    self.ns,
                    self.pod,
                    self.container.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
                ),
            };
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_sm().text_color(muted).child(format!("Open an interactive shell in {target}")))
                .when_some(self.container_select.clone().filter(|_| self.local.is_none()), |d, s| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_sm()
                            .text_color(muted)
                            .child("Container")
                            .child(div().w(px(220.)).child(Select::new(&s).small())),
                    )
                })
                .when(self.risky, |d| {
                    d.child(
                        div().text_sm().text_color(warning).child(
                            "This context looks like production or admin: commands you type run with those rights.",
                        ),
                    )
                })
                .child(
                    Button::new("connect")
                        .primary()
                        .icon(Lucide::SquareTerminal)
                        .label("Open shell")
                        .on_click(cx.listener(|this, _, window, cx| this.request(false, window, cx))),
                )
                .when(self.local.is_none(), |d| {
                    d.child(
                        Button::new("attach")
                            .outline()
                            .icon(Lucide::Cable)
                            .label("Attach to its process")
                            .tooltip(
                                "kubectl attach: see the main process' output live (and type into it if it has stdin)",
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.request(true, window, cx))),
                    )
                })
                .into_any_element();
        }

        self.sync_size(cell, window, cx);

        let theme = cx.theme();
        let fg = theme.foreground;
        let bg = if theme.is_dark() { hsla(0., 0., 0.06, 1.) } else { hsla(0., 0., 0.98, 1.) };
        let focused = self.focus.is_focused(window);
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (rows, cols) = (u16::from(rows), u16::from(cols));
        let (cur_row, cur_col) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor() && screen.scrollback() == 0;

        let mut lines = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut text = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                let s: &str = if contents.is_empty() { " " } else { contents };
                let mut cfg = vt_color(cell.fgcolor(), fg, theme.is_dark());
                let mut cbg = match cell.bgcolor() {
                    vt100::Color::Default => None,
                    other => Some(vt_color(other, bg, theme.is_dark())),
                };
                let at_cursor = show_cursor && r == cur_row && c == cur_col;
                if cell.inverse() || (at_cursor && focused) {
                    let new_bg = cfg;
                    cfg = cbg.unwrap_or(bg);
                    cbg = Some(new_bg);
                }
                let mut f = base_font.clone();
                if cell.bold() {
                    f.weight = FontWeight::BOLD;
                }
                let len = s.len();
                text.push_str(s);
                match runs.last_mut() {
                    Some(last) if last.color == cfg && last.background_color == cbg && last.font == f => {
                        last.len += len;
                    }
                    _ => runs.push(TextRun {
                        len,
                        font: f,
                        color: cfg,
                        background_color: cbg,
                        underline: None,
                        strikethrough: None,
                    }),
                }
            }
            lines.push(
                div().h(cell.height).whitespace_nowrap().overflow_hidden().child(StyledText::new(text).with_runs(runs)),
            );
        }

        let bounds = self.bounds.clone();
        let connected = self.input.is_some();
        let label = match (&self.local, &self.command) {
            (Some((ctx, _)), Some(c)) => format!("{c}  ·  on {ctx}"),
            (Some((ctx, ns)), None) => {
                format!("kubectl → {ctx}{}", ns.as_ref().map(|n| format!(" / {n}")).unwrap_or_default())
            }
            // The picker shows the container.
            _ if self.container_select.is_some() => format!("{}/{}", self.ns, self.pod),
            _ => format!(
                "{}/{}{}",
                self.ns,
                self.pod,
                self.container.as_ref().map(|c| format!(" · {c}")).unwrap_or_default()
            ),
        };
        let select = self.container_select.clone().filter(|_| self.local.is_none());
        let theme = cx.theme();
        v_flex()
            .size_full()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(div().size(px(7.)).rounded_full().bg(if connected {
                        theme.success
                    } else {
                        theme.muted_foreground
                    }))
                    .child(div().text_color(theme.muted_foreground).child(label))
                    .when_some(select, |d, s| d.child(div().w(px(180.)).child(Select::new(&s).xsmall())))
                    .child(div().flex_1())
                    .child(div().text_color(theme.muted_foreground).child("Ctrl+Shift+V to paste"))
                    .child(
                        Button::new("close-shell")
                            .xsmall()
                            .ghost()
                            .icon(Lucide::X)
                            .label(if connected { "Close shell" } else { "Close" })
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
            .when_some(self.status.clone(), |d, s| {
                d.child(h_flex().gap_2().text_xs().text_color(cx.theme().muted_foreground).child(s).when(
                    self.input.is_none() && !self.once,
                    |d| {
                        d.child(Button::new("reconnect").xsmall().ghost().label("Reconnect").on_click(cx.listener(
                            |this, _, window, cx| {
                                let attach = this.attach;
                                this.request(attach, window, cx);
                            },
                        )))
                    },
                ))
            })
            .child(
                div()
                    .id("terminal")
                    .track_focus(&self.focus)
                    .key_context("Terminal")
                    .on_key_down(cx.listener(Self::on_key))
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .rounded_md()
                    .border_1()
                    .border_color(if focused { cx.theme().ring } else { cx.theme().border })
                    .bg(bg)
                    .p_1()
                    .overflow_hidden()
                    .font_family(mono)
                    .text_size(font_size)
                    .line_height(cell.height)
                    .child(canvas(move |b, _, _| bounds.set(Some(b)), |_, (), _, _| {}).absolute().inset_1())
                    .children(lines),
            )
            .into_any_element()
    }
}

/// Bytes sent for pasted text: wrapped in ESC[200~ … ESC[201~ when the program asked
/// for bracketed paste, so a multi-line paste is not run line by line. An end marker
/// inside the text is dropped so the paste cannot end itself early.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut b = b"\x1b[200~".to_vec();
    b.extend(text.replace("\x1b[201~", "").into_bytes());
    b.extend(b"\x1b[201~");
    b
}

/// Bytes a terminal sends for a key press, as xterm does: named keys carry the modifiers as a
/// parameter (`ESC[1;5D` is Ctrl+Left), and Alt puts ESC before a key (Alt+Backspace).
fn key_bytes(ks: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    // 1, plus 1 for Shift, 2 for Alt and 4 for Ctrl.
    let p = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    // Arrows, Home, End and F1-F4; `ss3` is the unmodified form `ESC O x`.
    let letter = |c: char, ss3: bool| {
        let seq = if p > 1 {
            format!("\x1b[1;{p}{c}")
        } else if ss3 {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        };
        seq.into_bytes()
    };
    let tilde = |n: u8| if p > 1 { format!("\x1b[{n};{p}~") } else { format!("\x1b[{n}~") }.into_bytes();
    let alt = |b: &[u8]| [if m.alt { &b"\x1b"[..] } else { &[] }, b].concat();
    let bytes = match ks.key.as_str() {
        "enter" => alt(b"\r"),
        "backspace" if m.control => alt(b"\x08"),
        "backspace" => alt(b"\x7f"),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => alt(b"\t"),
        "escape" => alt(b"\x1b"),
        "up" => letter('A', app_cursor),
        "down" => letter('B', app_cursor),
        "right" => letter('C', app_cursor),
        "left" => letter('D', app_cursor),
        "home" => letter('H', app_cursor),
        "end" => letter('F', app_cursor),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "insert" => tilde(2),
        "f1" => letter('P', true),
        "f2" => letter('Q', true),
        "f3" => letter('R', true),
        "f4" => letter('S', true),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if m.control => alt(&[0]),
        // Copy in most Linux terminals: never an interrupt (there is no selection to copy).
        "c" if m.control && m.shift => return None,
        key if m.control && key.len() == 1 => alt(&[ctrl_byte(key.as_bytes()[0])?]),
        "space" => alt(b" "),
        _ => {
            let text = ks.key_char.clone()?;
            alt(text.as_bytes())
        }
    };
    Some(bytes)
}

/// What Ctrl with this key sends: a letter's control code, and xterm's codes for punctuation
/// and digits (Ctrl+/ is 0x1f, Ctrl+2 is NUL, Ctrl+8 is DEL).
fn ctrl_byte(key: u8) -> Option<u8> {
    Some(match key.to_ascii_lowercase() {
        c @ b'a'..=b'z' => c - b'a' + 1,
        b' ' | b'@' | b'2' => 0,
        b'[' | b'3' => 0x1b,
        b'\\' | b'4' => 0x1c,
        b']' | b'5' => 0x1d,
        b'^' | b'6' => 0x1e,
        b'/' | b'_' | b'-' | b'7' => 0x1f,
        b'?' | b'8' => 0x7f,
        _ => return None,
    })
}

/// The whole lines in `delta` plus what was left over before; the rest is kept for the next event.
fn whole_lines(rest: &mut f32, delta: f32) -> i32 {
    let total = *rest + delta;
    *rest = total.fract();
    total.trunc() as i32
}

/// A terminal dimension (vt100 wants at least one row and column).
fn nz(v: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(v).unwrap_or(std::num::NonZeroU16::MIN)
}

fn vt_color(color: vt100::Color, default: Hsla, dark: bool) -> Hsla {
    let rgb = |r: u8, g: u8, b: u8| -> Hsla {
        Rgba { r: f32::from(r) / 255., g: f32::from(g) / 255., b: f32::from(b) / 255., a: 1. }.into()
    };
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => rgb(r, g, b),
        vt100::Color::Idx(i) => match i {
            0..=15 => {
                // A readable palette for both themes.
                const DARK: [(u8, u8, u8); 16] = [
                    (40, 42, 46),
                    (224, 108, 117),
                    (152, 195, 121),
                    (229, 192, 123),
                    (97, 175, 239),
                    (198, 120, 221),
                    (86, 182, 194),
                    (200, 204, 212),
                    (92, 99, 112),
                    (240, 128, 136),
                    (170, 215, 140),
                    (240, 205, 140),
                    (120, 190, 250),
                    (215, 140, 235),
                    (110, 200, 210),
                    (240, 242, 246),
                ];
                const LIGHT: [(u8, u8, u8); 16] = [
                    (30, 30, 30),
                    (200, 40, 50),
                    (40, 140, 40),
                    (160, 110, 0),
                    (30, 90, 200),
                    (140, 50, 160),
                    (20, 130, 140),
                    (110, 110, 110),
                    (90, 90, 90),
                    (220, 60, 70),
                    (60, 160, 60),
                    (180, 130, 20),
                    (50, 110, 220),
                    (160, 70, 180),
                    (40, 150, 160),
                    (30, 30, 30),
                ];
                let (r, g, b) = if dark { DARK[i as usize] } else { LIGHT[i as usize] };
                rgb(r, g, b)
            }
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                rgb(v, v, v)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Keystroke, key_bytes, paste_bytes, whole_lines};

    #[test]
    fn copy_shortcut_is_not_an_interrupt() {
        let bytes = |k: &str| key_bytes(&Keystroke::parse(k).unwrap(), false);
        assert_eq!(bytes("ctrl-c"), Some(vec![3]));
        assert_eq!(bytes("ctrl-shift-c"), None);
    }

    #[test]
    fn modifiers_reach_the_shell() {
        let bytes = |k: &str| key_bytes(&Keystroke::parse(k).unwrap(), true).unwrap();
        // Word moves and deletes in bash and zsh.
        assert_eq!(bytes("ctrl-left"), b"\x1b[1;5D");
        assert_eq!(bytes("ctrl-right"), b"\x1b[1;5C");
        assert_eq!(bytes("alt-left"), b"\x1b[1;3D");
        assert_eq!(bytes("alt-backspace"), b"\x1b\x7f");
        assert_eq!(bytes("ctrl-backspace"), b"\x08");
        assert_eq!(bytes("shift-home"), b"\x1b[1;2H");
        assert_eq!(bytes("ctrl-shift-end"), b"\x1b[1;6F");
        assert_eq!(bytes("ctrl-delete"), b"\x1b[3;5~");
        assert_eq!(bytes("shift-pageup"), b"\x1b[5;2~");
        assert_eq!(bytes("shift-f1"), b"\x1b[1;2P");
        assert_eq!(bytes("ctrl-f5"), b"\x1b[15;5~");
        assert_eq!(bytes("shift-tab"), b"\x1b[Z");
        assert_eq!(bytes("ctrl-/"), [0x1f]);
        assert_eq!(bytes("ctrl-2"), [0]);
        assert_eq!(bytes("ctrl-6"), [0x1e]);
        assert_eq!(bytes("ctrl-8"), [0x7f]);
        assert_eq!(bytes("ctrl-alt-b"), b"\x1b\x02");
        // Without modifiers, unchanged (application cursor mode here).
        assert_eq!(bytes("left"), b"\x1bOD");
        assert_eq!(bytes("backspace"), b"\x7f");
        assert_eq!(bytes("delete"), b"\x1b[3~");
    }

    #[test]
    fn home_end_and_function_keys() {
        let bytes = |k: &str, app: bool| key_bytes(&Keystroke::parse(k).unwrap(), app).unwrap();
        assert_eq!(bytes("home", false), b"\x1b[H");
        assert_eq!(bytes("end", false), b"\x1b[F");
        assert_eq!(bytes("home", true), b"\x1bOH");
        assert_eq!(bytes("end", true), b"\x1bOF");
        assert_eq!(bytes("f1", false), b"\x1bOP");
        assert_eq!(bytes("f5", false), b"\x1b[15~");
        assert_eq!(bytes("f10", false), b"\x1b[21~");
        assert_eq!(bytes("f12", true), b"\x1b[24~");
    }

    #[test]
    fn small_scrolls_add_up() {
        let mut rest = 0.;
        let lines: i32 = (0..10).map(|_| whole_lines(&mut rest, 4. / 18.)).sum();
        assert_eq!(lines, 2);
        // Back the other way.
        assert_eq!(whole_lines(&mut rest, -1.), 0);
        assert_eq!(whole_lines(&mut rest, -2.5), -3);
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(paste_bytes("a\nb", false), b"a\nb");
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("x\x1b[201~rm -rf\n", true), b"\x1b[200~xrm -rf\n\x1b[201~");
    }
}
