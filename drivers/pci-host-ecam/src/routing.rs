//! PCI INTx bridge swizzle 与 requester ID 到 MSI domain 参数的纯路由模型。

extern crate alloc;

use alloc::vec::Vec;

const PCI_DEVICES_PER_BUS: u8 = 32;
const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;
const PCI_INTX_PIN_COUNT: u8 = 4;

/// 已归约到 host 根总线直接子节点的 PCI INTx key。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciIntxRouteKey {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub pin: u8,
}

/// 一条下游 bus 到根总线直连桥的 INTx 归约关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciIntxBridgeRoute {
    downstream_bus: u8,
    root_device: u8,
    root_function: u8,
    /// 下游 endpoint 自身 device 号之外，所有中间桥 device 号之和模 4。
    swizzle_offset: u8,
}

impl PciIntxBridgeRoute {
    pub(crate) fn new(
        downstream_bus: u8,
        root_device: u8,
        root_function: u8,
        swizzle_offset: u8,
    ) -> Option<Self> {
        if root_device >= PCI_DEVICES_PER_BUS
            || root_function >= PCI_FUNCTIONS_PER_DEVICE
            || swizzle_offset >= PCI_INTX_PIN_COUNT
        {
            return None;
        }
        Some(Self {
            downstream_bus,
            root_device,
            root_function,
            swizzle_offset,
        })
    }
}

/// 一个 host 内按 bus 索引的 PCI INTx bridge swizzle 快照。
pub(crate) struct PciIntxRouting {
    root_bus: u8,
    bridges: Vec<PciIntxBridgeRoute>,
}

impl PciIntxRouting {
    pub(crate) fn new(root_bus: u8, mut bridges: Vec<PciIntxBridgeRoute>) -> Option<Self> {
        bridges.sort_unstable_by_key(|route| route.downstream_bus);
        if bridges.iter().any(|route| route.downstream_bus == root_bus)
            || bridges
                .windows(2)
                .any(|routes| routes[0].downstream_bus == routes[1].downstream_bus)
        {
            return None;
        }
        Some(Self { root_bus, bridges })
    }

    /// 将任意已枚举 function 的原始 pin 逐级 swizzle 到 host 的直接 child key。
    pub(crate) fn resolve(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        pin: u8,
    ) -> Option<PciIntxRouteKey> {
        if device >= PCI_DEVICES_PER_BUS
            || function >= PCI_FUNCTIONS_PER_DEVICE
            || !(1..=PCI_INTX_PIN_COUNT).contains(&pin)
        {
            return None;
        }
        if bus == self.root_bus {
            return Some(PciIntxRouteKey {
                bus,
                device,
                function,
                pin,
            });
        }

        let route = self
            .bridges
            .binary_search_by_key(&bus, |route| route.downstream_bus)
            .ok()
            .map(|index| self.bridges[index])?;
        let pin_index = pin - 1;
        let pin = (pin_index + device % PCI_INTX_PIN_COUNT + route.swizzle_offset)
            % PCI_INTX_PIN_COUNT
            + 1;
        Some(PciIntxRouteKey {
            bus: self.root_bus,
            device: route.root_device,
            function: route.root_function,
            pin,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciMsiTarget {
    pub controller: u32,
    pub device_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciMsiMapRoute {
    requester_base: u32,
    controller: u32,
    output_base: Option<u32>,
    length: u32,
}

impl PciMsiMapRoute {
    /// 标准 PCI `msi-map` 允许 provider 使用零个或一个 MSI cell。
    pub(crate) fn new(
        requester_base: u32,
        controller: u32,
        specifier: &[u32],
        length: u32,
    ) -> Option<Self> {
        let output_base = match specifier {
            [] => None,
            [base] => Some(*base),
            _ => return None,
        };
        (length != 0).then_some(Self {
            requester_base,
            controller,
            output_base,
            length,
        })
    }

    fn resolve(self, requester: u32, mask: u32) -> Option<PciMsiTarget> {
        let offset = (requester & mask).checked_sub(self.requester_base)?;
        if offset >= self.length {
            return None;
        }
        // 零-cell map 只用 masked RID 做范围匹配，传给 MSI domain 的仍是原始 RID。
        let device_id = match self.output_base {
            None => requester,
            Some(base) => base.checked_add(offset)?,
        };
        Some(PciMsiTarget {
            controller: self.controller,
            device_id,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciMsiParentRoute {
    controller: u32,
}

impl PciMsiParentRoute {
    pub(crate) fn new(controller: u32, specifier: &[u32]) -> Option<Self> {
        specifier.is_empty().then_some(Self { controller })
    }
}

/// `msi-map` 与 `msi-parent` 是两种互斥路由模式。
pub(crate) enum PciMsiRoutingMode {
    Map {
        mask: u32,
        routes: Vec<PciMsiMapRoute>,
    },
    Parents(Vec<PciMsiParentRoute>),
}

impl PciMsiRoutingMode {
    /// 返回按分配尝试顺序排列的 MSI domain 参数。
    ///
    /// Map 模式至多返回一个目标，未命中时为空且绝不回退 parents；Parents 模式
    /// 则保留 DT 属性中的 controller 顺序，供调用方在 domain 不可用或耗尽时重试。
    pub(crate) fn allocation_targets(&self, requester: u32) -> Vec<PciMsiTarget> {
        match self {
            Self::Map { mask, routes } => routes
                .iter()
                .find_map(|route| route.resolve(requester, *mask))
                .into_iter()
                .collect(),
            Self::Parents(parents) => parents
                .iter()
                .map(|parent| PciMsiTarget {
                    controller: parent.controller,
                    device_id: requester,
                })
                .collect(),
        }
    }
}

/// 按固件顺序尝试 MSI domain，直到首个成功分配。
pub(crate) fn allocate_first_available<T, E>(
    targets: &[PciMsiTarget],
    mut allocate: impl FnMut(PciMsiTarget) -> Result<T, E>,
) -> Option<T> {
    targets
        .iter()
        .copied()
        .find_map(|target| allocate(target).ok())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn intx_routing_rejects_invalid_pin_bdf_and_unknown_bus() {
        let routing = PciIntxRouting::new(0, Vec::new()).unwrap();
        assert_eq!(routing.resolve(0, 1, 0, 0), None);
        assert_eq!(routing.resolve(0, 1, 0, 5), None);
        assert_eq!(routing.resolve(0, 32, 0, 1), None);
        assert_eq!(routing.resolve(0, 1, 8, 1), None);
        assert_eq!(routing.resolve(1, 1, 0, 1), None);
    }

    #[test]
    fn intx_routing_swizzles_multilevel_bridges_to_root_child() {
        let routing = PciIntxRouting::new(
            0,
            vec![
                PciIntxBridgeRoute::new(1, 2, 1, 0).unwrap(),
                // bus 2 位于 bus 1 的 device 3 桥后。
                PciIntxBridgeRoute::new(2, 2, 1, 3).unwrap(),
                // bus 3 再经过 bus 2 的 device 6 桥，累计偏移为 (3 + 6) % 4。
                PciIntxBridgeRoute::new(3, 2, 1, 1).unwrap(),
            ],
        )
        .unwrap();

        assert_eq!(
            routing.resolve(0, 31, 7, 4),
            Some(PciIntxRouteKey {
                bus: 0,
                device: 31,
                function: 7,
                pin: 4,
            })
        );
        assert_eq!(
            routing.resolve(1, 3, 0, 1),
            Some(PciIntxRouteKey {
                bus: 0,
                device: 2,
                function: 1,
                pin: 4,
            })
        );
        assert_eq!(
            routing.resolve(3, 5, 4, 2),
            Some(PciIntxRouteKey {
                bus: 0,
                device: 2,
                function: 1,
                pin: 4,
            })
        );
    }

    #[test]
    fn zero_cell_map_matches_masked_rid_but_forwards_raw_rid() {
        let mode = PciMsiRoutingMode::Map {
            mask: 0xff,
            routes: vec![PciMsiMapRoute::new(0x20, 7, &[], 0x10).unwrap()],
        };
        assert_eq!(
            mode.allocation_targets(0x123),
            vec![PciMsiTarget {
                controller: 7,
                device_id: 0x123,
            }]
        );
    }

    #[test]
    fn one_cell_map_adds_masked_rid_offset_to_output_base() {
        let mode = PciMsiRoutingMode::Map {
            mask: 0xff,
            routes: vec![PciMsiMapRoute::new(0x20, 8, &[0x400], 0x10).unwrap()],
        };
        assert_eq!(
            mode.allocation_targets(0x123),
            vec![PciMsiTarget {
                controller: 8,
                device_id: 0x403,
            }]
        );
    }

    #[test]
    fn map_miss_has_no_parent_fallback_targets() {
        let mode = PciMsiRoutingMode::Map {
            mask: u32::MAX,
            routes: vec![PciMsiMapRoute::new(0x100, 1, &[], 8).unwrap()],
        };
        assert!(mode.allocation_targets(0x200).is_empty());
    }

    #[test]
    fn zero_cell_parents_preserve_retry_order_and_raw_rid() {
        let mode = PciMsiRoutingMode::Parents(vec![
            PciMsiParentRoute::new(9, &[]).unwrap(),
            PciMsiParentRoute::new(3, &[]).unwrap(),
        ]);
        let targets = mode.allocation_targets(0x321);
        assert_eq!(
            targets,
            vec![
                PciMsiTarget {
                    controller: 9,
                    device_id: 0x321,
                },
                PciMsiTarget {
                    controller: 3,
                    device_id: 0x321,
                },
            ]
        );
        let mut attempted = Vec::new();
        let selected = allocate_first_available(&targets, |target| {
            attempted.push(target.controller);
            if target.controller == 3 {
                Ok(target)
            } else {
                Err(())
            }
        });
        assert_eq!(attempted, vec![9, 3]);
        assert_eq!(selected.map(|target| target.controller), Some(3));
    }

    #[test]
    fn nonzero_parent_and_multicell_map_fail_closed() {
        assert!(PciMsiParentRoute::new(1, &[0]).is_none());
        assert!(PciMsiMapRoute::new(0, 1, &[0, 1], 1).is_none());
        assert!(PciMsiMapRoute::new(0, 1, &[], 0).is_none());
    }
}
