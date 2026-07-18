//! TCP 线格式、序号运算与确定性连接状态机。

use core::ops::{Add, AddAssign, BitOr, BitOrAssign};

use crate::buf::{DropReason, PacketChain};
use crate::pipeline::{IpPacket, transport_checksum};

pub const TCP_PROTOCOL_NUMBER: u8 = 6;
pub const TCP_MIN_HEADER_LEN: usize = 20;
pub const TCP_MAX_HEADER_LEN: usize = 60;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TcpSequence(pub u32);

impl TcpSequence {
    pub const fn wrapping_add(self, value: u32) -> Self {
        Self(self.0.wrapping_add(value))
    }

    pub const fn wrapping_sub(self, value: u32) -> Self {
        Self(self.0.wrapping_sub(value))
    }

    pub const fn before(self, other: Self) -> bool {
        (self.0.wrapping_sub(other.0) as i32) < 0
    }

    pub const fn after(self, other: Self) -> bool {
        other.before(self)
    }

    pub const fn before_or_equal(self, other: Self) -> bool {
        self.0 == other.0 || self.before(other)
    }

    pub const fn after_or_equal(self, other: Self) -> bool {
        self.0 == other.0 || self.after(other)
    }

    pub const fn distance_from(self, start: Self) -> u32 {
        self.0.wrapping_sub(start.0)
    }

    pub const fn in_window(self, start: Self, len: u32) -> bool {
        self.distance_from(start) < len
    }
}

impl Add<u32> for TcpSequence {
    type Output = Self;

    fn add(self, rhs: u32) -> Self::Output {
        self.wrapping_add(rhs)
    }
}

impl AddAssign<u32> for TcpSequence {
    fn add_assign(&mut self, rhs: u32) {
        *self = self.wrapping_add(rhs);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpFlags(u16);

impl TcpFlags {
    pub const FIN: Self = Self(1 << 0);
    pub const SYN: Self = Self(1 << 1);
    pub const RST: Self = Self(1 << 2);
    pub const PSH: Self = Self(1 << 3);
    pub const ACK: Self = Self(1 << 4);
    pub const URG: Self = Self(1 << 5);
    pub const ECE: Self = Self(1 << 6);
    pub const CWR: Self = Self(1 << 7);
    pub const NS: Self = Self(1 << 8);

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & 0x01ff)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    pub const fn intersects(self, flags: Self) -> bool {
        self.0 & flags.0 != 0
    }
}

impl BitOr for TcpFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TcpFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpSackBlock {
    pub left: TcpSequence,
    pub right: TcpSequence,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpTimestamp {
    pub value: u32,
    pub echo_reply: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpOptions {
    pub maximum_segment_size: Option<u16>,
    pub window_scale: Option<u8>,
    pub sack_permitted: bool,
    pub sack_blocks: [Option<TcpSackBlock>; 4],
    pub timestamp: Option<TcpTimestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpPacket {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: TcpSequence,
    pub acknowledgement: TcpSequence,
    pub flags: TcpFlags,
    pub window: u16,
    pub urgent_pointer: u16,
    pub header_len: u16,
    pub payload_offset: u16,
    pub payload_len: u32,
    pub options: TcpOptions,
}

impl TcpPacket {
    pub const fn sequence_len(self) -> u32 {
        self.payload_len
            + self.flags.contains(TcpFlags::SYN) as u32
            + self.flags.contains(TcpFlags::FIN) as u32
    }

    pub const fn segment(self) -> TcpSegment {
        TcpSegment {
            sequence: self.sequence,
            acknowledgement: self.acknowledgement,
            flags: self.flags,
            window: self.window,
            payload_len: self.payload_len,
        }
    }
}

pub(crate) fn parse_tcp_packet(chain: &PacketChain, ip: IpPacket) -> Result<TcpPacket, DropReason> {
    parse_tcp_packet_inner(chain, ip, true)
}

pub(crate) fn parse_tcp_packet_trusted(
    chain: &PacketChain,
    ip: IpPacket,
) -> Result<TcpPacket, DropReason> {
    parse_tcp_packet_inner(chain, ip, false)
}

fn parse_tcp_packet_inner(
    chain: &PacketChain,
    ip: IpPacket,
    verify_checksum: bool,
) -> Result<TcpPacket, DropReason> {
    if ip.payload_len < TCP_MIN_HEADER_LEN as u32 {
        return Err(DropReason::MalformedTcp);
    }
    let mut header = [0u8; TCP_MIN_HEADER_LEN];
    chain
        .copy_out(usize::from(ip.payload_offset), &mut header)
        .map_err(|_| DropReason::MalformedTcp)?;

    let header_len = usize::from(header[12] >> 4) * 4;
    if !(TCP_MIN_HEADER_LEN..=TCP_MAX_HEADER_LEN).contains(&header_len)
        || header_len > ip.payload_len as usize
        || header[12] & 0x0e != 0
    {
        return Err(DropReason::MalformedTcp);
    }

    let source_port = u16::from_be_bytes([header[0], header[1]]);
    let destination_port = u16::from_be_bytes([header[2], header[3]]);

    let options = parse_options(
        chain,
        usize::from(ip.payload_offset) + TCP_MIN_HEADER_LEN,
        header_len,
    )?;
    if verify_checksum {
        let checksum = transport_checksum(
            chain,
            usize::from(ip.payload_offset),
            ip.payload_len as usize,
            ip.source,
            ip.destination,
            TCP_PROTOCOL_NUMBER,
        )
        .map_err(|_| DropReason::MalformedTcp)?;
        if checksum != 0 {
            return Err(DropReason::TcpChecksum);
        }
    }

    let flags = u16::from(header[12] & 1) << 8 | u16::from(header[13]);
    Ok(TcpPacket {
        source_port,
        destination_port,
        sequence: TcpSequence(u32::from_be_bytes(header[4..8].try_into().unwrap())),
        acknowledgement: TcpSequence(u32::from_be_bytes(header[8..12].try_into().unwrap())),
        flags: TcpFlags::from_bits(flags),
        window: u16::from_be_bytes([header[14], header[15]]),
        urgent_pointer: u16::from_be_bytes([header[18], header[19]]),
        header_len: header_len as u16,
        payload_offset: ip.payload_offset + header_len as u16,
        payload_len: ip.payload_len - header_len as u32,
        options,
    })
}

fn parse_options(
    chain: &PacketChain,
    offset: usize,
    header_len: usize,
) -> Result<TcpOptions, DropReason> {
    let options_len = header_len - TCP_MIN_HEADER_LEN;
    let mut bytes = [0u8; TCP_MAX_HEADER_LEN - TCP_MIN_HEADER_LEN];
    chain
        .copy_out(offset, &mut bytes[..options_len])
        .map_err(|_| DropReason::MalformedTcp)?;

    let mut parsed = TcpOptions::default();
    let mut sack_seen = false;
    let mut cursor = 0usize;
    while cursor < options_len {
        let kind = bytes[cursor];
        match kind {
            0 => {
                if bytes[cursor..options_len].iter().any(|byte| *byte != 0) {
                    return Err(DropReason::MalformedTcp);
                }
                break;
            }
            1 => {
                cursor += 1;
                continue;
            }
            _ => {}
        }
        if cursor + 2 > options_len {
            return Err(DropReason::MalformedTcp);
        }
        let len = usize::from(bytes[cursor + 1]);
        if len < 2 || cursor + len > options_len {
            return Err(DropReason::MalformedTcp);
        }
        let option = &bytes[cursor..cursor + len];
        match kind {
            2 => {
                if len != 4 || parsed.maximum_segment_size.is_some() {
                    return Err(DropReason::MalformedTcp);
                }
                let mss = u16::from_be_bytes([option[2], option[3]]);
                if mss == 0 {
                    return Err(DropReason::MalformedTcp);
                }
                parsed.maximum_segment_size = Some(mss);
            }
            3 => {
                if len != 3 || parsed.window_scale.is_some() {
                    return Err(DropReason::MalformedTcp);
                }
                parsed.window_scale = Some(option[2].min(14));
            }
            4 => {
                if len != 2 || parsed.sack_permitted {
                    return Err(DropReason::MalformedTcp);
                }
                parsed.sack_permitted = true;
            }
            5 => {
                if sack_seen || len < 10 || len > 34 || (len - 2) % 8 != 0 {
                    return Err(DropReason::MalformedTcp);
                }
                sack_seen = true;
                for (index, block) in option[2..].chunks_exact(8).enumerate() {
                    parsed.sack_blocks[index] = Some(TcpSackBlock {
                        left: TcpSequence(u32::from_be_bytes(block[0..4].try_into().unwrap())),
                        right: TcpSequence(u32::from_be_bytes(block[4..8].try_into().unwrap())),
                    });
                }
            }
            8 => {
                if len != 10 || parsed.timestamp.is_some() {
                    return Err(DropReason::MalformedTcp);
                }
                parsed.timestamp = Some(TcpTimestamp {
                    value: u32::from_be_bytes(option[2..6].try_into().unwrap()),
                    echo_reply: u32::from_be_bytes(option[6..10].try_into().unwrap()),
                });
            }
            _ => {}
        }
        cursor += len;
    }
    Ok(parsed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSegment {
    pub sequence: TcpSequence,
    pub acknowledgement: TcpSequence,
    pub flags: TcpFlags,
    pub window: u16,
    pub payload_len: u32,
}

impl TcpSegment {
    pub const fn sequence_len(self) -> u32 {
        self.payload_len
            + self.flags.contains(TcpFlags::SYN) as u32
            + self.flags.contains(TcpFlags::FIN) as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpTransmit {
    pub sequence: TcpSequence,
    pub acknowledgement: TcpSequence,
    pub flags: TcpFlags,
    pub window: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpMachineOutput {
    pub transmit: Option<TcpTransmit>,
    pub state_changed: bool,
    pub established: bool,
    pub closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpStateMachine {
    state: TcpState,
    initial_send_sequence: TcpSequence,
    send_unacknowledged: TcpSequence,
    send_next: TcpSequence,
    receive_next: TcpSequence,
    receive_window: u16,
}

impl TcpStateMachine {
    pub const fn new(initial_send_sequence: TcpSequence, receive_window: u16) -> Self {
        Self {
            state: TcpState::Closed,
            initial_send_sequence,
            send_unacknowledged: initial_send_sequence,
            send_next: initial_send_sequence,
            receive_next: TcpSequence(0),
            receive_window,
        }
    }

    pub const fn state(&self) -> TcpState {
        self.state
    }

    pub const fn send_unacknowledged(&self) -> TcpSequence {
        self.send_unacknowledged
    }

    pub const fn send_next(&self) -> TcpSequence {
        self.send_next
    }

    pub const fn receive_next(&self) -> TcpSequence {
        self.receive_next
    }

    pub fn reserve_send(&mut self, len: u32) -> Option<TcpSequence> {
        if len == 0 || !matches!(self.state, TcpState::Established | TcpState::CloseWait) {
            return None;
        }
        let sequence = self.send_next;
        self.send_next += len;
        Some(sequence)
    }

    pub fn advance_receive(&mut self, len: u32) -> bool {
        if len == 0
            || !matches!(
                self.state,
                TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
            )
        {
            return false;
        }
        self.receive_next += len;
        true
    }

    pub fn expire_time_wait(&mut self) -> bool {
        if self.state != TcpState::TimeWait {
            return false;
        }
        self.state = TcpState::Closed;
        true
    }

    pub fn listen(&mut self) -> bool {
        if self.state != TcpState::Closed {
            return false;
        }
        self.state = TcpState::Listen;
        true
    }

    pub fn active_open(&mut self) -> Option<TcpTransmit> {
        if self.state != TcpState::Closed {
            return None;
        }
        self.send_unacknowledged = self.initial_send_sequence;
        self.send_next = self.initial_send_sequence + 1;
        self.state = TcpState::SynSent;
        Some(TcpTransmit {
            sequence: self.initial_send_sequence,
            acknowledgement: TcpSequence(0),
            flags: TcpFlags::SYN,
            window: self.receive_window,
        })
    }

    pub fn close(&mut self) -> TcpMachineOutput {
        let previous = self.state;
        let transmit = match self.state {
            TcpState::Listen | TcpState::SynSent => {
                self.state = TcpState::Closed;
                None
            }
            TcpState::Established => {
                let transmit = self.transmit(TcpFlags::FIN | TcpFlags::ACK);
                self.send_next += 1;
                self.state = TcpState::FinWait1;
                Some(transmit)
            }
            TcpState::CloseWait => {
                let transmit = self.transmit(TcpFlags::FIN | TcpFlags::ACK);
                self.send_next += 1;
                self.state = TcpState::LastAck;
                Some(transmit)
            }
            _ => None,
        };
        self.output(previous, transmit, false, self.state == TcpState::Closed)
    }

    pub fn on_segment(&mut self, segment: TcpSegment) -> TcpMachineOutput {
        let previous = self.state;
        if segment.flags.contains(TcpFlags::RST) {
            if !matches!(self.state, TcpState::Closed | TcpState::Listen) {
                self.state = TcpState::Closed;
                return self.output(previous, None, false, true);
            }
            return self.output(previous, None, false, false);
        }

        let mut transmit = None;
        let mut established = false;
        let mut closed = false;
        match self.state {
            TcpState::Closed => {}
            TcpState::Listen => {
                if segment.flags.contains(TcpFlags::SYN) {
                    self.receive_next = segment.sequence + 1;
                    self.send_unacknowledged = self.initial_send_sequence;
                    self.send_next = self.initial_send_sequence + 1;
                    self.state = TcpState::SynReceived;
                    transmit = Some(self.transmit(TcpFlags::SYN | TcpFlags::ACK));
                }
            }
            TcpState::SynSent => {
                let acknowledges_syn = segment.flags.contains(TcpFlags::ACK)
                    && segment.acknowledgement == self.send_next;
                if segment.flags.contains(TcpFlags::SYN) && acknowledges_syn {
                    self.send_unacknowledged = segment.acknowledgement;
                    self.receive_next = segment.sequence + 1;
                    self.state = TcpState::Established;
                    transmit = Some(self.transmit(TcpFlags::ACK));
                    established = true;
                } else if segment.flags.contains(TcpFlags::SYN)
                    && !segment.flags.contains(TcpFlags::ACK)
                {
                    self.receive_next = segment.sequence + 1;
                    self.state = TcpState::SynReceived;
                    transmit = Some(self.transmit(TcpFlags::SYN | TcpFlags::ACK));
                }
            }
            TcpState::SynReceived => {
                if segment.flags.contains(TcpFlags::ACK)
                    && segment.sequence == self.receive_next
                    && self.accept_ack(segment.acknowledgement)
                {
                    self.state = TcpState::Established;
                    established = true;
                }
            }
            TcpState::Established => {
                self.accept_segment_ack(segment);
                if self.accept_payload(segment) {
                    transmit = Some(self.transmit(TcpFlags::ACK));
                }
                if self.accept_fin(segment) {
                    self.state = TcpState::CloseWait;
                    transmit = Some(self.transmit(TcpFlags::ACK));
                }
            }
            TcpState::FinWait1 => {
                let fin_acked =
                    self.accept_segment_ack(segment) && self.send_unacknowledged == self.send_next;
                self.accept_payload(segment);
                if self.accept_fin(segment) {
                    self.state = if fin_acked {
                        TcpState::TimeWait
                    } else {
                        TcpState::Closing
                    };
                    transmit = Some(self.transmit(TcpFlags::ACK));
                } else if fin_acked {
                    self.state = TcpState::FinWait2;
                }
            }
            TcpState::FinWait2 => {
                self.accept_segment_ack(segment);
                self.accept_payload(segment);
                if self.accept_fin(segment) {
                    self.state = TcpState::TimeWait;
                    transmit = Some(self.transmit(TcpFlags::ACK));
                }
            }
            TcpState::CloseWait => {
                self.accept_segment_ack(segment);
            }
            TcpState::Closing => {
                if self.accept_segment_ack(segment) && self.send_unacknowledged == self.send_next {
                    self.state = TcpState::TimeWait;
                }
            }
            TcpState::LastAck => {
                if self.accept_segment_ack(segment) && self.send_unacknowledged == self.send_next {
                    self.state = TcpState::Closed;
                    closed = true;
                }
            }
            TcpState::TimeWait => {
                if segment.flags.contains(TcpFlags::FIN) {
                    transmit = Some(self.transmit(TcpFlags::ACK));
                }
            }
        }
        self.output(previous, transmit, established, closed)
    }

    fn accept_ack(&mut self, acknowledgement: TcpSequence) -> bool {
        if self.send_unacknowledged.before(acknowledgement)
            && acknowledgement.before_or_equal(self.send_next)
        {
            self.send_unacknowledged = acknowledgement;
            true
        } else {
            acknowledgement == self.send_unacknowledged
        }
    }

    fn accept_segment_ack(&mut self, segment: TcpSegment) -> bool {
        segment.flags.contains(TcpFlags::ACK) && self.accept_ack(segment.acknowledgement)
    }

    fn accept_payload(&mut self, segment: TcpSegment) -> bool {
        if segment.payload_len == 0 || segment.sequence != self.receive_next {
            return false;
        }
        self.receive_next += segment.payload_len;
        true
    }

    fn accept_fin(&mut self, segment: TcpSegment) -> bool {
        let fin_sequence = segment.sequence + segment.payload_len;
        if !segment.flags.contains(TcpFlags::FIN) || fin_sequence != self.receive_next {
            return false;
        }
        self.receive_next += 1;
        true
    }

    fn transmit(&self, flags: TcpFlags) -> TcpTransmit {
        TcpTransmit {
            sequence: if flags.contains(TcpFlags::SYN) {
                self.initial_send_sequence
            } else {
                self.send_next
            },
            acknowledgement: self.receive_next,
            flags,
            window: self.receive_window,
        }
    }

    fn output(
        &self,
        previous: TcpState,
        transmit: Option<TcpTransmit>,
        established: bool,
        closed: bool,
    ) -> TcpMachineOutput {
        TcpMachineOutput {
            transmit,
            state_changed: previous != self.state,
            established,
            closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use core::ptr::NonNull;

    use super::*;
    use crate::buf::{NetBufPool, NetBufPoolOwner, NetBufStorage, PacketMetadata};
    use crate::{IpAddr, Ipv4Addr, Ipv6Addr};

    struct Storage {
        bytes: Box<[u8]>,
    }

    impl NetBufStorage for Storage {
        fn capacity(&self) -> usize {
            self.bytes.len()
        }

        fn base_ptr(&self) -> NonNull<u8> {
            NonNull::new(self.bytes.as_ptr() as *mut u8).unwrap()
        }

        fn dma_addr(&self) -> Option<u64> {
            None
        }

        fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
        fn sync_for_device(&self, _offset: usize, _len: usize) {}
    }

    fn packet_chain(bytes: &[u8]) -> (NetBufPoolOwner, PacketChain) {
        let storage = vec![Box::new(Storage {
            bytes: vec![0; bytes.len().max(64)].into_boxed_slice(),
        }) as Box<dyn NetBufStorage>]
        .into_boxed_slice();
        let mut owner = NetBufPool::new(storage).unwrap();
        let mut lease = owner
            .lease(0, bytes.len() as u16, PacketMetadata::default())
            .unwrap();
        lease.as_mut_slice().unwrap().copy_from_slice(bytes);
        (owner, PacketChain::from_lease(lease))
    }

    fn ip_packet(source: IpAddr, destination: IpAddr, len: usize) -> IpPacket {
        IpPacket {
            source,
            destination,
            next_header: TCP_PROTOCOL_NUMBER,
            header_len: 0,
            payload_offset: 0,
            payload_len: len as u32,
            hop_limit: 64,
            traffic_class: 0,
            fragment: None,
        }
    }

    fn tcp_chain(
        source: IpAddr,
        destination: IpAddr,
        options: &[u8],
    ) -> (NetBufPoolOwner, PacketChain, IpPacket) {
        assert_eq!(options.len() % 4, 0);
        let mut bytes = vec![0u8; TCP_MIN_HEADER_LEN + options.len() + 4];
        bytes[0..2].copy_from_slice(&1234u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&80u16.to_be_bytes());
        bytes[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        bytes[8..12].copy_from_slice(&7u32.to_be_bytes());
        bytes[12] = (((TCP_MIN_HEADER_LEN + options.len()) / 4) as u8) << 4;
        bytes[13] = (TcpFlags::SYN | TcpFlags::ACK).bits() as u8;
        bytes[14..16].copy_from_slice(&65535u16.to_be_bytes());
        bytes[TCP_MIN_HEADER_LEN..TCP_MIN_HEADER_LEN + options.len()].copy_from_slice(options);
        bytes[TCP_MIN_HEADER_LEN + options.len()..].copy_from_slice(b"data");
        let (owner, mut chain) = packet_chain(&bytes);
        let ip = ip_packet(source, destination, bytes.len());
        let checksum = transport_checksum(
            &chain,
            0,
            bytes.len(),
            source,
            destination,
            TCP_PROTOCOL_NUMBER,
        )
        .unwrap();
        chain.copy_in(16, &checksum.to_be_bytes()).unwrap();
        (owner, chain, ip)
    }

    #[test]
    fn sequence_comparison_handles_wraparound() {
        let start = TcpSequence(u32::MAX - 2);
        let end = start + 6;
        assert!(start.before(end));
        assert!(end.after(start));
        assert_eq!(end.distance_from(start), 6);
        assert!((start + 5).in_window(start, 6));
        assert!(!(start + 6).in_window(start, 6));
    }

    #[test]
    fn parses_syn_ack_and_known_options() {
        let options = [
            2, 4, 0x05, 0xb4, 1, 3, 3, 20, 4, 2, 1, 1, 8, 10, 0, 0, 0, 9, 0, 0, 0, 3, 0, 0,
        ];
        let (_owner, chain, ip) = tcp_chain(
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            &options,
        );
        let packet = parse_tcp_packet(&chain, ip).unwrap();
        assert_eq!(packet.sequence, TcpSequence(u32::MAX));
        assert_eq!(packet.acknowledgement, TcpSequence(7));
        assert!(packet.flags.contains(TcpFlags::SYN | TcpFlags::ACK));
        assert_eq!(packet.payload_len, 4);
        assert_eq!(packet.options.maximum_segment_size, Some(1460));
        assert_eq!(packet.options.window_scale, Some(14));
        assert!(packet.options.sack_permitted);
        assert_eq!(packet.options.timestamp.unwrap().value, 9);
    }

    #[test]
    fn skips_well_formed_unknown_option() {
        let options = [30, 4, 1, 2];
        let (_owner, chain, ip) = tcp_chain(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &options,
        );
        assert!(parse_tcp_packet(&chain, ip).is_ok());
    }

    #[test]
    fn rejects_bad_data_offset_and_option_length() {
        let (_owner, mut chain, ip) = tcp_chain(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &[],
        );
        chain.copy_in(12, &[4 << 4]).unwrap();
        assert_eq!(parse_tcp_packet(&chain, ip), Err(DropReason::MalformedTcp));

        let options = [2, 3, 0, 0];
        let (_owner, chain, ip) = tcp_chain(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &options,
        );
        assert_eq!(parse_tcp_packet(&chain, ip), Err(DropReason::MalformedTcp));

        let options = [0, 1, 1, 1];
        let (_owner, chain, ip) = tcp_chain(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &options,
        );
        assert_eq!(parse_tcp_packet(&chain, ip), Err(DropReason::MalformedTcp));
    }

    #[test]
    fn verifies_ipv4_and_ipv6_checksums() {
        let (_owner4, mut chain4, ip4) = tcp_chain(
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            &[],
        );
        assert!(parse_tcp_packet(&chain4, ip4).is_ok());
        chain4.copy_in(TCP_MIN_HEADER_LEN, b"Data").unwrap();
        assert_eq!(parse_tcp_packet(&chain4, ip4), Err(DropReason::TcpChecksum));

        let (_owner6, chain6, ip6) = tcp_chain(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            &[],
        );
        assert!(parse_tcp_packet(&chain6, ip6).is_ok());
    }

    #[test]
    fn active_and_passive_handshakes_are_deterministic() {
        let mut active = TcpStateMachine::new(TcpSequence(100), 32768);
        assert_eq!(active.active_open().unwrap().flags, TcpFlags::SYN);
        let output = active.on_segment(TcpSegment {
            sequence: TcpSequence(800),
            acknowledgement: TcpSequence(101),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        assert_eq!(active.state(), TcpState::Established);
        assert!(output.established);
        assert_eq!(output.transmit.unwrap().flags, TcpFlags::ACK);
        assert_eq!(active.receive_next(), TcpSequence(801));

        let mut passive = TcpStateMachine::new(TcpSequence(500), 32768);
        assert!(passive.listen());
        let syn_ack = passive
            .on_segment(TcpSegment {
                sequence: TcpSequence(900),
                acknowledgement: TcpSequence(0),
                flags: TcpFlags::SYN,
                window: 65535,
                payload_len: 0,
            })
            .transmit
            .unwrap();
        assert_eq!(syn_ack.flags, TcpFlags::SYN | TcpFlags::ACK);
        assert_eq!(syn_ack.acknowledgement, TcpSequence(901));
        let output = passive.on_segment(TcpSegment {
            sequence: TcpSequence(901),
            acknowledgement: TcpSequence(501),
            flags: TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        assert_eq!(passive.state(), TcpState::Established);
        assert!(output.established);
    }

    #[test]
    fn normal_close_and_reset_reach_terminal_states() {
        let mut machine = TcpStateMachine::new(TcpSequence(100), 32768);
        machine.active_open().unwrap();
        machine.on_segment(TcpSegment {
            sequence: TcpSequence(800),
            acknowledgement: TcpSequence(101),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        assert_eq!(
            machine.close().transmit.unwrap().flags,
            TcpFlags::FIN | TcpFlags::ACK
        );
        machine.on_segment(TcpSegment {
            sequence: TcpSequence(801),
            acknowledgement: TcpSequence(102),
            flags: TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        assert_eq!(machine.state(), TcpState::FinWait2);
        machine.on_segment(TcpSegment {
            sequence: TcpSequence(801),
            acknowledgement: TcpSequence(102),
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        assert_eq!(machine.state(), TcpState::TimeWait);

        let output = machine.on_segment(TcpSegment {
            sequence: TcpSequence(802),
            acknowledgement: TcpSequence(102),
            flags: TcpFlags::RST,
            window: 0,
            payload_len: 0,
        });
        assert_eq!(machine.state(), TcpState::Closed);
        assert!(output.closed);
    }

    #[test]
    fn payload_followed_by_fin_advances_receive_sequence_once() {
        let mut machine = TcpStateMachine::new(TcpSequence(100), 32768);
        machine.active_open().unwrap();
        machine.on_segment(TcpSegment {
            sequence: TcpSequence(800),
            acknowledgement: TcpSequence(101),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 65535,
            payload_len: 0,
        });
        let output = machine.on_segment(TcpSegment {
            sequence: TcpSequence(801),
            acknowledgement: TcpSequence(101),
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            payload_len: 12,
        });
        assert_eq!(machine.state(), TcpState::CloseWait);
        assert_eq!(machine.receive_next(), TcpSequence(814));
        assert_eq!(output.transmit.unwrap().acknowledgement, TcpSequence(814));
    }
}
