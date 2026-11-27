//! Bounded, nonblocking R8V1 backlog. A partial TCP record stays in order.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    time::Instant,
};

pub const MIN_BLOCK_BYTES: usize = 16384;
pub const MAX_BLOCK_BYTES: usize = 65536;
pub const MAX_FRAMES: usize = 8;
const NO_CRC: u32 = 1 << 31;

#[derive(Clone, Copy, Default)]
pub enum Packing {
    #[default]
    Raw,
    Packed,
    Diagnostic,
}

impl Packing {
    pub fn poll(&mut self, socket: &mut impl Read) -> io::Result<()> {
        let mut commands = [0; 32];
        let count = match socket.read(&mut commands) {
            Ok(n) => n,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => return Ok(()),
            Err(error) => return Err(error),
        };
        for &command in &commands[..count] {
            *self = match command {
                b'R' => Self::Raw,
                b'B' => Self::Packed,
                b'D' => Self::Diagnostic,
                _ => *self,
            };
        }
        Ok(())
    }
}

fn diagnostic_crc(payload: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in payload {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[cfg(all(target_arch = "arm", target_os = "linux"))]
unsafe extern "C" {
    fn real8_pack(input: *const u8, size: usize, dst: *mut u8) -> usize;
}

#[cfg(all(target_arch = "arm", target_os = "linux"))]
fn append_packed(buffer: &mut Vec<u8>, payload: &[u8]) -> bool {
    buffer.reserve(payload.len());
    let offset = buffer.len();
    // The codec writes at most payload.len() bytes, or returns zero for a raw fallback.
    let written = unsafe { real8_pack(payload.as_ptr(), payload.len(), buffer.as_mut_ptr().add(offset)) };
    if written == 0 { return false; }
    unsafe { buffer.set_len(offset + written); }
    true
}

#[cfg(not(all(target_arch = "arm", target_os = "linux")))]
fn append_packed(buffer: &mut Vec<u8>, payload: &[u8]) -> bool {
    let start = buffer.len();
    let mut groups = payload.chunks_exact(32);
    for group in &mut groups {
        let (lo, hi) = group.iter().fold((i8::MAX, i8::MIN), |(lo, hi), &x| (lo.min(x as i8), hi.max(x as i8)));
        let spread = (i16::from(hi) - i16::from(lo)) as u8;
        let width = 8 - spread.leading_zeros();
        buffer.push(width as u8);
        if width == 8 {
            buffer.extend_from_slice(group);
            continue;
        }
        buffer.push(lo as u8);
        let mut acc = 0u32;
        let mut bits = 0;
        for &x in group {
            acc |= u32::from(x.wrapping_sub(lo as u8)) << bits;
            bits += width;
            if bits >= 8 {
                buffer.push(acc as u8);
                acc >>= 8;
                bits -= 8;
            }
        }
    }
    buffer.extend_from_slice(groups.remainder());
    if buffer.len() - start < payload.len() { return true; }
    buffer.truncate(start);
    false
}

struct Packet {
    buffer: Vec<u8>,
    offset: usize,
    payload: usize,
    ready: Instant,
}

#[derive(Default)]
pub struct Counters {
    pub source_bytes: u64,
    pub source_reads: u64,
    pub source_errors: u64,
    pub source_read_ns: u128,
    pub packed_bytes: u64,
    pub packed_frames: u64,
    pub pack_ns: u128,
    pub bytes: u64,
    pub frames: u64,
    pub wire_bytes: u64,
    pub send_calls: u64,
    pub send_errors: u64,
    pub send_ns: u128,
    pub would_block: u64,
    pub overflow: u64,
    pub high_frames: usize,
    pub high_payload_bytes: usize,
    pub max_ready_to_send_ns: u128,
    pub ready_to_send_ns: u128,
    pub last_socket_errno: Option<i32>,
}

pub struct Queue {
    code: u8,
    pending: VecDeque<Packet>,
    buffers: Vec<Vec<u8>>,
    pub counts: Counters,
}

impl Queue {
    pub fn new(code: u8) -> Self {
        Self {
            code,
            pending: VecDeque::with_capacity(MAX_FRAMES),
            buffers: Vec::with_capacity(MAX_FRAMES),
            counts: Counters::default(),
        }
    }

    pub fn frames(&self) -> usize {
        self.pending.len()
    }

    pub fn payload_bytes(&self) -> u64 {
        self.counts.packed_bytes - self.counts.bytes
    }

    pub fn push(&mut self, generation: u64, payload: &[u8], ready: Instant, mode: Packing) -> io::Result<()> {
        if self.frames() == MAX_FRAMES {
            self.counts.overflow += 1;
            return Err(io::Error::other("real8 transport queue exhausted"));
        }
        if payload.is_empty() || payload.len() > MAX_BLOCK_BYTES || payload.len() % 4 != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unaligned real8 block"));
        }
        let start = Instant::now();
        let mut buffer = self.buffers.pop().unwrap_or_else(|| Vec::with_capacity(MAX_BLOCK_BYTES + 192));
        buffer.clear();
        let packed = !matches!(mode, Packing::Raw);
        let header = if packed { 24 } else { 12 };
        buffer.extend_from_slice(if packed { b"R8B1" } else { b"R8V1" });
        buffer.extend_from_slice(&[0; 4]);
        buffer.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buffer.resize(header, 0);
        let pos = self.counts.packed_bytes;
        let seq = self.counts.packed_frames as u32;
        write!(buffer, "{{\"generation\":{generation},\"pos\":{pos},\"seq\":{seq},\"code\":{},\"g\":0,\"reset\":{}}}",
            self.code, self.counts.packed_frames == 0)?;
        let payload_offset = buffer.len();
        let meta = (payload_offset - header) as u32;
        buffer[4..8].copy_from_slice(&meta.to_le_bytes());
        if !packed || !append_packed(&mut buffer, payload) {
            buffer.extend_from_slice(payload);
        }
        if packed {
            let stored = (buffer.len() - payload_offset) as u32;
            buffer[12..16].copy_from_slice(&stored.to_le_bytes());
            let diagnostic = matches!(mode, Packing::Diagnostic);
            if diagnostic {
                buffer[16..20].copy_from_slice(&diagnostic_crc(payload).to_le_bytes());
            }
            let elapsed = (start.elapsed().as_micros() as u32).min(NO_CRC - 1);
            let flags = elapsed | if diagnostic { 0 } else { NO_CRC };
            buffer[20..24].copy_from_slice(&flags.to_le_bytes());
        }
        self.pending.push_back(Packet { buffer, offset: 0, payload: payload.len(), ready });
        self.counts.packed_bytes += payload.len() as u64;
        self.counts.packed_frames += 1;
        self.counts.high_frames = self.counts.high_frames.max(self.frames());
        self.counts.high_payload_bytes = self.counts.high_payload_bytes.max(self.payload_bytes() as usize);
        self.counts.pack_ns += start.elapsed().as_nanos();
        Ok(())
    }

    pub fn pump(&mut self, writer: &mut impl Write) -> io::Result<bool> {
        let Some(packet) = self.pending.front_mut() else {
            return Ok(false);
        };
        let start = Instant::now();
        let transferred = writer.write(&packet.buffer[packet.offset..]);
        self.counts.send_ns += start.elapsed().as_nanos();
        self.counts.send_calls += 1;
        match transferred {
            Ok(0) => {
                self.counts.send_errors += 1;
                Err(io::Error::from(io::ErrorKind::WriteZero))
            }
            Ok(written) => {
                self.counts.wire_bytes += written as u64;
                packet.offset += written;
                if packet.offset == packet.buffer.len() {
                    let packet = self.pending.pop_front().unwrap();
                    self.counts.bytes += packet.payload as u64;
                    self.counts.frames += 1;
                    let age = packet.ready.elapsed().as_nanos();
                    self.counts.ready_to_send_ns += age;
                    self.counts.max_ready_to_send_ns = self.counts.max_ready_to_send_ns.max(age);
                    self.buffers.push(packet.buffer);
                }
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.counts.would_block += 1;
                Ok(false)
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(false),
            Err(err) => {
                self.counts.send_errors += 1;
                self.counts.last_socket_errno = err.raw_os_error();
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Partial {
        blocked: bool,
        result: Vec<u8>,
    }
    impl Write for Partial {
        fn write(&mut self, source: &[u8]) -> io::Result<usize> {
            self.blocked = !self.blocked;
            if self.blocked {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = source.len().min(19);
            self.result.extend_from_slice(&source[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn partial_writes_do_not_change_position_or_order() {
        let mut q = Queue::new(24);
        q.push(9, &[1; 20], Instant::now(), Packing::Raw).unwrap();
        q.push(9, &[2; 12], Instant::now(), Packing::Raw).unwrap();
        let mut socket = Partial { blocked: false, result: Vec::new() };
        while q.frames() > 0 {
            q.pump(&mut socket).unwrap();
        }
        assert_eq!(q.counts.bytes, 32);
        assert_eq!(q.counts.frames, 2);
        assert_eq!(q.counts.wire_bytes, socket.result.len() as u64);
        assert!(q.counts.would_block > 0);
        let meta = u32::from_le_bytes(socket.result[4..8].try_into().unwrap()) as usize;
        assert_eq!(&socket.result[..4], b"R8V1");
        assert_eq!(&socket.result[12+meta..32+meta], &[1; 20]);
        assert_eq!(&socket.result[32+meta..36+meta], b"R8V1");
        let json = std::str::from_utf8(&socket.result[44+meta..socket.result.len()-12]).unwrap();
        assert!(json.contains("\"pos\":20,\"seq\":1"));
    }

    #[test]
    fn saturation_is_reported_without_eviction() {
        let mut q = Queue::new(24);
        for _ in 0..MAX_FRAMES {
            q.push(3, &[9; 64], Instant::now(), Packing::Raw).unwrap();
        }
        assert!(q.push(3, &[7; 64], Instant::now(), Packing::Raw).is_err());
        assert_eq!(q.counts.overflow, 1);
        assert_eq!(q.payload_bytes(), (64 * MAX_FRAMES) as u64);
        let mut sink = Vec::new();
        while q.frames() > 0 {
            q.pump(&mut sink).unwrap();
        }
        assert_eq!(q.counts.frames, MAX_FRAMES as u64);
    }

    #[test]
    fn packed_and_diagnostic_modes_share_sample_positions() {
        let mut queue = Queue::new(28);
        let mut packing = Packing::Raw;
        packing.poll(&mut &b"B"[..]).unwrap();
        queue.push(7, &[3; 64], Instant::now(), packing).unwrap();
        packing.poll(&mut &b"D"[..]).unwrap();
        queue.push(7, &[4; 32], Instant::now(), packing).unwrap();
        let mut out = Partial { blocked: false, result: Vec::new() };
        while queue.frames() != 0 { queue.pump(&mut out).unwrap(); }
        let mut offset = 0;
        for (i, samples, value, crc) in [(0, 64, 3u8, false), (1, 32, 4u8, true)] {
            let block = &out.result[offset..];
            let word = |start| u32::from_le_bytes(block[start..start+4].try_into().unwrap());
            assert_eq!(&block[..4], b"R8B1");
            let meta = word(4) as usize;
            assert_eq!(word(8), samples);
            assert_eq!(word(12), samples / 16);
            assert_eq!(word(16), if crc { diagnostic_crc(&vec![value; samples as usize]) } else { 0 });
            assert_eq!(word(20) & NO_CRC == 0, crc);
            let meta_text = std::str::from_utf8(&block[24..24+meta]).unwrap();
            assert!(meta_text.contains(&format!("\"pos\":{},\"seq\":{i}", i * 64)));
            for group in block[24+meta..24+meta+word(12) as usize].chunks_exact(2) {
                assert_eq!(group, &[0, value]);
            }
            offset += 24 + meta + word(12) as usize;
        }
        assert_eq!(offset, out.result.len());
        assert_eq!(queue.counts.bytes, 96);
    }
}
