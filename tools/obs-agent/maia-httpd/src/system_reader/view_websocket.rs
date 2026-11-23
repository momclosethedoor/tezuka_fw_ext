//! Optional WebSocket sink for the managed VIEW transport benchmark.
use axum::{
    extract::ws::{Message, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    io,
    sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}},
    time::Duration,
};
use tokio::sync::mpsc;

const QUEUE_RECORD_BUDGET: usize = 128;
static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);

pub(super) struct Client {
    queue: mpsc::Sender<Bytes>,
    queue_limit: usize,
    peak: AtomicU64,
    written: AtomicU64,
    written_bytes: AtomicU64,
    writer_errors: AtomicU64,
}

pub(super) fn records_per_frame() -> usize {
    if std::env::var("OBS_DIAG_VIEW_WS_BATCH").as_deref() == Ok("8") { 8 } else { 1 }
}

pub(super) fn client() -> Option<Arc<Client>> {
    CLIENT.lock().unwrap().clone()
}

impl Client {
    pub(super) fn send(&self, record: &[u8]) -> io::Result<usize> {
        let slot = self.queue.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
            mpsc::error::TrySendError::Closed(_) => io::ErrorKind::BrokenPipe,
        })?;
        self.peak.fetch_max((self.queue_limit - self.queue.capacity()) as u64, Ordering::Relaxed);
        slot.send(Bytes::copy_from_slice(record));
        Ok(record.len())
    }

    pub(super) fn stats(&self) -> Value {
        json!({
            "queue_limit": self.queue_limit,
            "pending": self.queue_limit - self.queue.capacity(),
            "queue_peak": self.peak.load(Ordering::Relaxed),
            "written": self.written.load(Ordering::Relaxed),
            "written_bytes": self.written_bytes.load(Ordering::Relaxed),
            "writer_errors": self.writer_errors.load(Ordering::Relaxed),
        })
    }
}

pub(crate) async fn upgrade(ws: WebSocketUpgrade) -> Response {
    let mut active = CLIENT.lock().unwrap();
    if active.is_some() {
        return StatusCode::CONFLICT.into_response();
    }
    let queue_limit = QUEUE_RECORD_BUDGET / records_per_frame();
    let (queue, mut receiver) = mpsc::channel(queue_limit);
    let client = Arc::new(Client {
        queue, queue_limit, peak: AtomicU64::new(0), written: AtomicU64::new(0),
        written_bytes: AtomicU64::new(0), writer_errors: AtomicU64::new(0),
    });
    struct Lease;
    impl Drop for Lease {
        fn drop(&mut self) { *CLIENT.lock().unwrap() = None; }
    }
    let lease = Lease;
    *active = Some(client.clone());
    drop(active);
    ws.max_message_size(1024).on_upgrade(move |mut socket| async move {
        let _lease = lease;
        loop {
            tokio::select! {
                next = receiver.recv() => {
                    let Some(bytes) = next else { break };
                    let len = bytes.len();
                    match tokio::time::timeout(Duration::from_secs(2), socket.send(Message::Binary(bytes))).await {
                        Ok(Ok(())) => {
                            client.written.fetch_add(1, Ordering::Relaxed);
                            client.written_bytes.fetch_add(len as u64, Ordering::Relaxed);
                        }
                        _ => {
                            client.writer_errors.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                message = socket.recv() => {
                    if matches!(message, None | Some(Err(_) | Ok(Message::Close(_)))) { break; }
                }
            }
        }
    })
}
