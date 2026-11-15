use crate::app::AppState;
use axum::{extract::{State,ws::{Message,WebSocketUpgrade}},response::{Response,IntoResponse}};
use std::{sync::atomic::Ordering,time::Duration};

pub async fn status(State(state):State<AppState>)->Response {
    let value=state.scan().status.lock().unwrap().clone();
    ([(axum::http::header::CONTENT_TYPE,"application/json")],if value.is_empty(){"{\"version\":1,\"kind\":\"resident-status\",\"running\":false}".into()}else{value}).into_response()
}
pub async fn stop(State(state):State<AppState>)->Response {state.scan().stop.store(true,Ordering::Relaxed);(axum::http::StatusCode::ACCEPTED,"stop queued; poll resident status").into_response()}
pub async fn handler(State(state):State<AppState>,ws:WebSocketUpgrade)->Response {
    if !state.native_observations()&&state.parts().is_none(){return (axum::http::StatusCode::CONFLICT,"prepared source required").into_response();}
    if state.scan().clients.fetch_add(1,Ordering::AcqRel)>=2{state.scan().clients.fetch_sub(1,Ordering::AcqRel);return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();}
    struct Lease(AppState);impl Drop for Lease{fn drop(&mut self){self.0.scan().clients.fetch_sub(1,Ordering::AcqRel);}}
    let lease=Lease(state.clone());
    ws.on_upgrade(move |mut socket|async move {
        let _lease=lease;let mut ordinal=0;let mut pending=Vec::with_capacity(112+64*8+8*32768);
        let mut cursor=crate::continuous::Cursor::default();let parts=state.parts();
        loop {
            let ready=if let Some(parts)=&parts{
                let missed=cursor.missed;
                let ready=parts.lock().unwrap().poll(&mut cursor,&mut pending);
                state.scan().missed.fetch_add(cursor.missed-missed,Ordering::Relaxed);ready
            }else{state.scan().poll(&mut ordinal,&mut pending)};
            if ready{
                // At most one in-flight message/client. No acquisition lock is
                // held across send, and slow readers lose whole older frames.
                let bytes=bytes::Bytes::copy_from_slice(&pending);
                match tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Binary(bytes))).await {Ok(Ok(()))=>{state.scan().sent.fetch_add(1,Ordering::Relaxed);},_=>break}
            }
            tokio::select!{_=tokio::time::sleep(Duration::from_millis(5))=>{},message=socket.recv()=>{match message{None|Some(Err(_))|Some(Ok(Message::Close(_)))=>break,_=>{}}}}
        }
    })
}
