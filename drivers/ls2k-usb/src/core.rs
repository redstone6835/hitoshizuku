//! LS2K1000 USB 主机控制器公共核心：HCD 能力接口、总线枚举与
//! usb.rs PnP 设备创建。
//!
//! [`UsbHcd`] 描述一个主机控制器的最小能力面（端口电源/复位/连接检测 +
//! control/bulk/interrupt 传输），EHCI/OHCI/dwc2 三个实现共享同一套枚举
//! 逻辑：复位端口 → 读设备描述符（addr 0）→ set address → 完整设备/
//! 配置描述符解析 → 创建 UsbDevice + UsbInterface Pnp 设备（挂入
//! PNP_DEVICES 并触发 class 驱动 probe）。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use general::dev::pnp::{PNP_DEVICES, PNP_DRIVERS, PnpDevice, PnpError, PnpId};
use general::dev::usb::{
    UsbDevice, UsbDeviceInfo, UsbEndpointDesc, UsbInterfaceInfo, UsbSpeed,
    usb_device_pnp_info_boxed,
};

use crate::regs::*;

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

/// 主机控制器能力接口。
pub trait UsbHcd: Send + Sync {
    fn name(&self) -> &'static str;
    fn port_count(&self) -> usize;
    /// 停止控制器及其 DMA。返回成功后才允许释放 HCD 持有的 DMA 对象。
    fn shutdown(&self) -> Result<(), &'static str> {
        Ok(())
    }
    fn port_power_on(&self, port: usize) -> Result<(), &'static str>;
    /// 复位端口并返回连接设备速度（USB_SPEED_HIGH/FULL/LOW）。
    fn port_reset(&self, port: usize) -> Result<u8, &'static str>;
    fn port_connected(&self, port: usize) -> bool;
    /// 控制传输：setup + data（可为空）+ status。
    fn control_transfer(
        &self,
        dev_addr: u8,
        setup: &UsbSetup,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str>;
    /// 批量传输（整包，调用方保证 ≤ 端点最大包）。
    fn bulk_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str>;
    /// 中断传输。
    fn interrupt_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str>;
}

pub struct UsbBus {
    pub bus_id: u8,
    hcd: Arc<dyn UsbHcd>,
    next_address: core::sync::atomic::AtomicU8,
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

impl UsbBus {
    pub fn new(bus_id: u8, hcd: Arc<dyn UsbHcd>) -> Arc<Self> {
        Arc::new(Self {
            bus_id,
            hcd,
            next_address: core::sync::atomic::AtomicU8::new(1),
        })
    }

    pub fn hcd(&self) -> &Arc<dyn UsbHcd> {
        &self.hcd
    }

    fn alloc_address(&self) -> u8 {
        let address = self
            .next_address
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if address == 0 { 1 } else { address & 0x7f }
    }

    /// 扫描所有端口，为已连接设备执行枚举。
    pub fn scan_ports(&self) {
        for port in 0..self.hcd.port_count() {
            if !self.hcd.port_connected(port) {
                continue;
            }
            if let Err(error) = self.enumerate_port(port) {
                log::printk!(
                    "[ls2k-usb] {} port {} enumeration failed: {}",
                    self.hcd.name(),
                    port,
                    error
                );
            }
        }
    }

    fn control(
        &self,
        address: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        data_in: bool,
        length: u16,
    ) -> Result<usize, &'static str> {
        let setup = UsbSetup {
            bmRequestType: if data_in {
                USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE
            } else {
                USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE
            },
            bRequest: request,
            wValue: value,
            wIndex: index,
            wLength: length,
        };
        self.hcd.control_transfer(address, &setup, data, data_in)
    }

    fn get_descriptor(
        &self,
        address: u8,
        kind: u8,
        index: u16,
        out: &mut [u8],
    ) -> Result<usize, &'static str> {
        self.control(
            address,
            USB_REQ_GET_DESCRIPTOR,
            u16::from(kind) << 8 | index,
            0,
            out,
            true,
            out.len() as u16,
        )
    }

    fn enumerate_port(&self, port: usize) -> Result<(), &'static str> {
        // 1) 复位端口，得到速度。
        let speed = self.hcd.port_reset(port)?;
        // EHCI 把 FS/LS 移交伴生 OHCI 后端口不再连接，静默跳过
        //（由 OHCI 总线负责枚举）。
        if !self.hcd.port_connected(port) {
            return Ok(());
        }
        let mut device_desc = [0u8; 18];
        // 2) 地址 0 读前 8 字节设备描述符（获取 bMaxPacketSize0）。
        let mut short = [0u8; 8];
        let got = self.get_descriptor(0, USB_DT_DEVICE, 0, &mut short)?;
        if got < 8 || short[1] != USB_DT_DEVICE {
            return Err("invalid short device descriptor");
        }
        let max_packet = short[7].max(8);
        // 3) 分配地址。
        let address = self.alloc_address();
        let mut empty = [];
        // USB 2.0 9.4.6：SET_ADDRESS 本身仍发送到默认地址 0，设备在成功
        // 完成状态阶段后才启用新地址。规范还要求主机至少等待 2 ms，再向新
        // 地址发起下一次控制传输。
        self.control(
            0,
            USB_REQ_SET_ADDRESS,
            u16::from(address),
            0,
            &mut empty,
            false,
            0,
        )?;
        delay_ns(2_000_000);
        // 4) 完整设备描述符。
        let got = self.get_descriptor(address, USB_DT_DEVICE, 0, &mut device_desc)?;
        if got < 18 || device_desc[1] != USB_DT_DEVICE {
            return Err("invalid device descriptor");
        }
        let vendor = le16(&device_desc, 8);
        let product = le16(&device_desc, 10);
        let num_configs = device_desc[17];

        let mut usb_device = None;
        for config_index in 0..num_configs.max(1) {
            // 5) 配置描述符总长度。
            let mut config_head = [0u8; 9];
            let got = self.get_descriptor(
                address,
                USB_DT_CONFIG,
                u16::from(config_index),
                &mut config_head,
            )?;
            if got < 9 || config_head[1] != USB_DT_CONFIG {
                continue;
            }
            let total_len = le16(&config_head, 2) as usize;
            if total_len > 4096 {
                continue;
            }
            let mut config = vec![0u8; total_len];
            config[..9].copy_from_slice(&config_head);
            let got =
                self.get_descriptor(address, USB_DT_CONFIG, u16::from(config_index), &mut config)?;
            if got < total_len {
                continue;
            }
            // 6) 解析 interface/endpoint。
            let mut interfaces: Vec<UsbInterfaceInfo> = Vec::new();
            let mut position = 9usize;
            let mut current: Option<usize> = None;
            while position + 2 <= config.len() {
                let length = config[position] as usize;
                let kind = config[position + 1];
                if length == 0 || position + length > config.len() {
                    break;
                }
                match kind {
                    USB_DT_INTERFACE if length >= 9 => {
                        let desc = UsbInterfaceDesc {
                            bLength: config[position],
                            bDescriptorType: config[position + 1],
                            bInterfaceNumber: config[position + 2],
                            bAlternateSetting: config[position + 3],
                            bNumEndpoints: config[position + 4],
                            bInterfaceClass: config[position + 5],
                            bInterfaceSubClass: config[position + 6],
                            bInterfaceProtocol: config[position + 7],
                            iInterface: config[position + 8],
                        };
                        let info = UsbInterfaceInfo {
                            class: desc.bInterfaceClass,
                            subclass: desc.bInterfaceSubClass,
                            protocol: desc.bInterfaceProtocol,
                            interface_number: desc.bInterfaceNumber,
                            num_endpoints: desc.bNumEndpoints,
                            endpoints: Vec::new(),
                            vendor,
                            product,
                        };
                        interfaces.push(info);
                        current = Some(interfaces.len() - 1);
                    }
                    USB_DT_ENDPOINT if length >= 7 => {
                        if let Some(index) = current {
                            let desc = UsbEndpointDesc {
                                address: config[position + 2],
                                attributes: config[position + 3],
                                max_packet_size: le16(&config, position + 4),
                                interval: config[position + 6],
                            };
                            interfaces[index].endpoints.push(desc);
                            interfaces[index].num_endpoints =
                                interfaces[index].endpoints.len() as u8;
                        }
                    }
                    _ => {}
                }
                position += length;
            }
            // 7) set configuration。
            let config_value = config_head[5];
            let mut empty = [];
            self.control(
                address,
                USB_REQ_SET_CONFIGURATION,
                u16::from(config_value),
                0,
                &mut empty,
                false,
                0,
            )?;
            usb_device = Some((vendor, product, config_index, interfaces));
            break;
        }
        let Some((vendor, product, _, interfaces)) = usb_device else {
            return Err("no usable configuration");
        };

        // 8) 创建 UsbDevice + UsbInterface Pnp 设备。
        let speed_enum = match speed {
            USB_SPEED_HIGH => UsbSpeed::High,
            USB_SPEED_LOW => UsbSpeed::Low,
            _ => UsbSpeed::Full,
        };
        let dev_info = UsbDeviceInfo {
            vendor,
            product,
            device_class: device_desc[4],
            device_subclass: device_desc[5],
            device_protocol: device_desc[6],
            max_packet_size: max_packet,
            manufacturer_str: None,
            product_str: None,
            serial_str: None,
            num_configurations: num_configs,
            speed: speed_enum,
        };
        let name: String = alloc::format!("usb-{}:{}", self.bus_id, address);
        let pnp = PnpDevice::new(
            PnpId::Usb {
                bus_id: self.bus_id,
                address,
                interface: None,
            },
            name.into_boxed_str(),
            usb_device_pnp_info_boxed(dev_info),
        )
        .map_err(|_| "usb device pnp allocation failed")?;
        PNP_DEVICES
            .get_or_insert(Arc::clone(&pnp))
            .map_err(|_| "usb device registration failed")?;
        let usb_device = UsbDevice::from_pnp(&pnp).ok_or("usb device wrapper failed")?;

        for interface in interfaces {
            let interface_name: String = alloc::format!(
                "usb-{}:{}.{}",
                self.bus_id,
                address,
                interface.interface_number
            );
            let child = usb_device
                .create_interface(
                    interface.interface_number,
                    interface_name.into_boxed_str(),
                    interface,
                )
                .map_err(|_| "usb interface creation failed")?;
            PNP_DEVICES
                .get_or_insert(Arc::clone(&child))
                .map_err(|_| "usb interface registration failed")?;
            let _ = PNP_DRIVERS.probe_device(&child);
        }
        log::printk!(
            "[ls2k-usb] {} enumerated device at port {} addr {} {:04x}:{:04x} speed={} interfaces={}",
            self.hcd.name(),
            port,
            address,
            vendor,
            product,
            match speed_enum {
                UsbSpeed::High => "high",
                UsbSpeed::Full => "full",
                UsbSpeed::Low => "low",
                _ => "?",
            },
            usb_device.interfaces().len(),
        );
        Ok(())
    }
}

/// 把 PnpError 映射成静态错误描述（probe 用）。
pub fn pnp_error_message(error: PnpError) -> &'static str {
    match error {
        PnpError::OutOfMemory => "out of memory",
        PnpError::ProbeDeferred => "deferred",
        _ => "pnp error",
    }
}
