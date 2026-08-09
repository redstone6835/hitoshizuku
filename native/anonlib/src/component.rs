//! SOYO 动态组件与类型化 Interface 调用门。

use core::marker::PhantomData;

use super::{HandleTransfer, Image, OwnedHandle, Process, Status, abi};

pub enum ComponentObject {}
pub enum InterfaceObject {}

#[repr(C)]
struct MrtComponentResult {
    status: u32,
    handle: u64,
}

#[repr(C)]
struct MrtInterfaceResult {
    status: u32,
    handle: u64,
    gate: *const abi::MygoComponentInterfaceGate,
}

#[repr(C)]
struct MrtComponentCall {
    status: u32,
    target: u64,
    previous_component: u64,
}

unsafe extern "C" {
    fn mrt_component_load(
        process: u64,
        request: *const abi::MygoComponentLoadRequest,
    ) -> MrtComponentResult;
    fn mrt_component_query(component: u64, query: *mut abi::MygoComponentQuery) -> u32;
    fn mrt_component_interface(
        component: u64,
        request: *const abi::MygoInterfaceRequest,
    ) -> MrtInterfaceResult;
    fn mrt_component_unload(component: u64, deadline_ns: u64) -> MrtComponentResult;
    fn mrt_component_enter(gate: *const abi::MygoComponentInterfaceGate) -> MrtComponentCall;
    fn mrt_component_leave(
        gate: *const abi::MygoComponentInterfaceGate,
        previous_component: u64,
    );
}

pub struct Component {
    handle: OwnedHandle<ComponentObject>,
}

impl Component {
    pub fn load<const N: usize>(
        process: &Process,
        root: &Image,
        dependencies: [&Image; N],
    ) -> Result<Self, Status> {
        Self::load_with_bindings(process, root, dependencies, &[])
    }

    pub fn load_with_bindings<const N: usize>(
        process: &Process,
        root: &Image,
        dependencies: [&Image; N],
        bindings: &[HandleTransfer],
    ) -> Result<Self, Status> {
        let images = core::array::from_fn::<u64, N, _>(|index| dependencies[index].raw());
        let image_count = u32::try_from(N)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let binding_count = u32::try_from(bindings.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let binding_ptr = bindings
            .first()
            .map_or(0, |binding| binding.raw() as *const _ as usize as u64);
        let request = abi::MygoComponentLoadRequest {
            root_image: root.raw(),
            images: abi::MygoProcessArrayRef {
                ptr: if N == 0 {
                    0
                } else {
                    images.as_ptr() as usize as u64
                },
                count: image_count,
                reserved: 0,
            },
            bindings: abi::MygoProcessArrayRef {
                ptr: binding_ptr,
                count: binding_count,
                reserved: 0,
            },
            flags: 0,
            reserved: [0; 2],
        };
        let result = unsafe { mrt_component_load(process.raw(), &request) };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.handle)
            .map(|handle| Self { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub fn query(&self) -> Result<abi::MygoComponentQuery, Status> {
        let mut query = abi::MygoComponentQuery::default();
        let status = unsafe { mrt_component_query(self.handle.raw(), &mut query) };
        if status == abi::MYGO_STATUS_ok {
            Ok(query)
        } else {
            Err(Status(status))
        }
    }

    pub fn interface<T>(
        &self,
        interface_identity: [u8; 16],
        signature_hash: [u8; 32],
    ) -> Result<Interface<T>, Status> {
        let request = abi::MygoInterfaceRequest {
            interface_identity,
            signature_hash,
        };
        let result = unsafe { mrt_component_interface(self.handle.raw(), &request) };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        let handle = OwnedHandle::new(result.handle)
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?;
        if result.gate.is_null() {
            return Err(Status(abi::MYGO_STATUS_component_invalid_transaction));
        }
        Ok(Interface {
            handle,
            gate: result.gate,
            marker: PhantomData,
        })
    }

    pub fn unload(&self, deadline_ns: u64) -> Result<(), Status> {
        let result = unsafe { mrt_component_unload(self.handle.raw(), deadline_ns) };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }
}

pub struct Interface<T> {
    handle: OwnedHandle<InterfaceObject>,
    gate: *const abi::MygoComponentInterfaceGate,
    marker: PhantomData<T>,
}

impl<T> Interface<T> {
    pub fn enter(&self) -> Result<ComponentCall<'_, T>, Status> {
        let call = unsafe { mrt_component_enter(self.gate) };
        if call.status != abi::MYGO_STATUS_ok {
            return Err(Status(call.status));
        }
        Ok(ComponentCall {
            interface: self,
            target: call.target,
            previous_component: call.previous_component,
        })
    }

    pub fn handle_raw(&self) -> u64 {
        self.handle.raw()
    }
}

pub struct ComponentCall<'a, T> {
    interface: &'a Interface<T>,
    target: u64,
    previous_component: u64,
}

impl<T: Copy> ComponentCall<'_, T> {
    /// 调用者必须确保 `T` 是 manifest 中 signature hash 对应的函数指针类型。
    pub unsafe fn target(&self) -> T {
        assert!(core::mem::size_of::<T>() == core::mem::size_of::<u64>());
        unsafe { core::mem::transmute_copy(&self.target) }
    }
}

impl<T> Drop for ComponentCall<'_, T> {
    fn drop(&mut self) {
        unsafe { mrt_component_leave(self.interface.gate, self.previous_component) };
    }
}
