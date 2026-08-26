const TYPE1_ACCESS: usize = 1 << 28;
const EXTENDED_REGISTER_MASK: u16 = 0x0f00;
const REGISTER_LOW_MASK: u16 = 0x00ff;
const CONFIG_SPACE_SIZE: u16 = 0x1000;
const FUNCTIONS_PER_DEVICE: u8 = 8;
const DEVICES_PER_BUS: u8 = 32;
const ROOT_DEVICE_FIRST: u8 = 9;
const ROOT_DEVICE_LAST: u8 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2kConfigError {
    UnalignedWindow,
    InvalidBusRange,
    WindowTooSmall,
    AddressOverflow,
    BusOutOfRange,
    InvalidDevice,
    InvalidFunction,
    DeviceAbsent,
    InvalidRegister,
    InvalidAccessWidth,
    UnalignedAccess,
    InvalidIrqRoute,
    DuplicateIrqRoute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kRootIrqRoute {
    device: u8,
    function: u8,
    parent: u32,
    specifier: u32,
}

impl Ls2kRootIrqRoute {
    pub(crate) const fn new(device: u8, function: u8, parent: u32, specifier: u32) -> Self {
        Self {
            device,
            function,
            parent,
            specifier,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kRootIrqTable {
    root_bus: u8,
    routes: [Option<Ls2kRootIrqRoute>; 6],
}

impl Ls2kRootIrqTable {
    pub(crate) fn new(root_bus: u8, routes: &[Ls2kRootIrqRoute]) -> Result<Self, Ls2kConfigError> {
        if routes.is_empty() {
            return Err(Ls2kConfigError::InvalidIrqRoute);
        }
        let mut table = Self {
            root_bus,
            routes: [None; 6],
        };
        for &route in routes {
            if !(ROOT_DEVICE_FIRST..=ROOT_DEVICE_LAST).contains(&route.device)
                || route.function != 0
                || route.parent == 0
            {
                return Err(Ls2kConfigError::InvalidIrqRoute);
            }
            let slot = usize::from(route.device - ROOT_DEVICE_FIRST);
            if table.routes[slot].is_some() {
                return Err(Ls2kConfigError::DuplicateIrqRoute);
            }
            table.routes[slot] = Some(route);
        }
        Ok(table)
    }

    pub(crate) fn resolve(self, bus: u8, device: u8, function: u8) -> Option<(u32, u32)> {
        if bus != self.root_bus || !(ROOT_DEVICE_FIRST..=ROOT_DEVICE_LAST).contains(&device) {
            return None;
        }
        let route = self.routes[usize::from(device - ROOT_DEVICE_FIRST)]?;
        (route.function == function).then_some((route.parent, route.specifier))
    }

    pub(crate) fn len(self) -> usize {
        self.routes.iter().filter(|route| route.is_some()).count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kConfigWindow {
    base: usize,
    size: usize,
    bus_start: u8,
    bus_end: u8,
}

impl Ls2kConfigWindow {
    pub(crate) fn new(
        base: usize,
        size: usize,
        bus_start: u8,
        bus_end: u8,
    ) -> Result<Self, Ls2kConfigError> {
        if !base.is_multiple_of(core::mem::align_of::<u32>()) {
            return Err(Ls2kConfigError::UnalignedWindow);
        }
        if bus_start > bus_end {
            return Err(Ls2kConfigError::InvalidBusRange);
        }
        base.checked_add(size)
            .ok_or(Ls2kConfigError::AddressOverflow)?;
        let required = if bus_start == bus_end {
            config_offset(bus_start, bus_start, ROOT_DEVICE_LAST, 7, 0x0fff)?
        } else {
            config_offset(bus_start, bus_end, 0, 7, 0x0fff)?
        }
        .checked_add(1)
        .ok_or(Ls2kConfigError::AddressOverflow)?;
        if size < required {
            return Err(Ls2kConfigError::WindowTooSmall);
        }
        Ok(Self {
            base,
            size,
            bus_start,
            bus_end,
        })
    }

    pub(crate) fn address(
        self,
        bus: u8,
        device: u8,
        function: u8,
        register: u16,
        width: usize,
    ) -> Result<usize, Ls2kConfigError> {
        if bus < self.bus_start || bus > self.bus_end {
            return Err(Ls2kConfigError::BusOutOfRange);
        }
        if device >= DEVICES_PER_BUS {
            return Err(Ls2kConfigError::InvalidDevice);
        }
        if function >= FUNCTIONS_PER_DEVICE {
            return Err(Ls2kConfigError::InvalidFunction);
        }
        if bus == self.bus_start {
            if !(ROOT_DEVICE_FIRST..=ROOT_DEVICE_LAST).contains(&device) {
                return Err(Ls2kConfigError::DeviceAbsent);
            }
        } else if device != 0 {
            return Err(Ls2kConfigError::DeviceAbsent);
        }
        if register >= CONFIG_SPACE_SIZE {
            return Err(Ls2kConfigError::InvalidRegister);
        }
        if !matches!(width, 1 | 2 | 4) {
            return Err(Ls2kConfigError::InvalidAccessWidth);
        }
        if !usize::from(register).is_multiple_of(width) {
            return Err(Ls2kConfigError::UnalignedAccess);
        }
        if usize::from(register)
            .checked_add(width)
            .is_none_or(|end| end > usize::from(CONFIG_SPACE_SIZE))
        {
            return Err(Ls2kConfigError::InvalidRegister);
        }

        let offset = config_offset(self.bus_start, bus, device, function, register)?;
        if offset.checked_add(width).is_none_or(|end| end > self.size) {
            return Err(Ls2kConfigError::WindowTooSmall);
        }
        self.base
            .checked_add(offset)
            .ok_or(Ls2kConfigError::AddressOverflow)
    }
}

fn config_offset(
    root_bus: u8,
    bus: u8,
    device: u8,
    function: u8,
    register: u16,
) -> Result<usize, Ls2kConfigError> {
    let devfn = (usize::from(device) << 3) | usize::from(function);
    let mut offset = devfn
        .checked_shl(8)
        .ok_or(Ls2kConfigError::AddressOverflow)?;
    offset |= usize::from(register & REGISTER_LOW_MASK);
    offset |= usize::from(register & EXTENDED_REGISTER_MASK)
        .checked_shl(16)
        .ok_or(Ls2kConfigError::AddressOverflow)?;
    if bus != root_bus {
        offset |= TYPE1_ACCESS;
        offset |= usize::from(bus)
            .checked_shl(16)
            .ok_or(Ls2kConfigError::AddressOverflow)?;
    }
    Ok(offset)
}
