//! Port-forward with options.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use super::DetailPanel;

impl DetailPanel {
    /// Start a port-forward with options: local port, listen address, https, open in browser.
    pub fn pf_options_dialog(
        &mut self,
        saved: crate::settings::SavedForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = |text: String, hint: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(hint);
                s.set_value(text, window, cx);
                s
            })
        };
        let local = input(
            if saved.remote >= 1024 { saved.remote.to_string() } else { String::new() },
            "local port (empty = any free port)",
            window,
            cx,
        );
        let address = input("127.0.0.1".into(), "listen address", window, cx);
        let https = std::rc::Rc::new(std::cell::Cell::new(saved.https));
        let open = std::rc::Rc::new(std::cell::Cell::new(true));
        let (client, forwards) = (self.client.clone(), self.forwards.clone());
        let title = format!("Forward port {}", saved.remote);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (local, address, https, open, saved, client, forwards) = (
                local.clone(),
                address.clone(),
                https.clone(),
                open.clone(),
                saved.clone(),
                client.clone(),
                forwards.clone(),
            );
            let (h1, o1) = (https.clone(), open.clone());
            alert
                .title(title.clone())
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().w(px(110.)).text_sm().child("Local port"))
                                .child(div().flex_1().child(Input::new(&local))),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().w(px(110.)).text_sm().child("Listen on"))
                                .child(div().flex_1().child(Input::new(&address))),
                        )
                        .child(
                            div().text_xs().child(
                                "127.0.0.1 keeps it on this machine; 0.0.0.0 lets others on your network reach it.",
                            ),
                        )
                        .child(
                            Checkbox::new("pf-https").label("HTTPS (open as https://)").checked(https.get()).on_click(
                                move |v: &bool, window, _| {
                                    h1.set(*v);
                                    window.refresh();
                                },
                            ),
                        )
                        .child(
                            Checkbox::new("pf-open")
                                .label("Open in the browser when ready")
                                .checked(open.get())
                                .on_click(move |v: &bool, window, _| {
                                    o1.set(*v);
                                    window.refresh();
                                }),
                        ),
                )
                .show_cancel(true)
                .ok_text("Forward")
                .on_ok(move |_, _, cx| {
                    let port = local.read(cx).value().trim().to_string();
                    let addr = address.read(cx).value().trim().to_string();
                    let saved = crate::settings::SavedForward {
                        local: if port.is_empty() {
                            None
                        } else {
                            match port.parse() {
                                Ok(p) => Some(p),
                                Err(_) => return false,
                            }
                        },
                        address: (!addr.is_empty() && addr != "127.0.0.1").then_some(addr),
                        https: https.get(),
                        open_browser: open.get(),
                        ..saved.clone()
                    };
                    forwards.update(cx, |f, cx| f.start(client.clone(), saved, cx));
                    true
                })
        });
    }
}
