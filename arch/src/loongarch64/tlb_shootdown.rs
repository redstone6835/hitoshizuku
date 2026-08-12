use core::sync::atomic::{AtomicUsize, Ordering};

const KIND_MASK: usize = 0b11;
const KIND_PAGE: usize = 0b01;
const KIND_ASID: usize = 0b10;
const KIND_ALL: usize = 0b11;
const ASID_SHIFT: usize = 2;
const HARDWARE_ASID_MASK: usize = 0x3ff;
const PAGE_OFFSET_MASK: usize = 0xfff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TlbFlushOp {
    Page {
        hardware_asid: usize,
        address: usize,
    },
    Asid {
        hardware_asid: usize,
    },
    All,
}

impl TlbFlushOp {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (
                Self::Asid {
                    hardware_asid: left,
                },
                Self::Asid {
                    hardware_asid: right,
                },
            ) if left == right => self,
            (
                Self::Asid {
                    hardware_asid: left,
                },
                Self::Page {
                    hardware_asid: right,
                    ..
                },
            )
            | (
                Self::Page {
                    hardware_asid: right,
                    ..
                },
                Self::Asid {
                    hardware_asid: left,
                },
            ) if left == right => Self::Asid {
                hardware_asid: left,
            },
            (
                Self::Page {
                    hardware_asid: left_asid,
                    address: left_address,
                },
                Self::Page {
                    hardware_asid: right_asid,
                    address: right_address,
                },
            ) if left_asid == right_asid => {
                if left_address == right_address {
                    self
                } else {
                    Self::Asid {
                        hardware_asid: left_asid,
                    }
                }
            }
            _ => Self::All,
        }
    }

    fn encode(self) -> usize {
        match self {
            Self::Page {
                hardware_asid,
                address,
            } => {
                debug_assert_eq!(hardware_asid & !HARDWARE_ASID_MASK, 0);
                debug_assert_eq!(address & PAGE_OFFSET_MASK, 0);
                address | (hardware_asid << ASID_SHIFT) | KIND_PAGE
            }
            Self::Asid { hardware_asid } => {
                debug_assert_eq!(hardware_asid & !HARDWARE_ASID_MASK, 0);
                (hardware_asid << ASID_SHIFT) | KIND_ASID
            }
            Self::All => KIND_ALL,
        }
    }

    fn decode(encoded: usize) -> Option<Self> {
        let hardware_asid = (encoded >> ASID_SHIFT) & HARDWARE_ASID_MASK;
        match encoded & KIND_MASK {
            0 => None,
            KIND_PAGE => Some(Self::Page {
                hardware_asid,
                address: encoded & !PAGE_OFFSET_MASK,
            }),
            KIND_ASID => Some(Self::Asid { hardware_asid }),
            KIND_ALL => Some(Self::All),
            _ => unreachable!(),
        }
    }
}

pub(crate) struct PendingTlbFlush(AtomicUsize);

impl PendingTlbFlush {
    pub(crate) const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    pub(crate) fn merge(&self, operation: TlbFlushOp) {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            let merged = TlbFlushOp::decode(current)
                .map_or(operation, |pending| pending.merge(operation))
                .encode();
            if merged == current {
                return;
            }
            match self
                .0
                .compare_exchange_weak(current, merged, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn take(&self) -> Option<TlbFlushOp> {
        TlbFlushOp::decode(self.0.swap(0, Ordering::AcqRel))
    }
}
