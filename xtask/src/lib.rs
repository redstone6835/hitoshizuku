//! Shared platform catalog used by host-side build orchestration and build scripts.

mod platform;

pub use platform::{
    CATALOG_RELATIVE_PATH, CatalogError, HexAddress, ImageFormat, ImageSpec, LinkLayout, LinkSpec,
    PlatformCatalog, PlatformSpec, UImageSpec,
};
