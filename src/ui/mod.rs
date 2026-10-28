//! Pages and widgets that are not the main window's own: settings, agents' helpers,
//! pickers, terminal, Helm, apply, cluster and workloads overviews.

pub mod access_view;
pub mod apply_view;
pub mod assistant_view;
pub mod cluster;
pub mod ctx_picker;
pub mod dashboard;
pub mod data_edit;
pub mod field_docs;
pub mod helm;
pub mod helm_install;
pub mod kubeconfig_sources;
pub mod ns_picker;
pub mod palette;
pub mod problems_view;
pub mod schema_form;
pub mod selectable;
pub mod settings_view;
pub mod terminal;

use std::sync::{Arc, LazyLock};

use gpui_kit::{Image, ImageFormat, Img, Pixels, Styled as _, img};
use kube::Client;

/// What a page lists: each connected cluster with the namespaces to read there (`None`:
/// all of them at once; none: the cluster is left out).
pub type Scope = Vec<(String, Client, Vec<Option<String>>)>;

/// One list per cluster and namespace of `scope`: (what an error about it names,
/// context, client, namespace).
pub fn scope_lists(scope: Scope) -> Vec<(String, String, Client, Option<String>)> {
    let multi = scope.len() > 1;
    scope
        .into_iter()
        .flat_map(|(ctx, client, namespaces)| {
            namespaces.into_iter().map(move |ns| {
                let label = match &ns {
                    Some(ns) if multi => format!("{ctx}/{ns}"),
                    Some(ns) => ns.clone(),
                    None => ctx.clone(),
                };
                (label, ctx.clone(), client.clone(), ns)
            })
        })
        .collect()
}

/// The app's logo (the schooner, `assets/recouvrance.svg`) in its own colors.
pub fn logo(size: Pixels) -> Img {
    static LOGO: LazyLock<Arc<Image>> = LazyLock::new(|| {
        Arc::new(Image::from_bytes(ImageFormat::Svg, include_bytes!("../../assets/recouvrance.svg").to_vec()))
    });
    img(LOGO.clone()).size(size).flex_none()
}
