//! Connect to Argo CD…: a dialog that takes the address people paste, reads how that server
//! signs in, and offers only those ways: SSO in the browser, a username and password, or a
//! token pasted. The token found is checked, then kept as Settings → Argo CD keeps one (see
//! `k8s::argo::connect`). Closing the dialog stops whatever it was waiting for, the browser's
//! answer included.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s::argo::connect::{self, SignIn};
use crate::k8s::argo::server::Server;
use crate::latest::Latest;

pub struct ConnectArgo {
    address: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    token: Entity<InputState>,
    /// The server picked and how it signs in, once read.
    server: Option<(Server, SignIn)>,
    /// What runs now (reading the server, waiting for the browser, checking the token).
    busy: Option<SharedString>,
    error: Option<SharedString>,
    /// What connecting found, once done.
    done: Option<SharedString>,
    /// One step at a time: a new one replaces (and stops) the one running.
    work: Latest,
    _subs: Vec<Subscription>,
}

/// Open the dialog; with `address`, read that server at once (Reconnect).
pub fn open(address: Option<String>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ConnectArgo::new(address, window, cx));
    let focus = view.read(cx).address.focus_handle(cx);
    focus.focus(window, cx);
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Connect to Argo CD").w(px(600.)).child(view.clone()));
}

impl ConnectArgo {
    fn new(address: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |hint: &'static str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).masked(masked).placeholder(hint))
        };
        let address_input =
            input("https://argocd.example.com, or a page of it copied from the browser", false, window, cx);
        let username = input("username", false, window, cx);
        let password = input("password", true, window, cx);
        let token = input("token (argocd account generate-token)", true, window, cx);
        let subs = vec![
            cx.subscribe_in(&address_input, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.read_server(window, cx);
                }
            }),
            cx.subscribe_in(&password, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.sign_in_password(window, cx);
                }
            }),
            cx.subscribe_in(&token, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.use_token(window, cx);
                }
            }),
        ];
        let mut this = Self {
            address: address_input,
            username,
            password,
            token,
            server: None,
            busy: None,
            error: None,
            done: None,
            work: Latest::default(),
            _subs: subs,
        };
        if let Some(address) = address {
            this.address.update(cx, |s, cx| s.set_value(address, window, cx));
            this.read_server(window, cx);
        }
        this
    }

    fn failed(&mut self, why: String, cx: &mut Context<Self>) {
        self.busy = None;
        self.error = Some(why.into());
        cx.notify();
    }

    fn working(&mut self, what: String, cx: &mut Context<Self>) {
        self.busy = Some(what.into());
        self.error = None;
        cx.notify();
    }

    /// The address typed: how that server signs in.
    fn read_server(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let typed = self.address.read(cx).value().to_string();
        self.server = None;
        self.done = None;
        let server = match connect::normalize(&typed) {
            Ok(s) => s,
            Err(e) => return self.failed(e, cx),
        };
        self.working(format!("Reading how {} signs in…", server.name()), cx);
        let s = server.clone();
        self.work.spawn(cx, connect::sign_in(s), move |this, found, cx| match found {
            Ok(sign_in) => {
                this.busy = None;
                this.server = Some((server, sign_in));
                cx.notify();
            }
            Err(e) => this.failed(e, cx),
        });
    }

    /// SSO in the browser: listen for its answer first (port 8085 taken is said before
    /// anything opens), read the provider, then open the browser and wait.
    fn sign_in_sso(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some((server, SignIn { sso: Some(sso), .. })) = self.server.clone() else { return };
        self.working(format!("Opening {} in the browser…", sso.name), cx);
        let issuer = sso.issuer.clone();
        let ready = async move {
            let listener = connect::listen().await?;
            let provider = connect::discover(issuer).await?;
            Ok::<_, String>((listener, provider))
        };
        self.work.spawn(cx, ready, move |this, ready, cx| {
            let (listener, provider) = match ready {
                Ok(r) => r,
                Err(e) => return this.failed(e, cx),
            };
            let pkce = connect::Pkce::new();
            if !crate::forwards::open_web_link(&connect::authorize_url(&provider, &sso, &pkce), cx) {
                return this.failed("The identity provider's address is not a web link".into(), cx);
            }
            this.working(
                format!("Waiting for the browser: sign in with {} there (this stops in 5 minutes)…", sso.name),
                cx,
            );
            let client_id = sso.client_id.clone();
            let token_endpoint = provider.token.clone();
            let got = async move {
                let code = connect::wait_for_code(listener, pkce.state, connect::SSO_TIMEOUT).await?;
                connect::exchange(provider, client_id, code, pkce.verifier).await
            };
            this.work.spawn(cx, got, move |this, tokens, cx| match tokens {
                Ok(t) => {
                    // Its own refresh token renews it (never the argocd CLI's).
                    let refresh = t.refresh_token.map(|refresh_token| connect::Refresh {
                        token_endpoint,
                        client_id: sso.client_id.clone(),
                        refresh_token,
                    });
                    this.check_and_save(server, t.id_token, refresh, cx);
                }
                Err(e) => this.failed(e, cx),
            });
        });
    }

    fn sign_in_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, sign_in)) = self.server.clone() else { return };
        if !sign_in.password {
            return;
        }
        let user = self.username.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        if user.is_empty() || password.is_empty() {
            return self.failed("Type the username and the password".into(), cx);
        }
        self.password.update(cx, |s, cx| s.set_value("", window, cx));
        self.working(format!("Signing in to {} as {user}…", server.name()), cx);
        let s = server.clone();
        self.work.spawn(cx, connect::log_in(s, user, password), move |this, token, cx| match token {
            Ok(token) => this.check_and_save(server, token, None, cx),
            Err(e) => this.failed(e, cx),
        });
    }

    fn use_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, _)) = self.server.clone() else { return };
        let token = self.token.read(cx).value().trim().to_string();
        if token.is_empty() {
            return self.failed("Paste a token".into(), cx);
        }
        self.token.update(cx, |s, cx| s.set_value("", window, cx));
        self.check_and_save(server, token, None, cx);
    }

    /// Check the token on the server (who it logs in as), then keep it.
    fn check_and_save(
        &mut self,
        server: Server,
        token: String,
        refresh: Option<connect::Refresh>,
        cx: &mut Context<Self>,
    ) {
        self.working(format!("Checking the login to {}…", server.name()), cx);
        let (s, t) = (server.clone(), token.clone());
        self.work.spawn(cx, crate::k8s::argo::api::check_token(s, t), move |this, checked, cx| {
            let who = match checked {
                Ok(who) => who,
                Err(e) => return this.failed(e, cx),
            };
            match connect::save(&server, &token, refresh) {
                Ok(()) => {
                    this.busy = None;
                    this.error = None;
                    this.done = Some(format!("Connected to {}. {who}.", server.name()).into());
                    cx.notify();
                }
                Err(e) => this.failed(e, cx),
            }
        });
    }

    fn methods(&self, server: &Server, sign_in: &SignIn, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let busy = self.busy.is_some();
        let heading = |t: &str| div().pt_2().text_sm().font_weight(FontWeight::MEDIUM).child(t.to_string());
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                "{} signs in with {}.",
                server.base,
                match (&sign_in.sso, sign_in.password) {
                    (Some(s), true) => format!("{} (SSO), a local account, or a token", s.name),
                    (Some(s), false) => format!("{} (SSO), or a token", s.name),
                    (None, true) => "a local account, or a token".to_string(),
                    (None, false) => "a token".to_string(),
                }
            )))
            .when_some(sign_in.sso.as_ref(), |d, sso| {
                d.child(
                    div().debug_selector(|| "argo-connect-sso".into()).child(
                        Button::new("argo-connect-sso")
                            .small()
                            .primary()
                            .icon(Lucide::ExternalLink)
                            .label(format!("Sign in with {} in the browser", sso.name))
                            .tooltip("As `argocd login --sso` does; the browser comes back to localhost:8085")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.sign_in_sso(window, cx))),
                    ),
                )
            })
            .when(sign_in.password, |d| {
                d.child(heading("Local account")).child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.username).small()))
                        .child(div().flex_1().child(Input::new(&self.password).small()))
                        .child(
                            div().debug_selector(|| "argo-connect-password".into()).child(
                                Button::new("argo-connect-password")
                                    .small()
                                    .outline()
                                    .label("Sign in")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| this.sign_in_password(window, cx))),
                            ),
                        ),
                )
            })
            .child(heading("Token"))
            .child(
                h_flex().gap_2().child(div().flex_1().child(Input::new(&self.token).small())).child(
                    div().debug_selector(|| "argo-connect-token".into()).child(
                        Button::new("argo-connect-token")
                            .small()
                            .outline()
                            .label("Use token")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.use_token(window, cx))),
                    ),
                ),
            )
            .into_any_element()
    }
}

impl Render for ConnectArgo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.busy.is_some();
        let server = self.server.clone();
        v_flex()
            .id("argo-connect")
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.address)))
                    .child(
                        Button::new("argo-connect-next")
                            .outline()
                            .label("Next")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.read_server(window, cx))),
                    ),
            )
            .when_some(server.filter(|_| self.done.is_none()), |d, (server, sign_in)| {
                d.child(self.methods(&server, &sign_in, cx))
            })
            .children(self.busy.clone().map(|b| div().text_sm().text_color(theme.muted_foreground).child(b)))
            .children(self.error.clone().map(|e| div().id("argo-connect-error").text_sm().text_color(theme.danger).child(e)))
            .when_some(self.done.clone(), |d, done| {
                d.child(div().id("argo-connect-done").text_sm().text_color(theme.success).child(done)).child(
                    h_flex().justify_end().child(
                        Button::new("argo-connect-close")
                            .small()
                            .primary()
                            .label("Close")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
            })
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                "The token is kept in {}, readable only by you, and sent to that server only. A password is never kept.",
                crate::paths::shown_config("argocd-api-tokens.json")
            )))
    }
}

#[cfg(all(test, feature = "dev"))]
mod tests {
    // Not `gpui_kit::*`: its `test` macro would shadow the `#[test]` that `#[gpui_kit::test]` expands to.
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use super::ConnectArgo;
    use crate::k8s::argo::connect::{SignIn, Sso, normalize};

    fn dialog(cx: &mut TestAppContext) -> (Entity<ConnectArgo>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let v = cx.new(|cx| ConnectArgo::new(None, window, cx));
            view = Some(v.clone());
            gpui_kit::base::Root::new(v, window, cx)
        });
        (view.expect("the window was built"), cx)
    }

    fn drawn(cx: &mut VisualTestContext, id: &'static str) -> bool {
        cx.update(|window, cx| drop(window.draw(cx)));
        cx.debug_bounds(id).is_some()
    }

    /// Plain http to another machine is refused before anything is sent.
    #[gpui_kit::test]
    fn plain_http_is_refused_before_any_request(cx: &mut TestAppContext) {
        let (view, cx) = dialog(cx);
        view.update_in(cx, |v, window, cx| {
            v.address.update(cx, |s, cx| s.set_value("http://argocd.example.com", window, cx));
            v.read_server(window, cx);
        });
        view.read_with(cx, |v, _| {
            assert!(v.busy.is_none() && v.server.is_none());
            assert!(v.error.as_ref().is_some_and(|e| e.contains("https")), "{:?}", v.error);
        });
    }

    /// Only the ways the server offers are shown; a token always is.
    #[gpui_kit::test]
    fn only_the_servers_ways_in_are_offered(cx: &mut TestAppContext) {
        let (view, cx) = dialog(cx);
        let server = normalize("argocd.example.com").expect("an address");
        view.update(cx, |v, cx| {
            v.server = Some((server.clone(), SignIn { sso: None, password: false, url: None }));
            cx.notify();
        });
        assert!(!drawn(cx, "argo-connect-sso") && !drawn(cx, "argo-connect-password"));
        assert!(drawn(cx, "argo-connect-token"));
        let sso = Sso {
            name: "GitHub".into(),
            issuer: "https://argocd.example.com/api/dex".into(),
            client_id: "argo-cd-cli".into(),
            scopes: vec!["openid".into()],
            id_token_claims: None,
        };
        view.update(cx, |v, cx| {
            v.server = Some((server, SignIn { sso: Some(sso), password: true, url: None }));
            cx.notify();
        });
        assert!(drawn(cx, "argo-connect-sso") && drawn(cx, "argo-connect-password"));
    }
}
