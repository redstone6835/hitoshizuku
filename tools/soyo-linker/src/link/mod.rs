//! ELF ET_REL 到 SOYO 映像的静态链接模型。

mod error;
mod layout;
mod model;
mod relocate;

pub use error::{LinkError, LinkErrorKind};
pub use layout::build_link_image;
pub use model::{
    InputObject, LinkImage, LinkRequest, LinkSegment, LinkSymbol, LinkedImage, PendingRelocation,
    RuntimeArrays, SymbolValue,
};
pub use relocate::apply_relocations;
