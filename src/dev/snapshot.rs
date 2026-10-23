//! Renders the app off-screen to a PNG, without opening a window.
//! `cargo run --features dev -- --snapshot out.png [seconds]`

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::ThemeMode;
use gpui_kit::*;

pub fn run(path: &str, wait: Duration) -> Result<()> {
    let text_system = platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(assets::AllAssets),
        platform::current_headless_renderer,
    );
    cx.update(|cx| {
        init(cx);
        crate::app::bind_keys(cx);
        crate::plugins::reload(cx);
        if std::env::var("RECOUVRANCE_WRITES").is_ok() {
            cx.set_global(crate::details::WriteMode(true));
        }
        if let Ok(z) = std::env::var("RECOUVRANCE_ZOOM") {
            crate::app::set_zoom_global(z.parse().unwrap_or(1.0), cx);
        }
        let dark = std::env::var("RECOUVRANCE_DARK").is_ok();
        let mode = if dark { ThemeMode::Dark } else { ThemeMode::Light };
        crate::app::apply_theme(mode, None, cx);
    });
    let mut app_view = None;
    let window = cx.open_window(size(px(1500.), px(920.)), |window, cx| {
        let view = cx.new(|cx| crate::app::AppView::new(window, cx));
        app_view = Some(view.clone());
        cx.new(|cx| base::Root::new(view, window, cx))
    })?;
    let app_view = app_view.expect("view");

    // Kubernetes work runs on real threads; keep pumping until it settles.
    // Draw a frame each step, like a real window, so animations progress.
    let win = window;
    let pump = move |cx: &mut HeadlessAppContext, wait: Duration| {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            cx.run_until_parked();
            let _ = cx.update_window(win.into(), |_, window, cx| window.draw(cx).clear(cx));
            std::thread::sleep(Duration::from_millis(50));
            cx.advance_clock(Duration::from_millis(50));
        }
    };
    pump(&mut cx, wait);
    // RECOUVRANCE_QA=1: open every resource type, its first object and every tab, then every
    // page, drawing frames all along; print what looks wrong. RECOUVRANCE_QA_KINDS limits types.
    if std::env::var("RECOUVRANCE_QA").is_ok() {
        let only: Option<Vec<String>> =
            std::env::var("RECOUVRANCE_QA_KINDS").ok().map(|v| v.split(',').map(String::from).collect());
        let kinds = cx.update(|cx| app_view.read(cx).qa_kinds());
        let mut problems = 0;
        for kind in kinds.iter().filter(|k| only.as_ref().is_none_or(|o| o.iter().any(|x| x == *k))) {
            cx.update(|cx| app_view.update(cx, |a, cx| a.qa_select(kind, cx)));
            // Wait for the list (up to 20 s).
            let start = Instant::now();
            let (mut rows, mut listing, mut status) = (0, true, None);
            while start.elapsed() < Duration::from_secs(20) {
                pump(&mut cx, Duration::from_millis(500));
                (rows, listing, status) = cx.update(|cx| app_view.read(cx).qa_list(cx));
                if !listing {
                    break;
                }
            }
            let mut line = format!("QA {kind}: rows={rows}");
            if listing {
                line.push_str(" STILL-LISTING");
                problems += 1;
            }
            if let Some(st) = status.filter(|s| !s.is_empty()) {
                line.push_str(&format!(" status={st:?}"));
            }
            if rows > 0 {
                cx.update_window(window.into(), |_, window, cx| {
                    app_view.update(cx, |a, cx| a.open_detail(0, window, cx))
                })?;
                pump(&mut cx, Duration::from_secs(4));
                if let Some(d) = cx.update(|cx| app_view.read(cx).qa_detail()) {
                    let tabs = cx.update(|cx| d.read(cx).qa_tabs());
                    for tab in tabs {
                        cx.update(|cx| d.update(cx, |d, cx| d.select_tab_named(tab, cx)));
                        pump(&mut cx, Duration::from_millis(700));
                    }
                    let (yaml_err, events_err, loaded) = cx.update(|cx| d.read(cx).qa_state());
                    if !loaded {
                        line.push_str(" DETAIL-NOT-LOADED");
                        problems += 1;
                    }
                    if let Some(e) = yaml_err {
                        line.push_str(&format!(" DETAIL-ERROR={e:?}"));
                        problems += 1;
                    }
                    if let Some(e) = events_err {
                        line.push_str(&format!(" EVENTS-ERROR={e:?}"));
                        problems += 1;
                    }
                }
                cx.update(|cx| app_view.update(cx, |a, cx| a.qa_back(cx)));
            }
            eprintln!("{line}");
        }
        for page in ["workloads", "cluster", "problems", "access", "apply", "helm", "settings", "agents", "contexts"] {
            cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.qa_page(page, window, cx)))?;
            pump(&mut cx, Duration::from_secs(5));
            eprintln!("QA page {page}: ok");
        }
        eprintln!("QA done: {problems} problem(s)");
    }
    // RECOUVRANCE_KIND=<kind>: list that resource type first.
    if let Ok(kind) = std::env::var("RECOUVRANCE_KIND") {
        cx.update(|cx| app_view.update(cx, |a, cx| a.qa_select(&kind, cx)));
        pump(&mut cx, Duration::from_secs(15));
    }
    // RECOUVRANCE_PAGE=<page>: open a page (problems, cluster, helm…) first.
    if let Ok(page) = std::env::var("RECOUVRANCE_PAGE") {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.qa_page(&page, window, cx)))?;
        pump(&mut cx, Duration::from_secs(25));
    }
    // RECOUVRANCE_FRAMES=<n>: time n full redraws of the window (render + layout + paint).
    if let Some(n) = std::env::var("RECOUVRANCE_FRAMES").ok().and_then(|n| n.parse::<u32>().ok()) {
        let start = Instant::now();
        for _ in 0..n {
            cx.update_window(win.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })?;
        }
        let per = start.elapsed() / n.max(1);
        eprintln!("frame time: {:.2} ms over {n} frames", per.as_secs_f64() * 1000.0);
    }
    if let Ok(v) = std::env::var("RECOUVRANCE_VIEW") {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.demo_view(&v, window, cx)))?;
        pump(&mut cx, Duration::from_secs(6));
    }
    if let Ok(f) = std::env::var("RECOUVRANCE_FILTER") {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |v, cx| v.demo_filter(&f, window, cx)))?;
        pump(&mut cx, Duration::from_secs(1));
    }
    // RECOUVRANCE_SELECT=<row> opens that row's details before capturing.
    if let Some(row) = std::env::var("RECOUVRANCE_SELECT").ok().and_then(|r| r.parse().ok()) {
        cx.update_window(window.into(), |_, window, cx| {
            app_view.update(cx, |v, cx| v.open_detail(row, window, cx));
        })?;
        pump(&mut cx, Duration::from_secs(4));
        if let Ok(tab) = std::env::var("RECOUVRANCE_TAB") {
            cx.update(|cx| app_view.update(cx, |v, cx| v.select_tab(&tab, cx)));
            pump(&mut cx, Duration::from_secs(3));
        }
    }
    if let Some(n) = std::env::var("RECOUVRANCE_MARK").ok().and_then(|n| n.parse().ok()) {
        let bulk = match std::env::var("RECOUVRANCE_BULK").as_deref() {
            Ok("delete") => Some("Delete"),
            Ok("restart") => Some("Restart"),
            Ok("scale") => Some("Scale"),
            _ => None,
        };
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |v, cx| v.demo_mark(n, bulk, window, cx)))?;
        pump(&mut cx, Duration::from_secs(1));
    }
    for act in std::env::var("RECOUVRANCE_ACT").unwrap_or_default().split(',').filter(|a| !a.is_empty()) {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.demo_act(act, window, cx)))?;
        pump(&mut cx, Duration::from_secs(2));
    }
    // RECOUVRANCE_INPUT="click:x,y;scroll:x,y,dy;key:down;key:enter;wait" drives real input events.
    if let Ok(script) = std::env::var("RECOUVRANCE_INPUT") {
        for step in script.split(';') {
            let input = match step.split_once(':') {
                Some(("click", at)) => {
                    let (x, y) = at.split_once(',').unwrap_or(("0", "0"));
                    let position = point(px(x.parse().unwrap_or(0.)), px(y.parse().unwrap_or(0.)));
                    let down = PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    });
                    let up = PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    });
                    vec![down, up]
                }
                Some(("scroll", at)) => {
                    let n: Vec<f32> = at.split(',').map(|v| v.parse().unwrap_or(0.)).collect();
                    let (x, y, dy) = (
                        n.first().copied().unwrap_or(0.),
                        n.get(1).copied().unwrap_or(0.),
                        n.get(2).copied().unwrap_or(0.),
                    );
                    vec![PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(x), px(y)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(-dy))),
                        ..Default::default()
                    })]
                }
                Some(("wait", secs)) => {
                    pump(&mut cx, Duration::from_secs(secs.parse().unwrap_or(1)));
                    vec![]
                }
                Some(("type", text)) => {
                    for ch in text.chars() {
                        let k = match ch {
                            ' ' => "space".to_string(),
                            '-' => "-".to_string(),
                            c => c.to_string(),
                        };
                        if let Ok(keystroke) = Keystroke::parse(&k) {
                            let keystroke = Keystroke { key_char: Some(ch.to_string()), ..keystroke };
                            cx.update_window(window.into(), |_, window, cx| {
                                window.dispatch_keystroke(keystroke, cx);
                            })?;
                        }
                    }
                    vec![]
                }
                Some(("key", k)) => {
                    if let Ok(keystroke) = Keystroke::parse(k) {
                        cx.update_window(window.into(), |_, window, cx| {
                            window.activate_window();
                            let handled = window.dispatch_keystroke(keystroke, cx);
                            eprintln!("key {k}: handled={handled}");
                        })?;
                    }
                    vec![]
                }
                _ => vec![],
            };
            for ev in input {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(ev, cx);
                })?;
                pump(&mut cx, Duration::from_millis(150));
            }
            pump(&mut cx, Duration::from_millis(600));
        }
        pump(&mut cx, Duration::from_secs(3));
    }
    if let Ok(text) = std::env::var("RECOUVRANCE_TYPE") {
        let text = text.replace("\\r", "\r");
        cx.update(|cx| app_view.update(cx, |v, cx| v.demo_type(&text, cx)));
        pump(&mut cx, Duration::from_secs(6));
    }
    if let Ok(ctx) = std::env::var("RECOUVRANCE_COMPARE_CTX") {
        cx.update(|cx| app_view.update(cx, |v, cx| v.demo_compare(ctx, cx)));
        pump(&mut cx, Duration::from_secs(4));
    }
    if std::env::var("RECOUVRANCE_RUNCRON").is_ok() {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |v, cx| v.demo_run_cron(window, cx)))?;
        pump(&mut cx, Duration::from_secs(1));
    }
    if let Some(ix) = std::env::var("RECOUVRANCE_PLUGIN").ok().and_then(|v| v.parse().ok()) {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.demo_plugin(ix, window, cx)))?;
        pump(&mut cx, Duration::from_secs(6));
    }
    if let Ok(q) = std::env::var("RECOUVRANCE_ASK") {
        cx.update(|cx| app_view.update(cx, |a, cx| a.demo_ask(&q, cx)));
        pump(&mut cx, Duration::from_secs(20));
    }
    if let Ok(q) = std::env::var("RECOUVRANCE_HELM_SEARCH") {
        let dry = std::env::var("RECOUVRANCE_HELM_DRY").is_ok();
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.demo_helm(&q, dry, window, cx)))?;
        pump(&mut cx, Duration::from_secs(25));
    }
    if std::env::var("RECOUVRANCE_RULES_CODE").is_ok() {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |a, cx| a.demo_rules_code(window, cx)))?;
        pump(&mut cx, Duration::from_secs(1));
    }
    if std::env::var("RECOUVRANCE_RESTORE").is_ok() {
        cx.update(|cx| app_view.update(cx, |a, cx| a.demo_restore_forwards(cx)));
        pump(&mut cx, Duration::from_secs(4));
    }
    if let Ok(w) = std::env::var("RECOUVRANCE_POPOVER") {
        cx.update(|cx| app_view.update(cx, |a, cx| a.demo_popover(&w, cx)));
        pump(&mut cx, Duration::from_secs(1));
    }
    // Opens a confirmation only; nothing is confirmed.
    if let Ok(a) = std::env::var("RECOUVRANCE_ACTION") {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |v, cx| v.demo_action(&a, window, cx)))?;
        pump(&mut cx, Duration::from_secs(1));
    }
    if let Ok(q) = std::env::var("RECOUVRANCE_PALETTE") {
        cx.update_window(window.into(), |_, window, cx| app_view.update(cx, |v, cx| v.demo_palette(&q, window, cx)))?;
        pump(&mut cx, Duration::from_secs(8));
    }
    if std::env::var("RECOUVRANCE_TERM_DEMO").is_ok() {
        let demo = "\x1b[1;32mroot@pod\x1b[0m:\x1b[1;34m/app\x1b[0m# ls -la --color\r\n\
total 12\r\n\
drwxr-xr-x 1 root root 4096 Oct 15 \x1b[1;34m.\x1b[0m\r\n\
-rwxr-xr-x 1 root root 1024 Oct 15 \x1b[1;32mentrypoint.sh\x1b[0m\r\n\
-rw-r--r-- 1 root root  512 Oct 15 config.yaml\r\n\
\x1b[31merror:\x1b[0m something \x1b[7minverse\x1b[0m \x1b[38;5;208m256-color\x1b[0m \x1b[38;2;80;160;255mtruecolor\x1b[0m\r\n\
\x1b[1;32mroot@pod\x1b[0m:\x1b[1;34m/app\x1b[0m# ";
        cx.update(|cx| app_view.update(cx, |v, cx| v.demo_terminal(demo.as_bytes(), cx)));
        pump(&mut cx, Duration::from_secs(1));
    }
    // Two frames: some views size themselves from the previous frame's layout.
    for _ in 0..2 {
        cx.update_window(window.into(), |_, window, _| window.refresh())?;
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        cx.run_until_parked();
    }

    cx.capture_screenshot(window.into())?.save(path)?;
    // RECOUVRANCE_FRAMES_END=<n>: time n full redraws of what is on screen at the end.
    let frames = std::env::var("RECOUVRANCE_FRAMES_END").ok().and_then(|n| n.parse::<u32>().ok());
    if let Some(n) = frames.filter(|&n| n > 0) {
        let mut times: Vec<Duration> = (0..n)
            .map(|_| {
                let start = Instant::now();
                let _ = cx.update_window(win.into(), |_, window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
                start.elapsed()
            })
            .collect();
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        eprintln!(
            "frames at the end: median {:.2} ms, p90 {:.2} ms over {n} frames",
            ms(times[times.len() / 2]),
            ms(times[times.len() * 9 / 10])
        );
    }
    Ok(())
}
