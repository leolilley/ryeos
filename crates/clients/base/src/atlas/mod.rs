//! Renderer-independent RyeOS namespace atlas.
//!
//! The atlas is a shared spatial language for clients. It turns RyeOS item
//! namespaces into a deterministic radial layout that web, terminal, and other
//! renderers can present at different fidelity levels.

pub mod build;
pub mod layout;
pub mod model;
pub mod text;

pub use build::{
    build_file_space_atlas, build_namespace_atlas, AtlasFileInput, AtlasFileSpaceInput, AtlasInput,
    AtlasItemInput,
};
pub use model::{
    AtlasBoundsVm, AtlasItemKind, AtlasLensVm, AtlasLinkVm, AtlasNodeVm, AtlasProjectionVm,
    AtlasRegionVm, AtlasScope, AtlasStackItemVm, AtlasUiStateVm, AtlasVisualStateVm,
    NamespaceAtlasVm,
};
