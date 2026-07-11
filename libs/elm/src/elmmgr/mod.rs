//! ELM 运行时管理 API。
//!
//! 这里是 ELM 开发侧推荐使用的 `elm::elmmgr::api::*` 命名空间。

pub mod api {
    pub use crate::developer::RuntimeApiError;
    pub use crate::developer::runtime_api::{
        abort_current, abort_panic, current_context, dispatch, features, invoke_managed, log,
        query_namespace,
    };
    #[cfg(feature = "runtime-model")]
    pub use crate::mgr::api::*;
}
