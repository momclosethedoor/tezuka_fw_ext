//! Bounded, nonblocking R8V1 backlog. A partial TCP record stays in order.

use std::{
    collections::VecDeque,
    io::{self, Write},
    time::Instant,
};

pub const MIN_BLOCK_BYTES: usize = 16384;
pub const MAX_BLOCK_BYTES: usize = 65536;
pub const MAX_FRAMES: usize = 8;

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
    pending: VecDeque<Packet>,
    buffers: Vec<Vec<u8>>,
    pub counts: Counters,
}

impl Queue {
    pub fn new() -> Self {
        Self {
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

    pub fn push(&mut self, generation: u64, payload: &[u8], ready: Instant) -> io::Result<()> {
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
        buffer.extend_from_slice(b"R8V1");
        buffer.extend_from_slice(&[0; 4]);
        buffer.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let pos = self.counts.packed_bytes;
        let seq = self.counts.packed_frames as u32;
        write!(buffer, "{{\"generation\":{generation},\"pos\":{pos},\"seq\":{seq},\"code\":24,\"g\":1,\"reset\":{}}}",
            self.counts.packed_frames == 0)?;
        let meta = (buffer.len() - 12) as u32;
        buffer[4..8].copy_from_slice(&meta.to_le_bytes());
        buffer.extend_from_slice(payload);
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
        let mut q = Queue::new();
        q.push(9, &[1; 20], Instant::now()).unwrap();
        q.push(9, &[2; 12], Instant::now()).unwrap();
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
        let mut q = Queue::new();
        for _ in 0..MAX_FRAMES {
            q.push(3, &[9; 64], Instant::now()).unwrap();
        }
        assert!(q.push(3, &[7; 64], Instant::now()).is_err());
        assert_eq!(q.counts.overflow, 1);
        assert_eq!(q.payload_bytes(), (64 * MAX_FRAMES) as u64);
        let mut sink = Vec::new();
        while q.frames() > 0 {
            q.pump(&mut sink).unwrap();
        }
        assert_eq!(q.counts.frames, MAX_FRAMES as u64);
    }
}
