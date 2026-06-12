//! `/dev/cpu_dma_latency` PM QoS compatibility node.
//!
//! This module is the only VFS/user ABI adapter for the Linux CPU DMA latency
//! device. It translates open/read/write/release into typed PM QoS latency
//! requests and keeps devtmpfs free of path-specific device branches.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use vfs::cred::Credentials;
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeOps};

use crate::dev::pm_qos::{
    LatencyConstraintHandle, LatencyQosClass, LatencyQosError, cpu_dma_latency_effective_us,
    open_latency_request,
};
use crate::vfs::device_files::spec::{
    CustomDevNodeKind, CustomDevNodeNumbering, CustomDevNodeSpec, DevNodeSpec,
};
use crate::vfs::devtmpfs::{
    DevTmpfsCustomNodeAdapter, DevTmpfsCustomNodeAdapterRegistration, DevTmpfsStaticNode,
    DevTmpfsStaticNodeRegistration, register_custom_devnode_adapter, register_static_dev_node,
};

const CPU_DMA_LATENCY_NODE_NAME: &str = "cpu_dma_latency";
const CPU_DMA_LATENCY_OWNER: &str = "cpu-dma-latency-devnode";
const CPU_DMA_LATENCY_ADAPTER_NAME: &str = "cpu-dma-latency";
const CPU_DMA_LATENCY_WORD_LEN: usize = core::mem::size_of::<i32>();

struct CpuDmaLatencyEndpoint;

struct CpuDmaLatencyInodeOps;

struct CpuDmaLatencyFileOps {
    request: LatencyConstraintHandle,
}

/// Register the custom devtmpfs adapter for `/dev/cpu_dma_latency`.
pub fn register_devtmpfs_adapter() -> VfsResult<DevTmpfsCustomNodeAdapterRegistration> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        CPU_DMA_LATENCY_OWNER,
        CPU_DMA_LATENCY_ADAPTER_NAME,
        build_cpu_dma_latency_inode_ops,
    ))
}

/// Register the static `/dev/cpu_dma_latency` node declaration.
pub fn register_static_node() -> VfsResult<DevTmpfsStaticNodeRegistration> {
    register_static_dev_node(DevTmpfsStaticNode::new(
        CPU_DMA_LATENCY_OWNER,
        CPU_DMA_LATENCY_NODE_NAME,
        build_cpu_dma_latency_node,
    ))
}

fn build_cpu_dma_latency_node() -> VfsResult<DevNodeSpec> {
    let payload: Arc<dyn Any + Send + Sync> = Arc::new(CpuDmaLatencyEndpoint);
    Ok(DevNodeSpec::custom(
        CustomDevNodeSpec::try_new_with_numbering(
            CPU_DMA_LATENCY_NODE_NAME,
            CustomDevNodeKind::CharDevice,
            payload,
            CustomDevNodeNumbering::MiscChar,
        )?,
    ))
}

fn build_cpu_dma_latency_inode_ops(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    let payload = spec.payload();
    if payload
        .as_ref()
        .downcast_ref::<CpuDmaLatencyEndpoint>()
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(Arc::new(CpuDmaLatencyInodeOps)))
}

impl InodeOps for CpuDmaLatencyInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let request =
            open_latency_request(LatencyQosClass::CpuDmaLatency).map_err(map_pm_qos_vfs_error)?;
        Ok(Box::new(CpuDmaLatencyFileOps { request }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileOps for CpuDmaLatencyFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidArgument)?;
        if offset >= CPU_DMA_LATENCY_WORD_LEN {
            return Ok(0);
        }
        let value = cpu_dma_latency_effective_us().to_le_bytes();
        let n = buf.len().min(CPU_DMA_LATENCY_WORD_LEN - offset);
        buf[..n].copy_from_slice(&value[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() != CPU_DMA_LATENCY_WORD_LEN {
            return Err(VfsError::InvalidArgument);
        }
        let mut raw = [0u8; CPU_DMA_LATENCY_WORD_LEN];
        raw.copy_from_slice(buf);
        let value = i32::from_le_bytes(raw);
        self.request
            .update_us(value)
            .map_err(map_pm_qos_vfs_error)?;
        Ok(CPU_DMA_LATENCY_WORD_LEN)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        PollEvents::POLLIN
            .with(PollEvents::POLLOUT)
            .intersect(interest)
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.request.release();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_pm_qos_vfs_error(err: LatencyQosError) -> VfsError {
    match err {
        LatencyQosError::Invalid => VfsError::InvalidArgument,
        LatencyQosError::NoDevice => VfsError::NoDevice,
        LatencyQosError::NoMemory => VfsError::OutOfMemory,
    }
}
