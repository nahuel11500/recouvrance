//! What the list page shows of contexts that can't show rows: "Connecting…" or "Can't
//! connect:" and why, for each, with a Retry. In place of the table when none of the set is
//! connected (an empty table read as "No resources"), as a banner over it when others are.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::conn::Conn;
use super::{AppView, Route};
use crate::ui::selectable::selectable;

impl AppView {
    /// Whether none of the set can show rows yet: the connect panel takes the table's place.
    pub(super) fn nothing_connected(&self) -> bool {
        self.client.is_none() && !self.conn.is_empty()
    }

    /// One line per context of the set that isn't connected.
    fn conn_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        (self.conn.iter())
            .map(|(ctx, state)| {
                let (icon, color, said) = match state {
                    Conn::Connecting => (Lucide::LoaderCircle, theme.muted_foreground, "Connecting…".to_string()),
                    Conn::Failed(why) => (Lucide::TriangleAlert, theme.danger, format!("Can't connect: {why}")),
                };
                h_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .child(div().pt_0p5().flex_none().child(Icon::new(icon).size_4().text_color(color)))
                    .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(ctx.clone()))
                    .child(div().min_w_0().flex_1().text_color(color).child(selectable(said)))
                    .into_any_element()
            })
            .collect()
    }

    /// Connect to the set again, once none of it is still connecting (a login may be waiting).
    fn retry_button(&self, id: &'static str) -> Option<Button> {
        let failed = self.conn.iter().any(|(_, s)| matches!(s, Conn::Failed(_)));
        (failed && !self.conn.any_connecting()).then(|| {
            Button::new(id).small().icon(Lucide::RefreshCw).label("Retry").tooltip("Connect to these contexts again")
        })
    }

    /// In place of the table while none of the set is connected.
    pub(super) fn render_connect(&self, cx: &mut Context<Self>) -> AnyElement {
        let connecting = self.conn.any_connecting();
        let title = if connecting { "Connecting…" } else { "Can't connect" };
        let hint = if connecting {
            "A login may have opened a browser window; it can take a few minutes."
        } else {
            "Check the VPN or network, the credentials, or the context in the kubeconfig."
        };
        let muted = cx.theme().muted_foreground;
        let retry = self.retry_button("conn-retry");
        v_flex()
            .id("conn-panel")
            .size_full()
            .items_center()
            .justify_center()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w(px(620.))
                    .max_w_full()
                    .p_4()
                    .gap_3()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
                    .children(self.conn_rows(cx))
                    .child(div().text_xs().text_color(muted).child(hint))
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_1()
                            .when_some(retry, |d, b| {
                                d.child(b.primary().on_click(cx.listener(|this, _, window, cx| {
                                    this.retry_connect(window, cx);
                                })))
                            })
                            .child(
                                Button::new("conn-choose")
                                    .small()
                                    .ghost()
                                    .label("Choose contexts")
                                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Contexts, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Over the table, while others of the set show their rows.
    pub(super) fn render_conn_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.conn.is_empty() || self.nothing_connected() {
            return None;
        }
        let retry = self.retry_button("conn-banner-retry");
        Some(
            h_flex()
                .gap_3()
                .items_start()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .child(v_flex().flex_1().min_w_0().gap_1().children(self.conn_rows(cx)))
                .when_some(retry, |d, b| {
                    d.child(b.outline().on_click(cx.listener(|this, _, window, cx| this.retry_connect(window, cx))))
                })
                .into_any_element(),
        )
    }
}
