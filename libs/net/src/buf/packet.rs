//! 无堆分配的固定容量 packet、TX 与 completion batch。

use super::{ChunkRef, NetBufLease, PacketMetadata};
use crate::tuning::{PACKET_BATCH_CAPACITY, PACKET_FRAGMENT_CAPACITY};

/// packet fragment 的独占或共享所有权。
pub enum PacketFragment {
    Exclusive(NetBufLease),
    Shared(ChunkRef),
}

impl PacketFragment {
    pub fn len(&self) -> usize {
        match self {
            Self::Exclusive(lease) => lease.len(),
            Self::Shared(chunk) => chunk.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dma_addr(&self) -> Result<Option<u64>, super::NetBufPoolError> {
        match self {
            Self::Exclusive(lease) => lease.dma_addr(),
            Self::Shared(chunk) => chunk.dma_addr(),
        }
    }

    pub fn sync_for_device(&self) -> Result<(), super::NetBufPoolError> {
        match self {
            Self::Exclusive(lease) => lease.sync_for_device(),
            Self::Shared(chunk) => chunk.sync_for_device(),
        }
    }

    pub fn prepend_zeroed(&mut self, len: u16) -> Result<(), super::NetBufPoolError> {
        match self {
            Self::Exclusive(lease) => lease.prepend_zeroed(len),
            Self::Shared(_) => Err(super::NetBufPoolError::CorruptState),
        }
    }
}

/// 一个逻辑 packet，最多内联 18 个 fragment。
pub struct PacketChain {
    fragments: [Option<PacketFragment>; PACKET_FRAGMENT_CAPACITY],
    len: u8,
    total_len: u32,
}

impl PacketChain {
    pub fn new() -> Self {
        Self {
            fragments: core::array::from_fn(|_| None),
            len: 0,
            total_len: 0,
        }
    }

    pub fn from_lease(lease: NetBufLease) -> Self {
        let mut chain = Self::new();
        chain
            .push(PacketFragment::Exclusive(lease))
            .unwrap_or_else(|_| unreachable!());
        chain
    }

    pub fn push(&mut self, fragment: PacketFragment) -> Result<(), PacketFragment> {
        if self.len as usize == self.fragments.len() {
            return Err(fragment);
        }
        let fragment_len = fragment.len();
        let Some(total_len) = self.total_len.checked_add(fragment_len as u32) else {
            return Err(fragment);
        };
        self.fragments[self.len as usize] = Some(fragment);
        self.len += 1;
        self.total_len = total_len;
        Ok(())
    }

    pub const fn fragment_count(&self) -> usize {
        self.len as usize
    }

    pub const fn total_len(&self) -> usize {
        self.total_len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn fragment(&self, index: usize) -> Option<&PacketFragment> {
        self.fragments.get(index)?.as_ref()
    }

    pub fn fragment_mut(&mut self, index: usize) -> Option<&mut PacketFragment> {
        self.fragments.get_mut(index)?.as_mut()
    }

    pub fn take_fragment(&mut self, index: usize) -> Option<PacketFragment> {
        if index >= self.len as usize {
            return None;
        }
        let fragment = self.fragments[index].take()?;
        self.total_len = self.total_len.saturating_sub(fragment.len() as u32);
        while self.len != 0 && self.fragments[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        Some(fragment)
    }

    pub fn prepend_first_zeroed(&mut self, len: u16) -> Result<(), super::NetBufPoolError> {
        let fragment = self
            .fragment_mut(0)
            .ok_or(super::NetBufPoolError::InvalidRange)?;
        fragment.prepend_zeroed(len)?;
        self.total_len = self
            .total_len
            .checked_add(u32::from(len))
            .ok_or(super::NetBufPoolError::InvalidRange)?;
        Ok(())
    }
}

impl Default for PacketChain {
    fn default() -> Self {
        Self::new()
    }
}

/// RX packet batch 与同索引 sidecar metadata。
pub struct PacketBatch {
    packets: [Option<PacketChain>; PACKET_BATCH_CAPACITY],
    metadata: [Option<PacketMetadata>; PACKET_BATCH_CAPACITY],
    len: u8,
}

impl PacketBatch {
    pub fn new() -> Self {
        Self {
            packets: core::array::from_fn(|_| None),
            metadata: [None; PACKET_BATCH_CAPACITY],
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(
        &mut self,
        packet: PacketChain,
        metadata: PacketMetadata,
    ) -> Result<(), PacketChain> {
        if self.len as usize == self.packets.len() {
            return Err(packet);
        }
        let index = self.len as usize;
        self.packets[index] = Some(packet);
        self.metadata[index] = Some(metadata);
        self.len += 1;
        Ok(())
    }

    pub fn packet(&self, index: usize) -> Option<&PacketChain> {
        (index < self.len())
            .then(|| self.packets[index].as_ref())
            .flatten()
    }

    pub fn metadata(&self, index: usize) -> Option<&PacketMetadata> {
        (index < self.len())
            .then(|| self.metadata[index].as_ref())
            .flatten()
    }

    pub fn metadata_mut(&mut self, index: usize) -> Option<&mut PacketMetadata> {
        (index < self.len())
            .then(|| self.metadata[index].as_mut())
            .flatten()
    }

    /// 取走任意 slot。slot metadata 同时失效，batch 长度只在取尾部时收缩。
    pub fn take(&mut self, index: usize) -> Option<(PacketChain, PacketMetadata)> {
        if index >= self.len() {
            return None;
        }
        let packet = self.packets[index].take()?;
        let metadata = self.metadata[index].take()?;
        while self.len != 0 && self.packets[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        Some((packet, metadata))
    }

    pub fn clear(&mut self) {
        for index in 0..self.len() {
            self.packets[index] = None;
            self.metadata[index] = None;
        }
        self.len = 0;
    }
}

impl Default for PacketBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// worker 交给 queue 的固定 32 项 RX replacement batch。
pub struct RxRefillBatch {
    leases: [Option<NetBufLease>; PACKET_BATCH_CAPACITY],
    len: u8,
}

impl RxRefillBatch {
    pub fn new() -> Self {
        Self {
            leases: core::array::from_fn(|_| None),
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, lease: NetBufLease) -> Result<(), NetBufLease> {
        if self.len as usize == self.leases.len() {
            return Err(lease);
        }
        self.leases[self.len as usize] = Some(lease);
        self.len += 1;
        Ok(())
    }

    /// queue 只应取走已经成功发布的前缀 slot。
    pub fn take(&mut self, index: usize) -> Option<NetBufLease> {
        if index >= self.len() {
            return None;
        }
        let lease = self.leases[index].take();
        while self.len != 0 && self.leases[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        lease
    }

    pub fn put(&mut self, index: usize, lease: NetBufLease) -> Result<(), NetBufLease> {
        if index >= self.leases.len() || self.leases[index].is_some() {
            return Err(lease);
        }
        self.leases[index] = Some(lease);
        self.len = self.len.max(index as u8 + 1);
        Ok(())
    }
}

impl Default for RxRefillBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// TX completion 的稳定 token。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct CompletionToken(pub u64);

/// 一项待提交 packet。
pub struct TxPacket {
    pub chain: PacketChain,
    pub completion: CompletionToken,
    pub low_latency: bool,
}

/// 固定 32 项 TX batch。
pub struct TxBatch {
    packets: [Option<TxPacket>; PACKET_BATCH_CAPACITY],
    len: u8,
}

impl TxBatch {
    pub fn new() -> Self {
        Self {
            packets: core::array::from_fn(|_| None),
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, packet: TxPacket) -> Result<(), TxPacket> {
        if self.len as usize == self.packets.len() {
            return Err(packet);
        }
        self.packets[self.len as usize] = Some(packet);
        self.len += 1;
        Ok(())
    }

    pub fn packet(&self, index: usize) -> Option<&TxPacket> {
        (index < self.len())
            .then(|| self.packets[index].as_ref())
            .flatten()
    }

    /// queue 只应取走已经成功发布的前缀 slot。
    pub fn take(&mut self, index: usize) -> Option<TxPacket> {
        if index >= self.len() {
            return None;
        }
        let packet = self.packets[index].take();
        while self.len != 0 && self.packets[self.len as usize - 1].is_none() {
            self.len -= 1;
        }
        packet
    }

    pub fn put(&mut self, index: usize, packet: TxPacket) -> Result<(), TxPacket> {
        if index >= self.packets.len() || self.packets[index].is_some() {
            return Err(packet);
        }
        self.packets[index] = Some(packet);
        self.len = self.len.max(index as u8 + 1);
        Ok(())
    }
}

impl Default for TxBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// 固定 32 项 completion batch。
pub struct CompletionBatch {
    tokens: [Option<CompletionToken>; PACKET_BATCH_CAPACITY],
    len: u8,
}

impl CompletionBatch {
    pub fn new() -> Self {
        Self {
            tokens: [None; PACKET_BATCH_CAPACITY],
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, token: CompletionToken) -> Result<(), CompletionToken> {
        if self.len as usize == self.tokens.len() {
            return Err(token);
        }
        self.tokens[self.len as usize] = Some(token);
        self.len += 1;
        Ok(())
    }

    pub fn token(&self, index: usize) -> Option<CompletionToken> {
        (index < self.len()).then_some(self.tokens[index]).flatten()
    }

    pub fn clear(&mut self) {
        let len = self.len();
        self.tokens[..len].fill(None);
        self.len = 0;
    }
}

impl Default for CompletionBatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_batches_have_stable_capacity() {
        let rx = PacketBatch::new();
        let tx = TxBatch::new();
        let completion = CompletionBatch::new();
        assert!(rx.is_empty());
        assert!(tx.is_empty());
        assert!(completion.is_empty());
        assert_eq!(
            core::mem::size_of_val(&rx),
            core::mem::size_of::<PacketBatch>()
        );
    }
}
