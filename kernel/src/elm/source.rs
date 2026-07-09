//! ELM EBI Source 注册表。
//!
//! ELM Core 只消费 EBI 协议对象。Projection Source 通过这里的 provider 注册表把外部
//! payload 投影成 `ElmEbiImage`，但不把 soyo 或其他具体容器格式写入核心。

use alloc::vec::Vec;

use elm_model::{ElmEbiArch, ElmEbiImage, ElmEbiLoadStatus, parse_eki_image};
use sched::sync::Spinlock;

pub(crate) type ElmProjectionSourceProvider =
    fn(payload: &[u8], arch: ElmEbiArch) -> Result<ElmEbiImage, ElmEbiLoadStatus>;

#[derive(Clone, Copy)]
struct ProjectionSourceProviderRuntime {
    id: u64,
    provider: ElmProjectionSourceProvider,
}

static PROJECTION_SOURCES: Spinlock<Vec<ProjectionSourceProviderRuntime>> =
    Spinlock::new(Vec::new());

pub(crate) fn register_projection_source(id: u64, provider: ElmProjectionSourceProvider) -> bool {
    if id == 0 {
        return false;
    }
    let mut sources = PROJECTION_SOURCES.lock();
    if sources.iter().any(|source| source.id == id) {
        return false;
    }
    sources.push(ProjectionSourceProviderRuntime { id, provider });
    true
}

pub(crate) fn project_ebi_image(
    id: u64,
    payload: &[u8],
    arch: ElmEbiArch,
) -> Result<ElmEbiImage, ElmEbiLoadStatus> {
    let provider = {
        let sources = PROJECTION_SOURCES.lock();
        sources
            .iter()
            .find(|source| source.id == id)
            .map(|source| source.provider)
    };
    match provider {
        Some(provider) => provider(payload, arch),
        None => Err(ElmEbiLoadStatus::RuntimeRejected),
    }
}

pub(crate) fn project_builtin_eki_image(
    payload: &[u8],
    arch: ElmEbiArch,
) -> Result<ElmEbiImage, ElmEbiLoadStatus> {
    // 这是内建 `eki` 子单元的投影入口；管理通道只选择 Source，不直接拥有格式解析。
    let image = parse_eki_image(payload)?;
    image.validate(arch)?;
    Ok(image)
}
