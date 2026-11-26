//! Plain TCP for the existing VIEW and sweep-completion records.
use std::{cell::RefCell, io::{self, Write}, net::{SocketAddrV4, TcpStream, UdpSocket},
          time::{Duration, Instant}};
use serde_json::{Value, json};

use super::{roi_stream::connect_nonblocking, view_batch::ViewOutput};

const CAPACITY: usize = 512 * 1024;
const RETRY: Duration = Duration::from_millis(250);
const STALL: Duration = Duration::from_secs(1);

pub(super) enum Output {
    Udp(UdpSocket),
    Tcp(Tcp),
}

impl ViewOutput for Output {
    fn udp(&self) -> Option<&UdpSocket> {
        match self { Self::Udp(socket) => Some(socket), Self::Tcp(_) => None }
    }

    fn send_frames(&self, payload: &[u8], frame_size: usize) -> io::Result<usize> {
        match self {
            Self::Udp(socket) => socket.send(payload),
            Self::Tcp(tcp) => tcp.enqueue(payload, frame_size),
        }
    }
}

impl Output {
    pub(super) fn pump(&self) {
        if let Self::Tcp(tcp) = self { tcp.pump(); }
    }

    pub(super) fn tcp_stats(&self) -> Value {
        match self { Self::Tcp(tcp) => tcp.stats(), Self::Udp(_) => Value::Null }
    }

    pub(super) fn finish(&self) {
        if let Self::Tcp(tcp) = self {
            let deadline = Instant::now() + Duration::from_millis(100);
            while tcp.inner.borrow().pending() > 0 && Instant::now() < deadline {
                tcp.pump();
                if tcp.inner.borrow().pending() > 0 { std::thread::sleep(Duration::from_millis(1)); }
            }
            tcp.inner.borrow_mut().disconnect();
        }
    }
}

pub(super) struct Tcp {
    peer: SocketAddrV4,
    inner: RefCell<State>,
}

struct State {
    socket: Option<TcpStream>,
    connecting: bool,
    retry: Instant,
    last_progress: Instant,
    bytes: Vec<u8>,
    offset: usize,
    peak: usize,
    connections: u64,
    disconnects: u64,
    write_calls: u64,
    written: u64,
    blocked: u64,
    rejected_records: u64,
    abandoned_bytes: u64,
}

impl State {
    fn pending(&self) -> usize { self.bytes.len() - self.offset }

    fn disconnect(&mut self) {
        self.disconnects += u64::from(self.socket.take().is_some());
        self.connecting = false;
        self.abandoned_bytes += self.pending() as u64;
        self.bytes.clear();
        self.offset = 0;
    }
}

impl Tcp {
    pub(super) fn new(peer: SocketAddrV4) -> Self {
        let now = Instant::now();
        Self { peer, inner: RefCell::new(State {
            socket: None, connecting: false, retry: now, last_progress: now,
            bytes: Vec::with_capacity(CAPACITY), offset: 0, peak: 0, connections: 0,
            disconnects: 0, write_calls: 0, written: 0, blocked: 0, rejected_records: 0,
            abandoned_bytes: 0,
        }) }
    }

    fn enqueue(&self, payload: &[u8], frame_size: usize) -> io::Result<usize> {
        let mut state = self.inner.borrow_mut();
        let count = payload.len() / frame_size;
        let need = payload.len() + count * 4;
        if state.socket.is_none() || state.connecting || state.pending() + need > CAPACITY {
            state.rejected_records += count as u64;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if state.pending() == 0 { state.last_progress = Instant::now(); }
        if state.bytes.len() + need > CAPACITY {
            let offset = state.offset;
            let remaining = state.pending();
            state.bytes.copy_within(offset.., 0);
            state.bytes.truncate(remaining);
            state.offset = 0;
        }
        let length = (frame_size as u32).to_le_bytes();
        for frame in payload.chunks_exact(frame_size) {
            state.bytes.extend_from_slice(&length);
            state.bytes.extend_from_slice(frame);
        }
        state.peak = state.peak.max(state.pending());
        Ok(payload.len())
    }

    fn pump(&self) {
        let mut state = self.inner.borrow_mut();
        let now = Instant::now();
        if state.socket.is_none() && now >= state.retry {
            state.retry = now + RETRY;
            if let Ok(socket) = connect_nonblocking(self.peer).and_then(|socket| {
                socket.set_nodelay(true)?;
                Ok(socket)
            }) {
                state.socket = Some(socket);
                state.connecting = true;
                state.last_progress = now;
            }
        }
        if state.connecting {
            let socket = state.socket.as_ref().unwrap();
            if !matches!(socket.take_error(), Ok(None)) || now.duration_since(state.last_progress) >= STALL {
                state.disconnect();
                return;
            }
            if socket.peer_addr().is_err() { return; }
            state.connecting = false;
            state.connections += 1;
        }
        if state.pending() == 0 { return; }
        if now.duration_since(state.last_progress) >= STALL {
            state.disconnect();
            return;
        }
        let Some(mut socket) = state.socket.as_ref() else { return; };
        let result = socket.write(&state.bytes[state.offset..]);
        state.write_calls += 1;
        match result {
            Ok(0) => state.disconnect(),
            Ok(n) => {
                state.last_progress = now;
                state.written += n as u64;
                state.offset += n;
                if state.pending() == 0 { state.offset = 0; state.bytes.clear(); }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => state.blocked += 1,
            Err(_) => state.disconnect(),
        }
    }

    fn stats(&self) -> Value {
        let state = self.inner.borrow();
        json!({"connected":state.socket.is_some() && !state.connecting, "connections":state.connections,
            "disconnects":state.disconnects,"write_calls":state.write_calls,"written_bytes":state.written,
            "blocked":state.blocked,"pending_bytes":state.pending(),"peak_bytes":state.peak,"capacity_bytes":CAPACITY,
            "rejected_records":state.rejected_records,"abandoned_bytes":state.abandoned_bytes})
    }
}
