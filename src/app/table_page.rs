//! The resource list page: filter menu, columns, selection and bulk actions, views, CSV.

#[allow(unused_imports)]
use super::*;

impl AppView {
    /// Row menu: open the right-clicked row, optionally on a tab.
    pub(super) fn open_row(&mut self, tab: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ctx, row)) = self.row_object(cx) else { return };
        self.open_row_detail(ctx, row, true, window, cx);
        if let (Some(tab), Some(d)) = (tab, self.detail().cloned()) {
            d.update(cx, |d, cx| d.select_tab_named(tab, cx));
        }
    }

    /// Run, scale, restart or delete the right-clicked object itself (an event
    /// row never redirects to the object it is about here).
    pub(super) fn row_quick_action(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ctx, row)) = self.row_object(cx) else { return };
        if !self.open_row_detail(ctx, row, false, window, cx) {
            return;
        }
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.quick_action(action, window, cx));
        }
    }

    /// The row the context menu was opened on, found by uid (it may have moved since).
    pub(super) fn row_object(&self, cx: &App) -> Option<(usize, Arc<k8s::Row>)> {
        let d = self.table.read(cx).delegate();
        let (ctx, uid) = d.menu_row.as_ref()?;
        Some((*ctx, d.find_row(*ctx, uid)?))
    }

    pub(super) fn toggle_column(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(kind) = self.kind.as_ref().map(|k| k.kubectl_name()) else { return };
        let mut visible: Vec<String> = self.table.read(cx).delegate().shown_columns();
        if let Some(i) = visible.iter().position(|n| *n == name) {
            visible.remove(i);
        } else {
            visible.push(name);
        }
        self.column_prefs.set(kind, Some(visible.clone()));
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_visible_columns(Some(visible));
            t.refresh(cx);
        });
        // Hiding the sorted column puts the rows back in the server's order.
        self.follow_selection(cx);
        cx.notify();
    }

    pub(super) fn reset_columns(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.kind.as_ref().map(|k| k.kubectl_name()) else { return };
        self.column_prefs.set(kind, None);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_visible_columns(None);
            t.refresh(cx);
        });
        self.follow_selection(cx);
        cx.notify();
    }

    /// Add the custom column typed in the Columns menu, then reload the table.
    pub(super) fn add_custom_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.kind.as_ref().map(|k| k.kubectl_name()) else { return };
        let header = self.col_header.read(cx).value().trim().to_string();
        let path = self.col_path.read(cx).value().trim().to_string();
        if header.is_empty() || path.is_empty() {
            return;
        }
        self.column_prefs.add_custom(&kind, header, path);
        self.col_header.update(cx, |s, cx| s.set_value("", window, cx));
        self.col_path.update(cx, |s, cx| s.set_value("", window, cx));
        self.restart_watch(cx);
    }

    pub fn apply_filter_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        crate::settings::remember_filter(&text);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_filter(text);
            cx.notify();
        });
        self.follow_selection(cx);
    }

    /// Recent filters, saved searches and the filter syntax.
    pub(super) fn render_filter_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = crate::settings::get();
        let current = self.filter.read(cx).value().trim().to_string();
        let muted = cx.theme().muted_foreground;
        let item = |id: SharedString, text: String, cx: &mut Context<Self>| {
            let t = text.clone();
            div()
                .id(id)
                .px_1()
                .py_0p5()
                .rounded_sm()
                .text_sm()
                .font_family(cx.theme().mono_font_family.clone())
                .cursor_pointer()
                .hover(|d| d.bg(cx.theme().list_hover))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(text)
                .on_click(cx.listener(move |this, _, w, cx| this.apply_filter_text(t.clone(), w, cx)))
        };
        Popover::new("filter-popover")
            .anchor(Anchor::TopLeft)
            .trigger(Button::new("filter-menu").small().ghost().icon(Lucide::ClockArrowDown).tooltip("Recent and saved filters"))
            .child(
                v_flex()
                    .w(px(420.))
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_xs().text_color(muted).child("SAVED SEARCHES"))
                            .child(div().flex_1())
                            .when(!current.is_empty() && !st.saved_searches.contains(&current), |d| {
                                let c = current.clone();
                                d.child(Button::new("save-search").xsmall().ghost().icon(Lucide::Bookmark).label("Save current").on_click(cx.listener(move |_, _, _, cx| {
                                    let c = c.clone();
                                    crate::settings::update(|s| s.saved_searches.push(c));
                                    cx.notify();
                                })))
                            }),
                    )
                    .when(st.saved_searches.is_empty(), |d| d.child(div().text_xs().text_color(muted).child("None yet: type a filter, then Save current.")))
                    .children(st.saved_searches.iter().enumerate().map(|(i, f)| {
                        let f2 = f.clone();
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().min_w_0().child(item(SharedString::from(format!("saved-{i}")), f.clone(), cx)))
                            .child(Button::new(("rm-search", i)).xsmall().ghost().icon(Lucide::X).on_click(cx.listener(move |_, _, _, cx| {
                                let f = f2.clone();
                                crate::settings::update(|s| s.saved_searches.retain(|x| *x != f));
                                cx.notify();
                            })))
                    }))
                    .child(div().pt_2().text_xs().text_color(muted).child("RECENT"))
                    .when(st.filter_history.is_empty(), |d| d.child(div().text_xs().text_color(muted).child("Filters you use show up here.")))
                    .children(st.filter_history.iter().take(15).enumerate().map(|(i, f)| item(SharedString::from(format!("recent-{i}")), f.clone(), cx)))
                    .child(div().pt_2().text_xs().text_color(muted).child("SYNTAX"))
                    .child(
                        div().text_xs().text_color(muted).child(
                            "words (all must match) · !word excludes · app=web, app!=web, l:key (labels) · ns:team-* · /regex/ on names · \
                             column:value (status:Running, node:worker-*) · column>value, <, >=, <= (restarts>3, cpu>200m, memory>1Gi, age<1h)",
                        ),
                    ),
            )
    }

    pub(super) fn render_columns_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices = self.table.read(cx).delegate().column_choices();
        let kind_name = self.kind.as_ref().map(|k| k.kubectl_name()).unwrap_or_default();
        let custom = self.column_prefs.custom(&kind_name);
        Popover::new("columns-popover")
            .anchor(Anchor::TopRight)
            .trigger(Button::new("columns").small().ghost().icon(Lucide::Columns3).tooltip("Columns"))
            .child(
                v_flex()
                    .w(px(280.))
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Columns"))
                    .children(choices.into_iter().enumerate().map(|(i, (name, shown))| {
                        let n = name.clone();
                        Checkbox::new(("col", i))
                            .label(name)
                            .checked(shown)
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_column(n.clone(), cx)))
                    }))
                    .child(
                        Button::new("cols-reset")
                            .xsmall()
                            .ghost()
                            .label("Reset to defaults")
                            .on_click(cx.listener(|this, _, _, cx| this.reset_columns(cx))),
                    )
                    .child(
                        div()
                            .pt_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Custom columns (JSONPath)"),
                    )
                    .children(custom.into_iter().enumerate().map(|(i, (h, p))| {
                        let (k, h2) = (kind_name.clone(), h.clone());
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .child(div().font_weight(FontWeight::MEDIUM).child(h))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(p),
                            )
                            .child(Button::new(("rm-col", i)).xsmall().ghost().icon(Lucide::X).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.column_prefs.remove_custom(&k, &h2);
                                    this.restart_watch(cx);
                                },
                            )))
                    }))
                    .child(Input::new(&self.col_header).small())
                    .child(Input::new(&self.col_path).small())
                    .child(
                        Button::new("add-col")
                            .xsmall()
                            .outline()
                            .icon(Lucide::Plus)
                            .label("Add column")
                            .on_click(cx.listener(|this, _, window, cx| this.add_custom_column(window, cx))),
                    ),
            )
    }

    /// Shown while rows are checked: bulk actions on them.
    pub(super) fn render_selection_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.table.read(cx).delegate();
        let n = t.marked_rows().len();
        if n == 0 {
            return None;
        }
        let (visible, acts) = (t.rows_count_visible(), t.row_actions.clone());
        let hidden = n - t.marked_visible_rows().len();
        let label =
            if hidden > 0 { format!("{n} selected ({hidden} hidden by the filter)") } else { format!("{n} selected") };
        let theme = cx.theme();
        Some(
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .bg(theme.blue.opacity(0.08))
                .border_b_1()
                .border_color(theme.border)
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                .child(Button::new("sel-all").ghost().small().label(format!("Toggle all {visible} shown")).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.table.update(cx, |t, cx| {
                            t.delegate_mut().toggle_all_visible();
                            cx.notify();
                        });
                    }),
                ))
                .child(div().flex_1())
                .child(Button::new("sel-copy").ghost().small().icon(Lucide::Copy).label("Copy names").on_click(
                    cx.listener(|this, _, _, cx| {
                        let names: Vec<String> =
                            this.table.read(cx).delegate().marked_rows().iter().map(|(_, r)| r.name.clone()).collect();
                        cx.write_to_clipboard(ClipboardItem::new_string(names.join("\n")));
                    }),
                ))
                .when(acts.contains(&"scale"), |d| {
                    d.child(
                        Button::new("sel-scale")
                            .ghost()
                            .small()
                            .icon(Lucide::ArrowUpDown)
                            .label("Scale")
                            .on_click(cx.listener(|this, _, w, cx| this.bulk_action("Scale", w, cx))),
                    )
                })
                .when(acts.contains(&"restart"), |d| {
                    d.child(
                        Button::new("sel-restart")
                            .ghost()
                            .small()
                            .icon(Lucide::RotateCw)
                            .label("Restart")
                            .on_click(cx.listener(|this, _, w, cx| this.bulk_action("Restart", w, cx))),
                    )
                })
                .when(acts.contains(&"delete"), |d| {
                    d.child(
                        Button::new("sel-delete")
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Trash).text_color(theme.danger))
                            .label("Delete")
                            .on_click(cx.listener(|this, _, w, cx| this.bulk_action("Delete", w, cx))),
                    )
                })
                .child(
                    Button::new("sel-clear").ghost().small().icon(IconName::Close).tooltip("Clear selection").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.table.update(cx, |t, cx| {
                                t.delegate_mut().marked.clear();
                                cx.notify();
                            });
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    /// Restart or delete every checked row, after one confirmation that lists them.
    pub(super) fn bulk_action(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let Some(kind) = self.kind.clone() else { return };
        // Only the checked rows shown: the filter may hide some that were checked before.
        let (rows, hidden) = {
            let d = self.table.read(cx).delegate();
            let rows = d.marked_visible_rows();
            let hidden = d.marked_rows().len() - rows.len();
            (rows, hidden)
        };
        if rows.is_empty() {
            window.push_notification(Notification::info("No checked rows are shown: clear the filter first"), cx);
            return;
        }
        let n = rows.len();
        let targets: Vec<(String, Client, Arc<k8s::Row>)> = rows
            .into_iter()
            .filter_map(|(ctx, r)| self.clusters.get(ctx).map(|(name, client)| (name.clone(), client.clone(), r)))
            .collect();
        let mut protected: Vec<String> =
            targets.iter().map(|t| t.0.clone()).filter(|c| crate::settings::is_protected(c)).collect();
        protected.sort();
        protected.dedup();
        let ctxs: std::collections::BTreeSet<&String> = targets.iter().map(|t| &t.0).collect();
        // Rows from several clusters say which one each is on.
        let several = ctxs.len() > 1;
        let preview: Vec<String> = targets
            .iter()
            .take(8)
            .map(|(ctx, _, r)| bulk_row_label(several.then_some(ctx.as_str()), r.namespace.as_deref(), &r.name))
            .chain((n > 8).then(|| format!("…and {} more", n - 8)))
            .collect();
        // What deleting this kind asks (Settings → General); scaling, or a protected
        // context, always needs the count typed.
        let level = crate::settings::delete_confirm(&kind.ar.kind);
        if action == "Delete" && level == crate::settings::DeleteConfirm::Off && protected.is_empty() {
            self.run_bulk(action, kind, targets, 0, window, cx);
            return;
        }
        let typed = ((action == "Delete" && level == crate::settings::DeleteConfirm::Type)
            || action == "Scale"
            || !protected.is_empty())
        .then(|| cx.new(|cx| InputState::new(window, cx).placeholder(n.to_string())));
        // No count is suggested: it has to be typed.
        let replicas = (action == "Scale").then(|| cx.new(|cx| InputState::new(window, cx).placeholder("replicas")));
        // Each protected context's name typed, like for a single write there.
        let guards: Vec<(Entity<InputState>, String)> = protected
            .iter()
            .map(|c| (cx.new(|cx| InputState::new(window, cx).placeholder(c.clone())), c.clone()))
            .collect();
        let names: Vec<String> = targets.iter().map(|t| crate::details::shell_word(&t.2.name)).collect();
        let namespaces: std::collections::BTreeSet<Option<&str>> =
            targets.iter().map(|t| t.2.namespace.as_deref()).collect();
        let (ns, ns_note) = match namespaces.len() {
            1 => (targets[0].2.namespace.as_deref(), ""),
            _ => (None, " (several namespaces)"),
        };
        let args = match action {
            "Delete" => format!("delete {} {}", kind.kubectl_name(), names.join(" ")),
            "Scale" => format!("scale {} {} --replicas=…", kind.kubectl_name(), names.join(" ")),
            _ => format!("rollout restart {} {}", kind.kubectl_name(), names.join(" ")),
        };
        let cmd = match ctxs.first().filter(|_| !several) {
            Some(ctx) => format!("{}{ns_note}", crate::details::kubectl(ctx, ns, &args)),
            None => format!("kubectl (on {} contexts){ns_note} {args}", ctxs.len()),
        };
        let cmd = if cmd.len() > 300 { format!("{}…", &cmd[..cmd.floor_char_boundary(300)]) } else { cmd };
        let this = cx.entity().downgrade();
        let label = kind.label.clone();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (this, typed, kind, targets, replicas, guards) =
                (this.clone(), typed.clone(), kind.clone(), targets.clone(), replicas.clone(), guards.clone());
            let theme = cx.theme();
            alert
                .title(format!("{action} {n} {label}?"))
                .description(
                    [
                        (!protected.is_empty())
                            .then(|| format!("Includes protected contexts: {}.", protected.join(", "))),
                        (hidden > 0).then(|| {
                            format!(
                                "{hidden} checked row{} hidden by the filter {} left out.",
                                if hidden > 1 { "s" } else { "" },
                                if hidden > 1 { "are" } else { "is" }
                            )
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
                )
                .child(v_flex().gap_0p5().max_h(px(200.)).overflow_hidden().children(preview.iter().map(|p| {
                    div().text_sm().font_family("monospace").text_color(theme.muted_foreground).child(p.clone())
                })))
                .when_some(replicas.clone(), |a, input| {
                    a.child(div().pt_1().text_sm().child("New replica count for all of them:"))
                        .child(Input::new(&input))
                })
                .child(crate::details::confirm_extras(&cmd, None, cx))
                .children(guards.iter().map(|g| crate::details::confirm_extras("", Some(g), cx)))
                .when_some(typed.clone(), |a, input| {
                    a.child(div().pt_1().text_sm().child(format!("Type {n} to confirm:"))).child(Input::new(&input))
                })
                .when(action == "Delete", |a| a.child(crate::details::delete_settings_link(&kind.ar.kind, &label, cx)))
                .show_cancel(true)
                .ok_text(action)
                .when(action == "Delete", |a| a.ok_variant(component::button::ButtonVariant::Danger))
                .on_ok(move |_, window, cx| {
                    if typed.as_ref().is_some_and(|i| i.read(cx).value().trim() != n.to_string()) {
                        return false;
                    }
                    if !guards.iter().all(|g| crate::details::guard_ok(Some(g), cx)) {
                        return false;
                    }
                    let count: i32 = match &replicas {
                        Some(i) => match crate::details::parse_replicas(&i.read(cx).value()) {
                            Some(v) => v,
                            None => return false,
                        },
                        None => 0,
                    };
                    let (targets, kind) = (targets.clone(), kind.clone());
                    let _ = this.update(cx, |this, cx| this.run_bulk(action, kind, targets, count, window, cx));
                    true
                })
        });
    }

    /// Delete, scale or restart the checked rows, and say how it went.
    fn run_bulk(
        &mut self,
        action: &'static str,
        kind: Kind,
        targets: Vec<(String, Client, Arc<k8s::Row>)>,
        count: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = targets.len();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().marked.clear();
            cx.notify();
        });
        cx.spawn_in(window, async move |_, cx| {
            let jobs = targets.into_iter().map(|(_, client, r)| {
                let (kind, ns, name) = (kind.clone(), r.namespace.clone(), r.name.clone());
                async move {
                    match action {
                        "Delete" => k8s::delete(client, kind, ns, name.clone(), false).await,
                        "Scale" => k8s::scale(client, kind, ns, name.clone(), count, false).await,
                        _ => k8s::rollout_restart(client, kind, ns, name.clone(), false).await,
                    }
                    .map_err(|e| format!("{name}: {e:#}"))
                }
            });
            let results = futures::future::join_all(jobs).await;
            let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
            let ok = n - errors.len();
            let _ = cx.update(|window, cx| {
                let past = match action {
                    "Delete" => "deleted",
                    "Scale" => "scaled",
                    _ => "restarted",
                };
                if errors.is_empty() {
                    window.push_notification(Notification::success(format!("{ok} {past}")), cx);
                } else {
                    window.push_notification(
                        Notification::error(format!(
                            "{ok} {past}, {} failed. {}",
                            errors.len(),
                            errors.first().cloned().unwrap_or_default()
                        )),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Open a saved view: its kind, namespaces and filter.
    pub(super) fn apply_view(&mut self, v: &SavedView, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = k8s::find_kind(&self.kinds, &v.kind).cloned() else {
            window.push_notification(
                Notification::error(format!("{} is not available on the connected clusters", v.kind)),
                cx,
            );
            return;
        };
        let namespaces = v.namespaces.clone();
        self.ns_picker.update(cx, |p, cx| {
            p.selected = namespaces.iter().cloned().collect();
            cx.notify();
        });
        // Another kind restarts the watch when it is shown; the same kind needs it here.
        let same_kind = self.kind.as_ref().map(|k| k.key()) == Some(kind.key());
        self.choose_namespaces(namespaces, cx);
        self.select_kind(kind, cx);
        if same_kind {
            self.restart_watch(cx);
        }
        let filter = v.filter.clone();
        self.filter.update(cx, |s, cx| s.set_value(filter.clone(), window, cx));
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_filter(filter);
            cx.notify();
        });
    }

    /// Save the rows shown (filtered, sorted, visible columns) as CSV in ~/Downloads.
    pub(super) fn export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.kind.clone() else { return };
        let csv = self.table.read(cx).delegate().to_csv();
        let dir = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads");
        let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
        let path = dir.join(format!("{}-{stamp}.csv", kind.ar.plural));
        let r = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, csv));
        window.push_notification(
            match r {
                Ok(()) => Notification::success(format!("Saved {}", path.display())),
                Err(e) => Notification::error(format!("Could not save CSV: {e}")),
            },
            cx,
        );
    }

    /// Name and save the current kind, namespaces and filter.
    pub(super) fn save_view_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.kind.clone() else { return };
        let filter = self.filter.read(cx).value().to_string();
        let default = if filter.is_empty() { kind.label.clone() } else { format!("{} · {filter}", kind.label) };
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx);
            s.set_value(default, window, cx);
            s
        });
        let view =
            SavedView { name: String::new(), kind: kind.kubectl_name(), namespaces: self.namespaces.clone(), filter };
        let favs = self.favorites.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (input, view, favs) = (input.clone(), view.clone(), favs.clone());
            alert
                .title("Save view")
                .description("Keeps this resource type, the selected namespaces and the filter in the sidebar.")
                .child(Input::new(&input))
                .show_cancel(true)
                .ok_text("Save")
                .on_ok(move |_, _, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return false;
                    }
                    let v = SavedView { name, ..view.clone() };
                    favs.update(cx, |f, cx| f.save_view(v, cx));
                    true
                })
        });
    }

    pub(crate) fn open_detail(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = self.table.read(cx).delegate();
        let (Some(row), Some(ctx)) = (delegate.row(ix).cloned(), delegate.row_context(ix)) else {
            return;
        };
        self.open_row_detail(ctx, row, true, window, cx);
    }

    /// Open a row's details; with `follow_event`, an event opens the object it is
    /// about ("pod/web-1") instead. False when nothing was opened.
    fn open_row_detail(
        &mut self,
        ctx: usize,
        row: Arc<k8s::Row>,
        follow_event: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(kind) = self.kind.clone() else { return false };
        self.table.update(cx, |t, _| t.delegate_mut().selected_uid = Some(row.uid.clone()));
        if follow_event && kind.is("", "Event") {
            let target = self.table.read(cx).delegate().cells_text(&row, "Object").map(String::from);
            if let Some((k, name)) = target.as_deref().and_then(|o| o.split_once('/')) {
                self.open_object_in(ctx, k, name.to_string(), row.namespace.clone(), window, cx);
                return true;
            }
        }
        let obj = ObjectRef { kind, name: row.name.clone(), namespace: row.namespace.clone(), uid: row.uid.clone() };
        self.show_detail_in(ctx, obj, window, cx);
        true
    }
}

/// A row in a bulk confirmation: `ns/name`, after its context when rows span several.
fn bulk_row_label(context: Option<&str>, ns: Option<&str>, name: &str) -> String {
    let obj = ns.map(|ns| format!("{ns}/{name}")).unwrap_or_else(|| name.to_string());
    context.map(|c| format!("{c}: {obj}")).unwrap_or(obj)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bulk_rows_name_their_context() {
        assert_eq!(super::bulk_row_label(None, Some("team-a"), "web"), "team-a/web");
        assert_eq!(super::bulk_row_label(Some("prod"), Some("team-a"), "web"), "prod: team-a/web");
        assert_eq!(super::bulk_row_label(Some("prod"), None, "node-1"), "prod: node-1");
    }
}
