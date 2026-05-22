use alloc::vec::Vec;
use core::fmt::{self, Write};

use allocator::MemorySegment;

// ── Log sink line buffer ──────────────────────────────────────────────────────

const SINK_LINE_BUFFER_SIZE: usize = 1280;

pub(crate) struct SinkLineBuffer {
    buf: [u8; SINK_LINE_BUFFER_SIZE],
    len: usize,
}

impl SinkLineBuffer {
    pub(crate) const fn new() -> Self {
        Self {
            buf: [0; SINK_LINE_BUFFER_SIZE],
            len: 0,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl fmt::Write for SinkLineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.len >= self.buf.len() {
            return Ok(());
        }
        let available = self.buf.len() - self.len;
        let copy_len = s.len().min(available);
        self.buf[self.len..self.len + copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
        self.len += copy_len;
        Ok(())
    }
}

pub(crate) fn format_log_record_line(record: &log::LogRecord<'_>) -> SinkLineBuffer {
    let (secs, nanos) = log::format_timestamp(record.timestamp);
    let mut buf = SinkLineBuffer::new();
    let _ = writeln!(
        &mut buf,
        "[{:6}.{:06}] {}",
        secs,
        nanos / 1000,
        record.message
    );
    buf
}

// ── Memory segment helpers ────────────────────────────────────────────────────

pub(crate) fn normalize_memory_segments(
    mut segments: Vec<MemorySegment>,
) -> Option<Vec<MemorySegment>> {
    if segments.is_empty() {
        return None;
    }
    segments.sort_unstable_by_key(|s| s.start);
    let mut merged: Vec<MemorySegment> = Vec::with_capacity(segments.len());
    for seg in segments {
        if seg.size == 0 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            let last_end = last.start.saturating_add(last.size);
            if last_end >= seg.start {
                let merged_end = last_end.max(seg.start.saturating_add(seg.size));
                last.size = merged_end.saturating_sub(last.start);
                continue;
            }
        }
        merged.push(seg);
    }
    (!merged.is_empty()).then_some(merged)
}

pub(crate) fn intersect_memory_segments(
    lhs: &[MemorySegment],
    rhs: &[MemorySegment],
) -> Option<Vec<MemorySegment>> {
    let lhs = normalize_memory_segments(lhs.to_vec())?;
    let rhs = normalize_memory_segments(rhs.to_vec())?;
    let mut result = Vec::new();
    let (mut li, mut ri) = (0usize, 0usize);
    while li < lhs.len() && ri < rhs.len() {
        let start = lhs[li].start.max(rhs[ri].start);
        let end = lhs[li].end().min(rhs[ri].end());
        if start < end {
            result.push(MemorySegment {
                start,
                size: end - start,
            });
        }
        if lhs[li].end() <= rhs[ri].end() {
            li += 1;
        } else {
            ri += 1;
        }
    }
    normalize_memory_segments(result)
}
