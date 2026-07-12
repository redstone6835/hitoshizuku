//! ELM Kernel API 命名空间与按代授权注册表。

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use elm_model::{
    ELM_API_NAMESPACE_FLAG_MANAGEMENT, ELM_API_NAMESPACE_FLAG_PUBLIC,
    ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT, ElmApiNamespaceDescriptorV1, ElmApiNamespaceV1,
    ElmEbiKernelApiRequirement, ElmId, Generation,
};
use sched::sync::Spinlock;

const NAMESPACE_CAPACITY: usize = 64;
const GRANT_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiRegistryError {
    InvalidDescriptor,
    DuplicateNamespace,
    NamespaceUnavailable,
    VersionUnsupported,
    CapabilityDenied,
    LayoutMismatch,
    DuplicateGrant,
    CounterExhausted,
    OutOfMemory,
}

#[derive(Debug, Clone)]
struct ApiGrant {
    id: u64,
    cell: ElmId,
    generation: Generation,
    descriptor_index: usize,
    capabilities: u64,
}

struct ApiRegistry {
    namespaces: Vec<&'static ElmApiNamespaceDescriptorV1>,
    grants: Vec<ApiGrant>,
    next_grant_id: u64,
}

impl ApiRegistry {
    const fn new() -> Self {
        Self {
            namespaces: Vec::new(),
            grants: Vec::new(),
            next_grant_id: 1,
        }
    }

    fn initialize_capacity(&mut self) -> bool {
        let namespace_missing = NAMESPACE_CAPACITY.saturating_sub(self.namespaces.capacity());
        if namespace_missing != 0
            && self
                .namespaces
                .try_reserve_exact(namespace_missing)
                .is_err()
        {
            return false;
        }
        let grant_missing = GRANT_CAPACITY.saturating_sub(self.grants.capacity());
        grant_missing == 0 || self.grants.try_reserve_exact(grant_missing).is_ok()
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
                && registered.layout_hash == descriptor.layout_hash
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

    fn grant_requirements(
        &mut self,
        cell: ElmId,
        generation: Generation,
        requirements: &[ElmEbiKernelApiRequirement],
    ) -> Result<usize, ApiRegistryError> {
        // TODO: 发布首个真实 kernel.* 命名空间前，把签名、内建来源或管理员批准接入此授权事务。
        if requirements.is_empty() {
            return Ok(0);
        }
        let mut resolved = Vec::new();
        resolved
            .try_reserve_exact(requirements.len())
            .map_err(|_| ApiRegistryError::OutOfMemory)?;
        for requirement in requirements {
            let Some((descriptor_index, descriptor)) =
                self.namespaces.iter().enumerate().find(|(_, descriptor)| {
                    descriptor.identifier == requirement.identifier
                        && descriptor.version == requirement.version
                })
            else {
                return Err(ApiRegistryError::NamespaceUnavailable);
            };
            if descriptor.flags != ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT {
                return Err(ApiRegistryError::CapabilityDenied);
            }
            if requirement.required_capabilities & !descriptor.capabilities != 0 {
                return Err(ApiRegistryError::CapabilityDenied);
            }
            if requirement.layout_hash != descriptor.layout_hash {
                return Err(ApiRegistryError::LayoutMismatch);
            }
            if self.grants.iter().any(|grant| {
                grant.cell == cell
                    && grant.generation == generation
                    && grant.descriptor_index == descriptor_index
            }) || resolved
                .iter()
                .any(|grant: &ApiGrant| grant.descriptor_index == descriptor_index)
            {
                return Err(ApiRegistryError::DuplicateGrant);
            }
            resolved.push(ApiGrant {
                id: 0,
                cell,
                generation,
                descriptor_index,
                capabilities: requirement.required_capabilities,
            });
        }
        self.grants
            .try_reserve(resolved.len())
            .map_err(|_| ApiRegistryError::OutOfMemory)?;
        let count = resolved.len();
        let count_u64 = u64::try_from(count).map_err(|_| ApiRegistryError::CounterExhausted)?;
        let next_grant_id = self
            .next_grant_id
            .checked_add(count_u64)
            .ok_or(ApiRegistryError::CounterExhausted)?;
        for (offset, grant) in resolved.iter_mut().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| ApiRegistryError::CounterExhausted)?;
            grant.id = self
                .next_grant_id
                .checked_add(offset)
                .ok_or(ApiRegistryError::CounterExhausted)?;
        }
        self.next_grant_id = next_grant_id;
        self.grants.extend(resolved);
        Ok(count)
    }

    fn query(
        &self,
        cell: ElmId,
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
        let (descriptor_index, descriptor) = self
            .namespaces
            .iter()
            .enumerate()
            .filter(|(_, descriptor)| descriptor.identifier.as_bytes() == identifier)
            .filter(|(_, descriptor)| versions.contains(&descriptor.version))
            .max_by_key(|(_, descriptor)| descriptor.version)
            .ok_or(ApiRegistryError::VersionUnsupported)?;

        let (grant_id, capabilities) = match descriptor.flags {
            ELM_API_NAMESPACE_FLAG_PUBLIC => (0, descriptor.capabilities),
            ELM_API_NAMESPACE_FLAG_MANAGEMENT if management_allowed => (0, descriptor.capabilities),
            ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT => {
                let grant = self
                    .grants
                    .iter()
                    .find(|grant| {
                        grant.cell == cell
                            && grant.generation == generation
                            && grant.descriptor_index == descriptor_index
                    })
                    .ok_or(ApiRegistryError::CapabilityDenied)?;
                (grant.id, grant.capabilities)
            }
            _ => return Err(ApiRegistryError::CapabilityDenied),
        };

        Ok(ElmApiNamespaceV1 {
            struct_size: core::mem::size_of::<ElmApiNamespaceV1>() as u32,
            flags: 0,
            selected_version: descriptor.version,
            reserved0: 0,
            table_size: descriptor.table_size,
            table_address: descriptor.table_address as usize,
            generation: generation.0,
            grant_id,
            capabilities,
        })
    }

    fn authorize(
        &self,
        grant_id: u64,
        cell: ElmId,
        generation: Generation,
        identifier: &[u8],
        version: u16,
        required_capabilities: u64,
    ) -> Result<(), ApiRegistryError> {
        let (descriptor_index, descriptor) = self
            .namespaces
            .iter()
            .enumerate()
            .find(|(_, descriptor)| {
                descriptor.identifier.as_bytes() == identifier && descriptor.version == version
            })
            .ok_or_else(|| {
                if self
                    .namespaces
                    .iter()
                    .any(|descriptor| descriptor.identifier.as_bytes() == identifier)
                {
                    ApiRegistryError::VersionUnsupported
                } else {
                    ApiRegistryError::NamespaceUnavailable
                }
            })?;
        if descriptor.flags != ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT || grant_id == 0 {
            return Err(ApiRegistryError::CapabilityDenied);
        }
        let grant = self
            .grants
            .iter()
            .find(|grant| grant.id == grant_id)
            .ok_or(ApiRegistryError::CapabilityDenied)?;
        if grant.cell != cell
            || grant.generation != generation
            || grant.descriptor_index != descriptor_index
            || required_capabilities & !grant.capabilities != 0
            || required_capabilities & !descriptor.capabilities != 0
        {
            return Err(ApiRegistryError::CapabilityDenied);
        }
        Ok(())
    }

    fn remove_generation(&mut self, cell: ElmId, generation: Generation) -> usize {
        let before = self.grants.len();
        self.grants
            .retain(|grant| grant.cell != cell || grant.generation != generation);
        before - self.grants.len()
    }

    fn remove_cell(&mut self, cell: ElmId) -> usize {
        let before = self.grants.len();
        self.grants.retain(|grant| grant.cell != cell);
        before - self.grants.len()
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

pub(crate) fn grant_requirements(
    cell: ElmId,
    generation: Generation,
    requirements: &[ElmEbiKernelApiRequirement],
) -> Result<usize, ApiRegistryError> {
    API_REGISTRY
        .lock()
        .grant_requirements(cell, generation, requirements)
}

pub(crate) fn query(
    cell: ElmId,
    generation: Generation,
    identifier: &[u8],
    versions: &[u16],
    management_allowed: bool,
) -> Result<ElmApiNamespaceV1, ApiRegistryError> {
    API_REGISTRY
        .lock()
        .query(cell, generation, identifier, versions, management_allowed)
}

pub(crate) fn authorize(
    grant_id: u64,
    cell: ElmId,
    generation: Generation,
    identifier: &[u8],
    version: u16,
    required_capabilities: u64,
) -> Result<(), ApiRegistryError> {
    API_REGISTRY.lock().authorize(
        grant_id,
        cell,
        generation,
        identifier,
        version,
        required_capabilities,
    )
}

pub(crate) fn remove_generation(cell: ElmId, generation: Generation) -> usize {
    API_REGISTRY.lock().remove_generation(cell, generation)
}

pub(crate) fn remove_cell(cell: ElmId) -> usize {
    API_REGISTRY.lock().remove_cell(cell)
}

pub(crate) fn diagnostic_text() -> String {
    let registry = API_REGISTRY.lock();
    let mut out = format!(
        "kernel_namespaces={}\nkernel_grants={}\n",
        registry.namespaces.len(),
        registry.grants.len()
    );
    for descriptor in &registry.namespaces {
        out.push_str(
            format!(
                "kernel_namespace identifier={} version={} flags=0x{:x} capabilities=0x{:x} table_size={} layout_hash={:02x?}\n",
                descriptor.identifier,
                descriptor.version,
                descriptor.flags,
                descriptor.capabilities,
                descriptor.table_size,
                descriptor.layout_hash
            )
            .as_str(),
        );
    }
    for grant in &registry.grants {
        let descriptor = registry.namespaces[grant.descriptor_index];
        out.push_str(
            format!(
                "kernel_grant id={} cell={} generation={} identifier={} version={} capabilities=0x{:x}\n",
                grant.id,
                grant.cell.0,
                grant.generation.0,
                descriptor.identifier,
                descriptor.version,
                grant.capabilities
            )
            .as_str(),
        );
    }
    out
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_requirement_roundtrip() -> bool {
    use kernel_api::ApiTableHeaderV1;

    #[repr(C)]
    struct TestApiV1 {
        header: ApiTableHeaderV1,
    }

    static TABLE: TestApiV1 = TestApiV1 {
        header: ApiTableHeaderV1::new::<TestApiV1>(0x3),
    };
    static DESCRIPTOR: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
        "kernel.test",
        1,
        ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT,
        0x3,
        &TABLE,
        [0x5a; 32],
    );
    static OTHER_DESCRIPTOR: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
        "kernel.test-other",
        1,
        ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT,
        0x3,
        &TABLE,
        [0xa5; 32],
    );
    static PUBLIC_DESCRIPTOR: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
        "elm.test-public",
        1,
        ELM_API_NAMESPACE_FLAG_PUBLIC,
        0x3,
        &TABLE,
        [0; 32],
    );

    let cell = ElmId(u64::MAX - 1);
    let generation = Generation::FIRST;
    if super::register_kernel_api_namespace(&DESCRIPTOR).is_err() {
        return false;
    }
    if super::register_kernel_api_namespace(&OTHER_DESCRIPTOR).is_err() {
        return false;
    }
    if super::register_kernel_api_namespace(&PUBLIC_DESCRIPTOR).is_err() {
        return false;
    }
    remove_cell(cell);
    if query(cell, generation, b"kernel.missing", &[1], false)
        != Err(ApiRegistryError::NamespaceUnavailable)
        || query(cell, generation, b"kernel.test", &[2], false)
            != Err(ApiRegistryError::VersionUnsupported)
        || query(cell, generation, b"kernel.test", &[1], false)
            != Err(ApiRegistryError::CapabilityDenied)
    {
        return false;
    }
    let requirement = match ElmEbiKernelApiRequirement::new("kernel.test", 1, 0x1, [0x5a; 32]) {
        Ok(requirement) => requirement,
        Err(_) => return false,
    };
    if grant_requirements(cell, generation, &[requirement]).ok() != Some(1) {
        return false;
    }
    let duplicate = match ElmEbiKernelApiRequirement::new("kernel.test", 1, 0x1, [0x5a; 32]) {
        Ok(requirement) => requirement,
        Err(_) => return false,
    };
    if grant_requirements(cell, generation, &[duplicate]) != Err(ApiRegistryError::DuplicateGrant)
    {
        return false;
    }
    let transactional_cell = ElmId(cell.0 - 2);
    remove_cell(transactional_cell);
    let transactional = [
        match ElmEbiKernelApiRequirement::new("kernel.test", 1, 0x1, [0x5a; 32]) {
            Ok(requirement) => requirement,
            Err(_) => return false,
        },
        match ElmEbiKernelApiRequirement::new("kernel.missing", 1, 0x1, [0x11; 32]) {
            Ok(requirement) => requirement,
            Err(_) => return false,
        },
    ];
    if grant_requirements(transactional_cell, generation, &transactional)
        != Err(ApiRegistryError::NamespaceUnavailable)
        || query(
            transactional_cell,
            generation,
            b"kernel.test",
            &[1],
            false,
        ) != Err(ApiRegistryError::CapabilityDenied)
        || !query(cell, generation, b"elm.test-public", &[1], false)
            .is_ok_and(|namespace| namespace.grant_id == 0 && namespace.capabilities == 0x3)
    {
        remove_cell(cell);
        return false;
    }
    let Ok(namespace) = query(cell, generation, b"kernel.test", &[1], false) else {
        return false;
    };
    if namespace.capabilities != 0x1
        || namespace.table_address == 0
        || namespace.grant_id == 0
        || authorize(namespace.grant_id, cell, generation, b"kernel.test", 1, 0x1).is_err()
        || authorize(
            namespace.grant_id,
            ElmId(cell.0 - 1),
            generation,
            b"kernel.test",
            1,
            0x1,
        ) != Err(ApiRegistryError::CapabilityDenied)
        || authorize(
            namespace.grant_id,
            cell,
            Generation(generation.0 + 1),
            b"kernel.test",
            1,
            0x1,
        ) != Err(ApiRegistryError::CapabilityDenied)
        || authorize(
            namespace.grant_id,
            cell,
            generation,
            b"kernel.test-other",
            1,
            0x1,
        ) != Err(ApiRegistryError::CapabilityDenied)
        || authorize(namespace.grant_id, cell, generation, b"kernel.test", 1, 0x2)
            != Err(ApiRegistryError::CapabilityDenied)
    {
        remove_cell(cell);
        return false;
    }
    if remove_generation(cell, generation) != 1 {
        remove_cell(cell);
        return false;
    }
    let revoked = authorize(namespace.grant_id, cell, generation, b"kernel.test", 1, 0x1)
        == Err(ApiRegistryError::CapabilityDenied)
        && query(cell, generation, b"kernel.test", &[1], false)
            == Err(ApiRegistryError::CapabilityDenied);
    remove_cell(cell);
    revoked
}
