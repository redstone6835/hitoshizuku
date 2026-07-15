//! ELM 运行时 API 命名空间注册表。
//!
//! 该注册表只发布 ELM 自身的普通运行时表和 Manager 控制表。allocator、设备及其他
//! 内核子系统不进入这里，它们由装载器通过直接内核符号目录解析。

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use elm_model::{
    ELM_API_NAMESPACE_FLAG_MANAGEMENT, ELM_API_NAMESPACE_FLAG_PUBLIC, ElmApiNamespaceDescriptorV1,
    ElmApiNamespaceV1, ElmId, Generation,
};
use sched::sync::Spinlock;

const NAMESPACE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiRegistryError {
    InvalidDescriptor,
    DuplicateNamespace,
    NamespaceUnavailable,
    VersionUnsupported,
    CapabilityDenied,
    OutOfMemory,
}

struct ApiRegistry {
    namespaces: Vec<&'static ElmApiNamespaceDescriptorV1>,
}

impl ApiRegistry {
    const fn new() -> Self {
        Self {
            namespaces: Vec::new(),
        }
    }

    fn initialize_capacity(&mut self) -> bool {
        let missing = NAMESPACE_CAPACITY.saturating_sub(self.namespaces.capacity());
        missing == 0 || self.namespaces.try_reserve_exact(missing).is_ok()
    }

    fn register(
        &mut self,
        descriptor: &'static ElmApiNamespaceDescriptorV1,
    ) -> Result<(), ApiRegistryError> {
        if !descriptor.validate() {
            return Err(ApiRegistryError::InvalidDescriptor);
        }
        if let Some(registered) = self.namespaces.iter().find(|registered| {
            registered.identifier == descriptor.identifier
                && registered.version == descriptor.version
        }) {
            return if registered.table_address == descriptor.table_address
                && registered.table_size == descriptor.table_size
                && registered.flags == descriptor.flags
                && registered.capabilities == descriptor.capabilities
            {
                Ok(())
            } else {
                Err(ApiRegistryError::DuplicateNamespace)
            };
        }
        self.namespaces
            .try_reserve(1)
            .map_err(|_| ApiRegistryError::OutOfMemory)?;
        self.namespaces.push(descriptor);
        Ok(())
    }

    fn query(
        &self,
        generation: Generation,
        identifier: &[u8],
        versions: &[u16],
        management_allowed: bool,
    ) -> Result<ElmApiNamespaceV1, ApiRegistryError> {
        if !self
            .namespaces
            .iter()
            .any(|descriptor| descriptor.identifier.as_bytes() == identifier)
        {
            return Err(ApiRegistryError::NamespaceUnavailable);
        }
        let descriptor = self
            .namespaces
            .iter()
            .copied()
            .filter(|descriptor| descriptor.identifier.as_bytes() == identifier)
            .filter(|descriptor| versions.contains(&descriptor.version))
            .max_by_key(|descriptor| descriptor.version)
            .ok_or(ApiRegistryError::VersionUnsupported)?;

        match descriptor.flags {
            ELM_API_NAMESPACE_FLAG_PUBLIC => {}
            ELM_API_NAMESPACE_FLAG_MANAGEMENT if management_allowed => {}
            _ => return Err(ApiRegistryError::CapabilityDenied),
        }

        Ok(ElmApiNamespaceV1 {
            struct_size: core::mem::size_of::<ElmApiNamespaceV1>() as u32,
            flags: 0,
            selected_version: descriptor.version,
            reserved0: 0,
            table_size: descriptor.table_size,
            table_address: descriptor.table_address as usize,
            generation: generation.0,
            capabilities: descriptor.capabilities,
        })
    }
}

static API_REGISTRY: Spinlock<ApiRegistry> = Spinlock::new(ApiRegistry::new());

pub(crate) fn init() -> bool {
    API_REGISTRY.lock().initialize_capacity()
}

pub(crate) fn register(
    descriptor: &'static ElmApiNamespaceDescriptorV1,
) -> Result<(), ApiRegistryError> {
    API_REGISTRY.lock().register(descriptor)
}

pub(crate) fn query(
    _cell: ElmId,
    generation: Generation,
    identifier: &[u8],
    versions: &[u16],
    management_allowed: bool,
) -> Result<ElmApiNamespaceV1, ApiRegistryError> {
    API_REGISTRY
        .lock()
        .query(generation, identifier, versions, management_allowed)
}

pub(crate) fn diagnostic_text() -> String {
    let registry = API_REGISTRY.lock();
    let mut out = format!("runtime_namespaces={}\n", registry.namespaces.len());
    for descriptor in &registry.namespaces {
        out.push_str(
            format!(
                "runtime_namespace identifier={} version={} flags=0x{:x} capabilities=0x{:x} table_size={}\n",
                descriptor.identifier,
                descriptor.version,
                descriptor.flags,
                descriptor.capabilities,
                descriptor.table_size,
            )
            .as_str(),
        );
    }
    out
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_runtime_namespace_roundtrip() -> bool {
    static TEST_TABLE: [u64; 2] = [1, 2];
    static DESCRIPTOR: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
        "elm.test.runtime",
        1,
        ELM_API_NAMESPACE_FLAG_PUBLIC,
        0x3,
        &TEST_TABLE,
    );

    let mut registry = ApiRegistry::new();
    registry.initialize_capacity()
        && registry.register(&DESCRIPTOR).is_ok()
        && registry
            .query(Generation::FIRST, b"elm.test.runtime", &[1], false)
            .is_ok_and(|namespace| {
                namespace.selected_version == 1
                    && namespace.table_address == TEST_TABLE.as_ptr() as usize
                    && namespace.capabilities == 0x3
            })
}
