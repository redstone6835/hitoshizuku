#![allow(dead_code)]

#[path = "../../src/bar.rs"]
mod bar;
#[path = "../../src/routing.rs"]
mod routing;
#[path = "../../src/topology.rs"]
mod topology;
#[path = "../../src/ls2k_config.rs"]
mod ls2k_config;

#[cfg(test)]
mod ls2k_config_tests {
    use super::ls2k_config::{
        Ls2kConfigError, Ls2kConfigWindow, Ls2kRootIrqRoute, Ls2kRootIrqTable,
    };

    const CONFIG_BASE: usize = 0xfe00_0000_00;

    fn window() -> Ls2kConfigWindow {
        Ls2kConfigWindow::new(CONFIG_BASE, 0x2000_0000, 1, 0x16).unwrap()
    }

    #[test]
    fn root_bridge_cfg1_offsets_cover_devices_nine_through_fourteen() {
        let window = window();

        assert_eq!(window.address(1, 9, 0, 0, 4), Ok(CONFIG_BASE + 0x4800));
        assert_eq!(
            window.address(1, 14, 7, 0xffc, 4),
            Ok(CONFIG_BASE + 0x0f00_77fc)
        );
        assert_eq!(
            window.address(1, 8, 0, 0, 4),
            Err(Ls2kConfigError::DeviceAbsent)
        );
        assert_eq!(
            window.address(1, 15, 0, 0, 4),
            Err(Ls2kConfigError::DeviceAbsent)
        );
    }

    #[test]
    fn downstream_cfg1_offsets_set_type_one_and_reencode_extended_offset() {
        let window = window();

        assert_eq!(
            window.address(2, 0, 0, 0, 4),
            Ok(CONFIG_BASE + 0x1002_0000)
        );
        assert_eq!(
            window.address(0x16, 0, 7, 0xabc, 1),
            Ok(CONFIG_BASE + 0x1a16_07bc)
        );
        assert_eq!(
            window.address(2, 1, 0, 0, 4),
            Err(Ls2kConfigError::DeviceAbsent)
        );
    }

    #[test]
    fn cfg1_window_rejects_invalid_bdf_register_and_access_width() {
        let window = window();

        assert_eq!(
            window.address(0, 9, 0, 0, 4),
            Err(Ls2kConfigError::BusOutOfRange)
        );
        assert_eq!(
            window.address(0x17, 0, 0, 0, 4),
            Err(Ls2kConfigError::BusOutOfRange)
        );
        assert_eq!(
            window.address(1, 9, 8, 0, 4),
            Err(Ls2kConfigError::InvalidFunction)
        );
        assert_eq!(
            window.address(1, 9, 0, 0x1000, 4),
            Err(Ls2kConfigError::InvalidRegister)
        );
        assert_eq!(
            window.address(1, 9, 0, 3, 2),
            Err(Ls2kConfigError::UnalignedAccess)
        );
        assert_eq!(
            window.address(1, 9, 0, 0, 3),
            Err(Ls2kConfigError::InvalidAccessWidth)
        );
    }

    #[test]
    fn cfg1_window_checks_required_span_and_address_overflow() {
        assert_eq!(
            Ls2kConfigWindow::new(CONFIG_BASE, 0x1000, 1, 0x16),
            Err(Ls2kConfigError::WindowTooSmall)
        );
        assert_eq!(
            Ls2kConfigWindow::new(CONFIG_BASE, 0x2000_0000, 2, 1),
            Err(Ls2kConfigError::InvalidBusRange)
        );
        assert_eq!(
            Ls2kConfigWindow::new(usize::MAX - 0xfff, 0x2000_0000, 1, 0x16),
            Err(Ls2kConfigError::AddressOverflow)
        );
    }

    #[test]
    fn root_irq_table_maps_six_bridge_devices_and_rejects_duplicates() {
        let routes = [
            Ls2kRootIrqRoute::new(9, 0, 6, 0x20),
            Ls2kRootIrqRoute::new(10, 0, 6, 0x21),
            Ls2kRootIrqRoute::new(11, 0, 6, 0x22),
            Ls2kRootIrqRoute::new(12, 0, 6, 0x23),
            Ls2kRootIrqRoute::new(13, 0, 6, 0x24),
            Ls2kRootIrqRoute::new(14, 0, 6, 0x25),
        ];
        let table = Ls2kRootIrqTable::new(1, &routes).unwrap();

        assert_eq!(table.resolve(1, 9, 0), Some((6, 0x20)));
        assert_eq!(table.resolve(1, 14, 0), Some((6, 0x25)));
        assert_eq!(table.resolve(2, 9, 0), None);
        assert_eq!(table.resolve(1, 9, 1), None);
        assert_eq!(
            Ls2kRootIrqTable::new(1, &[routes[0], routes[0]]),
            Err(Ls2kConfigError::DuplicateIrqRoute)
        );
        assert_eq!(
            Ls2kRootIrqTable::new(1, &[Ls2kRootIrqRoute::new(8, 0, 6, 0x20)]),
            Err(Ls2kConfigError::InvalidIrqRoute)
        );
    }
}
