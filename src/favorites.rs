//! Starred resource kinds and objects, shown at the top of the sidebar.

use std::path::PathBuf;

use gpui_kit::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FavObject {
    pub context: String,
    /// kubectl-style kind, e.g. `deployments` or `certificates.cert-manager.io`.
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

/// A named table view: kind, namespaces and filter.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SavedView {
    pub name: String,
    pub kind: String,
    pub namespaces: Vec<String>,
    pub filter: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Stored {
    kinds: Vec<String>,
    objects: Vec<FavObject>,
    views: Vec<SavedView>,
}

pub struct Favorites {
    stored: Stored,
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("favorites.json")
}

impl Favorites {
    pub fn load() -> Self {
        Self { stored: crate::settings::load_json(&path()) }
    }

    fn save(&self) {
        save_json(&path(), &self.stored);
    }

    pub fn kinds(&self) -> &[String] {
        &self.stored.kinds
    }

    pub fn objects(&self) -> &[FavObject] {
        &self.stored.objects
    }

    pub fn toggle_kind(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(i) = self.stored.kinds.iter().position(|k| *k == key) {
            self.stored.kinds.remove(i);
        } else {
            self.stored.kinds.push(key);
        }
        self.save();
        cx.notify();
    }

    pub fn views(&self) -> &[SavedView] {
        &self.stored.views
    }

    /// Add a view, replacing one with the same name.
    pub fn save_view(&mut self, v: SavedView, cx: &mut Context<Self>) {
        self.stored.views.retain(|x| x.name != v.name);
        self.stored.views.push(v);
        self.save();
        cx.notify();
    }

    pub fn remove_view(&mut self, name: &str, cx: &mut Context<Self>) {
        self.stored.views.retain(|x| x.name != name);
        self.save();
        cx.notify();
    }

    pub fn has_object(&self, o: &FavObject) -> bool {
        self.stored.objects.contains(o)
    }

    pub fn toggle_object(&mut self, o: FavObject, cx: &mut Context<Self>) {
        if let Some(i) = self.stored.objects.iter().position(|x| *x == o) {
            self.stored.objects.remove(i);
        } else {
            self.stored.objects.push(o);
        }
        self.save();
        cx.notify();
    }
}

/// Columns chosen per resource type (kubectl name -> visible column names).
#[derive(Default)]
pub struct ColumnPrefs {
    /// Visible columns per kind, in display order.
    map: std::collections::HashMap<String, Vec<String>>,
    /// Column widths per kind (at zoom 1).
    widths: std::collections::HashMap<String, std::collections::HashMap<String, f32>>,
    /// Custom columns per kind: (header, JSONPath).
    custom: std::collections::HashMap<String, Vec<(String, String)>>,
}

/// Write JSON atomically (creating the directory); failures are reported on stderr.
fn save_json(path: &std::path::Path, value: &impl Serialize) {
    let written = serde_json::to_vec_pretty(value)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::private_files::write_atomic(path, &json));
    if let Err(e) = written {
        eprintln!("{}: {e}", path.display());
    }
}

fn custom_path() -> PathBuf {
    path().with_file_name("custom-columns.json")
}

fn columns_path() -> PathBuf {
    path().with_file_name("columns.json")
}

fn widths_path() -> PathBuf {
    path().with_file_name("column-widths.json")
}

impl ColumnPrefs {
    pub fn load() -> Self {
        use crate::settings::load_json;
        Self { map: load_json(&columns_path()), widths: load_json(&widths_path()), custom: load_json(&custom_path()) }
    }

    pub fn custom(&self, kind: &str) -> Vec<(String, String)> {
        self.custom.get(kind).cloned().unwrap_or_default()
    }

    /// Add (or replace by header) a custom column for a kind.
    pub fn add_custom(&mut self, kind: &str, header: String, path: String) {
        let list = self.custom.entry(kind.to_string()).or_default();
        list.retain(|(h, _)| *h != header);
        list.push((header, path));
        self.save_custom();
    }

    pub fn remove_custom(&mut self, kind: &str, header: &str) {
        if let Some(list) = self.custom.get_mut(kind) {
            list.retain(|(h, _)| h != header);
        }
        self.save_custom();
    }

    fn save_custom(&self) {
        save_json(&custom_path(), &self.custom);
    }

    pub fn widths(&self, kind: &str) -> std::collections::HashMap<String, f32> {
        self.widths.get(kind).cloned().unwrap_or_default()
    }

    pub fn set_widths(&mut self, kind: String, widths: std::collections::HashMap<String, f32>) {
        self.widths.entry(kind).or_default().extend(widths);
        save_json(&widths_path(), &self.widths);
    }

    pub fn get(&self, kind: &str) -> Option<Vec<String>> {
        self.map.get(kind).cloned()
    }

    pub fn set(&mut self, kind: String, visible: Option<Vec<String>>) {
        match visible {
            Some(v) => self.map.insert(kind, v),
            None => self.map.remove(&kind),
        };
        save_json(&columns_path(), &self.map);
    }
}
