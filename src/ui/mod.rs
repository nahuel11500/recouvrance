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

/// The app's logo (the schooner, `assets/recouvrance.svg`) in its own colors.
pub fn logo(size: Pixels) -> Img {
    static LOGO: LazyLock<Arc<Image>> = LazyLock::new(|| {
        Arc::new(Image::from_bytes(ImageFormat::Svg, include_bytes!("../../assets/recouvrance.svg").to_vec()))
    });
    img(LOGO.clone()).size(size).flex_none()
}
