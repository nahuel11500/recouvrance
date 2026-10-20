//! Rendering of the detail tabs: overview, containers, ports, YAML, events, and the page itself.

#[allow(unused_imports)]
use super::*;

impl DetailPanel {
    pub(super) fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        match (&self.object, &self.yaml_error) {
            (Some(value), _) if self.obj.kind.is("", "Service") => v_flex()
                .size_full()
                .gap_3()
                .child(self.render_service_ports(value, cx))
                .child(div().flex_1().min_h_0().child(crate::overview::render(&self.ctx(value, cx), cx)))
                .into_any_element(),
            (Some(value), _) if self.data_editor.is_some() => v_flex()
                .id("data-editor")
                .size_full()
                .overflow_y_scroll()
                .gap_3()
                .children(self.data_editor.clone())
                .child(crate::overview::render(&self.ctx(value, cx), cx))
                .into_any_element(),
            (Some(value), _) => {
                let body = crate::overview::render(&self.ctx(value, cx), cx);
                if !self.obj.kind.is("", "Secret") {
                    return body;
                }
                v_flex()
                    .size_full()
                    .gap_2()
                    .child(
                        h_flex().px_2().child(
                            Button::new("reveal-all")
                                .xsmall()
                                .ghost()
                                .icon(if self.reveal_secrets { Lucide::EyeOff } else { Lucide::Eye })
                                .label(if self.reveal_secrets { "Hide values" } else { "Reveal all" })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_yaml_view(!this.reveal_secrets, this.show_managed, window, cx);
                                })),
                        ),
                    )
                    .child(div().flex_1().min_h_0().child(body))
                    .into_any_element()
            }
            (None, Some(err)) => error_box(err.clone(), cx).into_any_element(),
            (None, None) => div().text_color(cx.theme().muted_foreground).child("Loading…").into_any_element(),
        }
    }

    pub(super) fn render_container(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(value) = &self.object else {
            return div().into_any_element();
        };
        let Some(name) = container_names(value).get(ix).cloned() else {
            return div().into_any_element();
        };
        let (n1, n2) = (name.clone(), name.clone());
        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Files"))
                    .child(
                        Button::new("file-down")
                            .xsmall()
                            .outline()
                            .icon(Lucide::Download)
                            .label("Download…")
                            .tooltip("Copy a file or folder out of the container to ~/Downloads (kubectl cp)")
                            .on_click(cx.listener(move |this, _, w, cx| this.download_dialog(n1.clone(), w, cx))),
                    )
                    .child(
                        Button::new("file-up")
                            .xsmall()
                            .outline()
                            .icon(Lucide::Upload)
                            .label("Upload…")
                            .tooltip("Copy a local file into the container (kubectl cp)")
                            .on_click(cx.listener(move |this, _, w, cx| this.upload_pick(n2.clone(), w, cx))),
                    ),
            )
            .child(self.render_ports(value, &name, cx))
            .child(div().flex_1().min_h_0().child(crate::overview::container(&self.ctx(value, cx), &name, cx)))
            .into_any_element()
    }

    /// Service ports, forwarded to a ready pod behind the Service.
    pub(super) fn render_service_ports(&self, svc: &serde_json::Value, cx: &mut Context<Self>) -> AnyElement {
        let ports = svc.pointer("/spec/ports").and_then(|p| p.as_array()).cloned().unwrap_or_default();
        if ports.is_empty() {
            return div().into_any_element();
        }
        let ready_pod = self.related.pods.iter().find(|p| {
            p.pointer("/status/conditions")
                .and_then(|c| c.as_array())
                .is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
        });
        let muted = cx.theme().muted_foreground;
        let Some(pod) = ready_pod.cloned() else {
            return div()
                .px_2()
                .text_xs()
                .text_color(muted)
                .child(if self.related.pods.is_empty() {
                    "Port forward: no pods behind this Service"
                } else {
                    "Port forward: no ready pod"
                })
                .into_any_element();
        };
        let pod_name = pod.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        // targetPort may be a number or a container port name.
        let resolve = |target: &serde_json::Value, port: i64| -> Option<u16> {
            if let Some(n) = target.as_i64() {
                return Some(n as u16);
            }
            let Some(name) = target.as_str() else { return Some(port as u16) };
            pod.pointer("/spec/containers")?
                .as_array()?
                .iter()
                .flat_map(|c| c["ports"].as_array().cloned().unwrap_or_default())
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| p["containerPort"].as_i64())
                .map(|n| n as u16)
        };
        let ns = self.obj.namespace.clone().unwrap_or_default();
        h_flex()
            .px_2()
            .gap_2()
            .flex_wrap()
            .child(div().text_xs().text_color(muted).child(format!("Port forward (via {pod_name})")))
            .children(ports.iter().enumerate().filter_map(|(i, p)| {
                let port = p["port"].as_i64()?;
                let remote = resolve(&p["targetPort"], port)?;
                let label = match p["name"].as_str() {
                    Some(n) => format!("{n}:{port}"),
                    None => port.to_string(),
                };
                let (client, forwards) = (self.client.clone(), self.forwards.clone());
                let saved = crate::settings::SavedForward {
                    context: self.context.clone(),
                    namespace: ns.clone(),
                    service: Some(self.obj.name.clone()),
                    pod: pod_name.clone(),
                    remote,
                    local: None,
                    address: None,
                    https: remote == 443 || remote == 8443,
                    open_browser: crate::forwards::opens_browser(remote, p["name"].as_str()),
                };
                let opts = saved.clone();
                Some(
                    h_flex()
                        .child(
                            Button::new(("svc-pf", i))
                                .xsmall()
                                .outline()
                                .icon(Lucide::ArrowRightLeft)
                                .label(label)
                                .on_click(move |_, _, cx| {
                                    forwards.update(cx, |f, cx| f.start(client.clone(), saved.clone(), cx));
                                }),
                        )
                        .child(
                            Button::new(("svc-pf-opt", i))
                                .xsmall()
                                .ghost()
                                .icon(Lucide::Settings2)
                                .tooltip("Forward with options: local port, address, https, open in browser")
                                .on_click(
                                    cx.listener(move |this, _, w, cx| this.pf_options_dialog(opts.clone(), w, cx)),
                                ),
                        ),
                )
            }))
            .into_any_element()
    }

    /// A container's ports with a one-click port-forward each.
    pub(super) fn render_ports(
        &self,
        value: &serde_json::Value,
        container: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ports: Vec<(u16, String)> = value
            .pointer("/spec/containers")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(|c| c["name"].as_str() == Some(container))
            .flat_map(|c| c["ports"].as_array().cloned().unwrap_or_default())
            .filter_map(|p| {
                let port = p["containerPort"].as_u64()? as u16;
                let name = p["name"].as_str().map(|n| format!("{n}:{port}")).unwrap_or_else(|| port.to_string());
                Some((port, name))
            })
            .collect();
        if ports.is_empty() {
            return div().into_any_element();
        }
        let ns = self.obj.namespace.clone().unwrap_or_default();
        h_flex()
            .px_2()
            .gap_2()
            .flex_wrap()
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Port forward"))
            .children(ports.into_iter().map(|(port, label)| {
                let (client, forwards) = (self.client.clone(), self.forwards.clone());
                let saved = crate::settings::SavedForward {
                    context: self.context.clone(),
                    namespace: ns.clone(),
                    service: None,
                    pod: self.obj.name.clone(),
                    remote: port,
                    local: None,
                    address: None,
                    https: port == 443 || port == 8443,
                    open_browser: crate::forwards::opens_browser(port, Some(&label)),
                };
                let opts = saved.clone();
                h_flex()
                    .child(
                        Button::new(("pf", port as usize))
                            .xsmall()
                            .outline()
                            .icon(Lucide::ArrowRightLeft)
                            .label(label)
                            .on_click(move |_, _, cx| {
                                forwards.update(cx, |f, cx| f.start(client.clone(), saved.clone(), cx));
                            }),
                    )
                    .child(
                        Button::new(("pf-opt", port as usize))
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Settings2)
                            .tooltip("Forward with options: local port, address, https, open in browser")
                            .on_click(cx.listener(move |this, _, w, cx| this.pf_options_dialog(opts.clone(), w, cx))),
                    )
            }))
            .into_any_element()
    }

    /// Show Secret values or managedFields (or not). The YAML editor is reloaded
    /// for that, so unsaved edits there are only thrown away after asking.
    pub(super) fn set_yaml_view(&mut self, reveal: bool, managed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let apply = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            this.reveal_secrets = reveal;
            this.show_managed = managed;
            this.refresh_yaml(window, cx);
            cx.notify();
        };
        if self.yaml.read(cx).value().as_ref() == self.yaml_text {
            apply(self, window, cx);
            return;
        }
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            alert
                .title("Discard your YAML edits?")
                .description("The YAML editor is reloaded to show this, which loses the changes you have not saved.")
                .show_cancel(true)
                .ok_text("Discard edits")
                .ok_variant(component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = panel.update(cx, |this, cx| apply(this, window, cx));
                    true
                })
        });
    }

    pub(super) fn render_yaml(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(err) = &self.yaml_error {
            return error_box(err.clone(), cx).into_any_element();
        }
        let is_secret = self.obj.kind.is("", "Secret");
        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_4()
                    .when(is_secret, |d| {
                        d.child(
                            Checkbox::new("reveal")
                                .label("Reveal secret values")
                                .checked(self.reveal_secrets)
                                .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                    this.set_yaml_view(*checked, this.show_managed, window, cx);
                                })),
                        )
                    })
                    .child(Checkbox::new("managed").label("managedFields").checked(self.show_managed).on_click(
                        cx.listener(|this, v: &bool, window, cx| {
                            this.set_yaml_view(this.reveal_secrets, *v, window, cx);
                        }),
                    ))
                    .child(
                        Checkbox::new("field-docs").label("Field docs").checked(self.field_docs.is_some()).on_click(
                            cx.listener(|this, v: &bool, _, cx| {
                                this.toggle_field_docs(*v, cx);
                            }),
                        ),
                    ),
            )
            .when(writes_enabled(cx), |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Write mode: edit the YAML, then")
                        .child(
                            Button::new("apply-yaml")
                                .xsmall()
                                .primary()
                                .label("Validate & save…")
                                .on_click(cx.listener(|this, _, window, cx| this.apply_yaml(window, cx))),
                        )
                        .child(Button::new("reset-yaml").xsmall().ghost().label("Discard edits").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.refresh_yaml(window, cx);
                                cx.notify();
                            }),
                        )),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(Editor::new(&self.yaml).readonly(!writes_enabled(cx)).h_full()),
                    )
                    .children(self.field_docs.clone().map(|d| div().w(px(380.)).h_full().child(d))),
            )
            .into_any_element()
    }

    pub(super) fn render_events(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        match &self.events {
            None => div().text_color(muted).child("Loading…").into_any_element(),
            Some(Err(e)) => error_box(e.clone(), cx).into_any_element(),
            Some(Ok(events)) if events.is_empty() => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(div().text_lg().child("No recent events"))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("Events have a default time to live of 1 hour in Kubernetes"),
                )
                .into_any_element(),
            Some(Ok(events)) => {
                let warning = cx.theme().warning;
                v_flex()
                    .id("events")
                    .size_full()
                    .overflow_y_scroll()
                    .gap_2()
                    .children(events.iter().map(|e| {
                        v_flex()
                            .gap_0p5()
                            .pb_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .when(e.kind == "Warning", |d| d.text_color(warning))
                                            .child(e.reason.clone()),
                                    )
                                    .child(div().text_color(muted).child(format!(
                                        "{} ago{}",
                                        e.age,
                                        if e.count > 1 { format!(" · ×{}", e.count) } else { String::new() }
                                    ))),
                            )
                            .child(div().text_sm().child(e.message.clone()))
                    }))
                    .into_any_element()
            }
        }
    }
}

impl Render for DetailPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tabs = vec![DetailTab::Overview];
        let containers = self.object.as_ref().map(container_names).unwrap_or_default();
        if self.obj.kind.is("", "Pod") {
            tabs.extend((0..containers.len()).map(DetailTab::Container));
        }
        if self.obj.kind.is("apps", "Deployment")
            || self.obj.kind.is("apps", "StatefulSet")
            || self.obj.kind.is("apps", "DaemonSet")
        {
            tabs.push(DetailTab::Revisions);
        }
        if self.obj.kind.ar.group == "apps" || self.obj.kind.is("batch", "Job") || self.obj.kind.is("", "Pod") {
            tabs.push(DetailTab::Map);
        }
        if self.logs.is_some() {
            tabs.push(DetailTab::Logs);
        }
        tabs.extend([DetailTab::Yaml, DetailTab::Compare, DetailTab::Events]);
        if self.terminal.is_some() {
            tabs.push(DetailTab::Shell);
        }
        if !tabs.contains(&self.tab) {
            self.tab = DetailTab::Overview;
        }
        let selected = tabs.iter().position(|t| *t == self.tab).unwrap_or(0);
        let body = match self.tab {
            DetailTab::Overview => self.render_overview(cx),
            DetailTab::Container(ix) => self.render_container(ix, cx),
            DetailTab::Compare => self.render_compare(cx),
            DetailTab::Map => match &self.object {
                Some(v) => crate::overview::relations(&self.ctx(v, cx), cx),
                None => div().into_any_element(),
            },
            DetailTab::Revisions => match &self.object {
                Some(v) => crate::overview::revisions(&self.ctx(v, cx), cx),
                None => div().into_any_element(),
            },
            DetailTab::Yaml => self.render_yaml(cx),
            DetailTab::Events => self.render_events(cx),
            DetailTab::Logs => {
                self.logs.clone().map(|l| l.into_any_element()).unwrap_or_else(|| div().into_any_element())
            }
            DetailTab::Shell => {
                self.terminal.clone().map(|t| t.into_any_element()).unwrap_or_else(|| div().into_any_element())
            }
        };
        let created = self
            .object
            .as_ref()
            .and_then(|o| o.pointer("/metadata/creationTimestamp"))
            .and_then(|v| v.as_str())
            .and_then(|t| t.parse::<jiff::Timestamp>().ok());
        let full_name = self.obj.name.clone();
        let actions = h_flex()
            .gap_1()
            .child(
                Button::new("download-yaml")
                    .small()
                    .ghost()
                    .icon(Lucide::FileDown)
                    .tooltip("Save the YAML to ~/Downloads (Secret values stay masked unless revealed)")
                    .on_click(cx.listener(|this, _, window, cx| this.download_yaml(window, cx))),
            )
            .child(self.render_actions(cx));
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.context.clone())
                            .child("·")
                            .child(div().text_color(theme.blue).child(self.obj.kind.ar.kind.clone())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .text_xl()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .when_some(self.obj.namespace.clone(), |d, ns| {
                                        d.child(div().text_color(theme.muted_foreground).child(format!("{ns} / ")))
                                    })
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(self.obj.name.clone())),
                            )
                            .child({
                                let fav = FavObject {
                                    context: self.context.clone(),
                                    kind: self.obj.kind.kubectl_name(),
                                    namespace: self.obj.namespace.clone(),
                                    name: self.obj.name.clone(),
                                };
                                let starred = self.favorites.read(cx).has_object(&fav);
                                let favs = self.favorites.clone();
                                Button::new("star")
                                    .xsmall()
                                    .ghost()
                                    .icon(if starred {
                                        Icon::new(IconName::StarFill).text_color(yellow())
                                    } else {
                                        Icon::new(Lucide::Star)
                                    })
                                    .on_click(move |_, _, cx| favs.update(cx, |f, cx| f.toggle_object(fav.clone(), cx)))
                            })
                            .child(Button::new("copy-name").xsmall().ghost().icon(Lucide::Copy).on_click(
                                move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(full_name.clone())),
                            ))
                            .when_some(created, |d, t| {
                                d.child(
                                    h_flex()
                                        .gap_1()
                                        .text_xs()
                                        .text_color(theme.success)
                                        .child(div().size_1p5().rounded_full().bg(theme.success))
                                        .child(k8s::age(t)),
                                )
                            })
                            .child(div().flex_1())
                            .child(actions),
                    ),
            )
            .child(
                // Keyed on the layout and selection: the underline's spring animation
                // caches tab positions, which went stale when tabs appeared before the
                // selected one or while streaming logs re-rendered every frame.
                TabBar::new(SharedString::from(format!("detail-tabs-{}-{selected}", tabs.len())))
                    .underline()
                    .px_2()
                    .selected_index(selected)
                    .children(tabs.iter().map(|t| {
                        match t {
                            DetailTab::Container(ix) => Tab::new()
                                .prefix(Icon::new(Lucide::Box).size_3p5())
                                .label(containers.get(*ix).cloned().unwrap_or_default()),
                            other => Tab::new().label(match other {
                                DetailTab::Overview => "Overview",
                                DetailTab::Revisions => "Revisions",
                                DetailTab::Map => "Map",
                                DetailTab::Compare => "Compare",
                                DetailTab::Yaml => "YAML",
                                DetailTab::Events => "Events",
                                DetailTab::Logs => "Logs",
                                DetailTab::Shell => "Shell",
                                DetailTab::Container(_) => "",
                            }),
                        }
                    }))
                    .on_click(move |ix, _, cx| {
                        let tab = tabs[*ix];
                        let _ = view.update(cx, |this, cx| {
                            this.tab = tab;
                            this.resume_logs_if_shown(cx);
                            cx.notify();
                        });
                    }),
            )
            .child(div().flex_1().min_h_0().p_2().child(body))
    }
}

pub(super) fn error_box(msg: SharedString, cx: &App) -> impl IntoElement {
    div()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().danger)
        .text_color(cx.theme().danger)
        .text_sm()
        .child(msg)
}
