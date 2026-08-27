//! dwc2 主机模式驱动（2K1000 otg@40000000，dr_mode="host"）。
//!
//! 使用 synopsys dwc2 主机通道（DMA 模式）：初始化（GUSBCFG 强制主机 +
//! 软复位 + GAHBCFG DMA 使能 + FIFO 划分 + OTGCTL HSTEN）、HPRT 端口
//! 电源/复位/连接检测，控制（通道 0）/批量/中断传输经
//! HCCHAR/HCTSIZ/HCDMA/HCINT 编程，轮询 XFERCOMP/CHHLTD 完成。
//! 不启用任何全局中断（GINTMSK=0），全部传输为同步轮询。

use core::sync::atomic::compiler_fence;

use general::dev::dma::DmaContext;
use vfs::sync::Spinlock;

use crate::core::UsbHcd;
use crate::regs::*;

const RESET_TIMEOUT_LOOPS: u32 = 200_000;
const CHANNEL_TIMEOUT_LOOPS: u32 = 400_000;
const CHANNEL_COUNT: usize = 8;

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

pub struct Dwc2Hcd {
    base: usize,
    lock: Spinlock<()>,
    next_channel: core::sync::atomic::AtomicUsize,
}

impl Dwc2Hcd {
    pub fn new(base: usize, _context: DmaContext) -> Result<Self, &'static str> {
        let hcd = Self {
            base,
            lock: Spinlock::new(()),
            next_channel: core::sync::atomic::AtomicUsize::new(0),
        };
        hcd.core_reset()?;
        hcd.host_init()?;
        Ok(hcd)
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: base 由 platform probe 映射，offset 为固定寄存器偏移。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 同 read32，目标寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn chan(&self, index: usize, offset: usize) -> usize {
        DWC2_HCCHAR0 + index * DWC2_HC_CHAN_STRIDE + offset
    }

    fn core_reset(&self) -> Result<(), &'static str> {
        self.write32(DWC2_GRSTCTL, DWC2_GRSTCTL_CSFTRST);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read32(DWC2_GRSTCTL) & DWC2_GRSTCTL_CSFTRST == 0 {
                break;
            }
            delay_ns(1_000);
        }
        if self.read32(DWC2_GRSTCTL) & DWC2_GRSTCTL_CSFTRST != 0 {
            return Err("dwc2 core reset timeout");
        }
        delay_ns(1_000_000);
        Ok(())
    }

    fn host_init(&self) -> Result<(), &'static str> {
        // 强制主机模式，关闭 HNP/SRP。
        let mut gusbcfg = self.read32(DWC2_GUSBCFG);
        gusbcfg |= DWC2_GUSBCFG_FORCEHSTMODE;
        gusbcfg &= !(DWC2_GUSBCFG_HNPCAP | DWC2_GUSBCFG_SRPCAP);
        // UTMI+ 接口（非 ULPI）。
        gusbcfg &= !DWC2_GUSBCFG_ULPI_UTMI_SEL;
        self.write32(DWC2_GUSBCFG, gusbcfg);
        delay_ns(1_000_000);

        // DMA 使能 + 全局中断使能（传输走轮询）。
        let mut gahbcfg = self.read32(DWC2_GAHBCFG);
        gahbcfg |= DWC2_GAHBCFG_GLBL_INTR_EN | DWC2_GAHBCFG_DMA_EN;
        self.write32(DWC2_GAHBCFG, gahbcfg);

        // FIFO 划分：RX=512 words、non-periodic TX=256、periodic TX=256。
        self.write32(DWC2_GRXFSIZ, 0x200);
        self.write32(DWC2_GNPTXFSIZ, (0x100 << 16) | 0x200);
        self.write32(DWC2_HPTXFSIZ, (0x100 << 16) | 0x300);

        // 主机使能。
        self.write32(DWC2_OTGCTL, self.read32(DWC2_OTGCTL) | DWC2_OTGCTL_HSTEN);
        // 关闭全部全局中断（轮询模式）。
        self.write32(DWC2_GINTMSK, 0);
        delay_ns(1_000_000);
        Ok(())
    }

    /// 分配一个主机通道（轮询模式串行使用，取模循环）。
    fn alloc_channel(&self) -> usize {
        let index = self
            .next_channel
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
            % CHANNEL_COUNT;
        // 复位通道寄存器。
        let base = self.chan(index, 0);
        for offset in [0usize, 0x04, 0x08, 0x0c, 0x10] {
            self.write32(base + offset, 0);
        }
        index
    }

    fn channel_wait(&self, channel: usize) -> Result<(), &'static str> {
        let int_reg = self.chan(channel, 0x08);
        for _ in 0..CHANNEL_TIMEOUT_LOOPS {
            let status = self.read32(int_reg);
            if status & DWC2_HCINT_XFERCOMP != 0 {
                self.write32(int_reg, status & DWC2_HCINT_ALL);
                return Ok(());
            }
            if status & DWC2_HCINT_CHHLTD != 0 {
                let error = if status & DWC2_HCINT_STALL != 0 {
                    "dwc2 channel stall"
                } else if status & DWC2_HCINT_XACTERR != 0 {
                    "dwc2 channel transaction error"
                } else if status & DWC2_HCINT_BBLERR != 0 {
                    "dwc2 channel babble"
                } else {
                    "dwc2 channel halted"
                };
                self.write32(int_reg, status & DWC2_HCINT_ALL);
                return Err(error);
            }
            delay_ns(2_000);
        }
        Err("dwc2 channel timeout")
    }

    fn start_channel(
        &self,
        channel: usize,
        dev_addr: u8,
        ep: u8,
        data_in: bool,
        eptype: u32,
        mps: u32,
        pid: u32,
        size: usize,
        buffer: *const u8,
    ) -> Result<(), &'static str> {
        let packets = if size == 0 {
            1
        } else {
            (size + mps as usize - 1) / mps as usize
        };
        // 先清中断，再写 HCTSIZ/HCDMA/HCCHAR。
        self.write32(self.chan(channel, 0x08), DWC2_HCINT_ALL);
        self.write32(
            self.chan(channel, 0x10),
            ((size as u32) << DWC2_HCTSIZ_XFERSIZE_SHIFT)
                | ((packets as u32) << DWC2_HCTSIZ_PKTCNT_SHIFT)
                | pid,
        );
        self.write32(self.chan(channel, 0x14), buffer as u32);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let mut char = ((u32::from(dev_addr) & 0x7f) << DWC2_HCCHAR_DEVADDR_SHIFT)
            | ((u32::from(ep & 0x0f)) << DWC2_HCCHAR_EPNUM_SHIFT)
            | eptype
            | (mps & DWC2_HCCHAR_MPS_MASK);
        if data_in {
            char |= DWC2_HCCHAR_EPDIR;
        }
        char |= DWC2_HCCHAR_CHENA;
        self.write32(self.chan(channel, 0x00), char);
        Ok(())
    }

    fn control_phase(
        &self,
        dev_addr: u8,
        data_in: bool,
        pid: u32,
        buffer: *const u8,
        size: usize,
    ) -> Result<(), &'static str> {
        let channel = self.alloc_channel();
        self.start_channel(
            channel,
            dev_addr,
            0,
            data_in,
            DWC2_HCCHAR_EPTYPE_CONTROL,
            64,
            pid,
            size,
            buffer,
        )?;
        self.channel_wait(channel)
    }
}

impl UsbHcd for Dwc2Hcd {
    fn name(&self) -> &'static str {
        "dwc2"
    }

    fn port_count(&self) -> usize {
        1
    }

    fn port_power_on(&self, port: usize) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        let _ = port;
        self.write32(DWC2_HPRT, self.read32(DWC2_HPRT) | DWC2_HPRT_PRTPWR);
        delay_ns(100_000_000);
        Ok(())
    }

    fn port_connected(&self, port: usize) -> bool {
        let _guard = self.lock.lock();
        let _ = port;
        self.read32(DWC2_HPRT) & DWC2_HPRT_PRTCONNSTS != 0
    }

    fn port_reset(&self, port: usize) -> Result<u8, &'static str> {
        let _guard = self.lock.lock();
        let _ = port;
        let hprt = self.read32(DWC2_HPRT);
        // 清除连接/使能变化位。
        self.write32(
            DWC2_HPRT,
            hprt | DWC2_HPRT_PRTCONNDET | DWC2_HPRT_PRTENCHNG | DWC2_HPRT_PRTOVRCURRCHNG,
        );
        // 端口复位 ≥ 10ms。
        self.write32(DWC2_HPRT, self.read32(DWC2_HPRT) | DWC2_HPRT_PRTRST);
        delay_ns(20_000_000);
        self.write32(DWC2_HPRT, self.read32(DWC2_HPRT) & !DWC2_HPRT_PRTRST);
        delay_ns(20_000_000);
        // 等待使能。
        for _ in 0..RESET_TIMEOUT_LOOPS {
            let hprt = self.read32(DWC2_HPRT);
            if hprt & DWC2_HPRT_PRTENA != 0 {
                let speed = (hprt & DWC2_HPRT_PRTSPD_MASK) >> 17;
                return Ok(if speed == 1 {
                    USB_SPEED_FULL
                } else if speed == 2 {
                    USB_SPEED_LOW
                } else {
                    USB_SPEED_HIGH
                });
            }
            if hprt & DWC2_HPRT_PRTCONNSTS == 0 {
                return Err("dwc2 device disconnected during reset");
            }
            delay_ns(1_000);
        }
        Err("dwc2 port enable timeout")
    }

    fn control_transfer(
        &self,
        dev_addr: u8,
        setup: &UsbSetup,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let _guard = self.lock.lock();
        // SETUP（DATA0 PID 由 core 在 setup 阶段固定，PID 域写 SETUP）。
        self.control_phase(
            dev_addr,
            false,
            DWC2_HCTSIZ_PID_SETUP,
            (setup as *const UsbSetup).cast::<u8>(),
            core::mem::size_of::<UsbSetup>(),
        )?;
        if !data.is_empty() {
            self.control_phase(
                dev_addr,
                data_in,
                DWC2_HCTSIZ_PID_DATA1,
                data.as_ptr(),
                data.len(),
            )?;
        }
        // STATUS 阶段方向与数据阶段相反。
        let status_in = !data_in || data.is_empty();
        self.control_phase(
            dev_addr,
            status_in,
            DWC2_HCTSIZ_PID_DATA1,
            core::ptr::null(),
            0,
        )?;
        Ok(data.len())
    }

    fn bulk_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let _guard = self.lock.lock();
        let channel = self.alloc_channel();
        // 批量 toggle 由 dwc2 core 在通道内维护（HCCHAR 不重置时保持）。
        self.start_channel(
            channel,
            dev_addr,
            ep & 0x0f,
            data_in,
            DWC2_HCCHAR_EPTYPE_BULK,
            512,
            DWC2_HCTSIZ_PID_DATA1,
            data.len(),
            data.as_ptr(),
        )?;
        self.channel_wait(channel)?;
        Ok(data.len())
    }

    fn interrupt_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let _guard = self.lock.lock();
        let channel = self.alloc_channel();
        self.start_channel(
            channel,
            dev_addr,
            ep & 0x0f,
            data_in,
            DWC2_HCCHAR_EPTYPE_INTERRUPT,
            64,
            DWC2_HCTSIZ_PID_DATA1,
            data.len(),
            data.as_ptr(),
        )?;
        self.channel_wait(channel)?;
        Ok(data.len())
    }
}
