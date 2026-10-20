//! A minimal Chrome DevTools Protocol client over one WebSocket
//! (tokio-tungstenite): JSON-RPC commands correlated by `id`, flattened
//! sessions (`sessionId` on commands and events) and an event stream.
//!
//! Two tasks own the socket: a writer draining an mpsc of outgoing frames
//! (it closes the socket once every [`Connection`] handle is gone) and a
//! reader that completes pending calls and forwards the events the
//! [`EventFilter`] lets through. When the socket closes, every pending and
//! later call fails with [`CdpError::Closed`] and the event stream ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Which events reach the [`Events`] stream (`method`, `params`).
pub type EventFilter = fn(&str, &Value) -> bool;

/// Largest frame accepted from the browser (screenshots arrive base64-encoded).
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CdpError {
    /// The browser answered the command with an error.
    #[error("{message} (code {code})")]
    Protocol { code: i64, message: String },
    /// The connection is gone.
    #[error("the connection to the browser closed")]
    Closed,
}

/// A protocol event. `session_id` is the target session it belongs to
/// (`None` for browser-level events).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// One decoded incoming frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response { id: u64, result: Result<Value, CdpError> },
    Event(Event),
    /// Not a CDP message (ignored).
    Other,
}

/// The JSON text of a command.
pub fn encode(id: u64, method: &str, params: Value, session_id: Option<&str>) -> String {
    let mut msg = Map::new();
    msg.insert("id".into(), json!(id));
    msg.insert("method".into(), json!(method));
    if !params.is_null() {
        msg.insert("params".into(), params);
    }
    if let Some(s) = session_id {
        msg.insert("sessionId".into(), json!(s));
    }
    Value::Object(msg).to_string()
}

/// Decodes a frame from the browser.
pub fn decode(text: &str) -> Incoming {
    let Ok(Value::Object(mut msg)) = serde_json::from_str::<Value>(text) else {
        return Incoming::Other;
    };
    let session_id = msg.get("sessionId").and_then(Value::as_str).map(str::to_string);
    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let result = match msg.remove("error") {
            Some(err) => Err(CdpError::Protocol {
                code: err.get("code").and_then(Value::as_i64).unwrap_or_default(),
                message: err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
            }),
            None => Ok(msg.remove("result").unwrap_or(Value::Object(Map::new()))),
        };
        return Incoming::Response { id, result };
    }
    match msg.remove("method") {
        Some(Value::String(method)) => Incoming::Event(Event {
            method,
            params: msg.remove("params").unwrap_or(Value::Null),
            session_id,
        }),
        _ => Incoming::Other,
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>;

struct Inner {
    out: mpsc::UnboundedSender<String>,
    pending: Arc<Pending>,
    /// Set by the reader before it fails the pending calls.
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
}

/// A handle on the browser connection (cheap to clone). The socket closes
/// when the last handle is dropped.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

/// Events the filter let through, in arrival order. Ends when the
/// connection closes.
pub struct Events(mpsc::UnboundedReceiver<Event>);

impl Events {
    pub async fn next(&mut self) -> Option<Event> {
        self.0.recv().await
    }

    /// An event that already arrived, without waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.0.try_recv().ok()
    }
}

/// Removes a call's pending entry when the call is dropped (e.g. timed out).
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

impl Connection {
    /// Starts the reader and writer tasks for an open WebSocket.
    pub fn start<S>(ws: WebSocketStream<S>, filter: EventFilter) -> (Connection, Events)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let pending: Arc<Pending> = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = out_rx.recv() => match frame {
                        Some(text) => {
                            if sink.send(Message::text(text)).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            // Every handle is gone: say goodbye.
                            let _ = sink.send(Message::Close(None)).await;
                            break;
                        }
                    },
                    _ = &mut stop_rx => break,
                }
            }
            let _ = sink.close().await;
        });

        let reader_pending = pending.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                match decode(&text) {
                    Incoming::Response { id, result } => {
                        let waiter = reader_pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                        if let Some(tx) = waiter {
                            let _ = tx.send(result);
                        }
                    }
                    Incoming::Event(event) => {
                        if filter(&event.method, &event.params) {
                            let _ = event_tx.send(event);
                        }
                    }
                    Incoming::Other => {}
                }
            }
            // Closed: fail whatever still waits (dropping the senders does that).
            reader_closed.store(true, Ordering::SeqCst);
            reader_pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
            let _ = stop_tx.send(());
        });

        let inner = Inner { out: out_tx, pending, closed, next_id: AtomicU64::new(1) };
        (Connection { inner: Arc::new(inner) }, Events(event_rx))
    }

    /// Sends a command and waits for its answer. Callers bound the wait
    /// (`tokio::time::timeout`); a dropped call forgets its pending entry.
    pub async fn call(&self, method: &str, params: Value, session_id: Option<&str>) -> Result<Value, CdpError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        let _guard = PendingGuard { pending: &self.inner.pending, id };
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(CdpError::Closed);
        }
        if self.inner.out.send(encode(id, method, params, session_id)).is_err() {
            return Err(CdpError::Closed);
        }
        rx.await.unwrap_or(Err(CdpError::Closed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_encoded_with_id_method_params_and_session() {
        let text = encode(7, "Page.navigate", json!({"url": "https://example.com/?q=\"x\""}), Some("S1"));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v, json!({"id": 7, "method": "Page.navigate", "params": {"url": "https://example.com/?q=\"x\""}, "sessionId": "S1"}));
        let bare: Value = serde_json::from_str(&encode(1, "Browser.getVersion", Value::Null, None)).unwrap();
        assert_eq!(bare, json!({"id": 1, "method": "Browser.getVersion"}));
    }

    #[test]
    fn responses_errors_and_events_are_decoded() {
        assert_eq!(
            decode(r#"{"id":3,"result":{"frameId":"F","loaderId":"L"}}"#),
            Incoming::Response { id: 3, result: Ok(json!({"frameId": "F", "loaderId": "L"})) }
        );
        assert_eq!(decode(r#"{"id":4,"sessionId":"S"}"#), Incoming::Response { id: 4, result: Ok(json!({})) });
        assert_eq!(
            decode(r#"{"id":5,"error":{"code":-32000,"message":"Cannot navigate to invalid URL"}}"#),
            Incoming::Response { id: 5, result: Err(CdpError::Protocol { code: -32000, message: "Cannot navigate to invalid URL".into() }) }
        );
        assert_eq!(
            decode(r#"{"method":"Page.lifecycleEvent","params":{"name":"load"},"sessionId":"S"}"#),
            Incoming::Event(Event { method: "Page.lifecycleEvent".into(), params: json!({"name": "load"}), session_id: Some("S".into()) })
        );
        assert_eq!(decode("not json"), Incoming::Other);
        assert_eq!(decode(r#"{"hello":1}"#), Incoming::Other);
        assert_eq!(decode("[1,2]"), Incoming::Other);
    }

    /// A WebSocket pair over an in-memory duplex pipe.
    async fn pair() -> (WebSocketStream<tokio::io::DuplexStream>, WebSocketStream<tokio::io::DuplexStream>) {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (a, b) = tokio::io::duplex(1 << 16);
        let client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        (client, server)
    }

    #[tokio::test]
    async fn answers_are_matched_to_calls_by_id_even_out_of_order() {
        let (client, mut server) = pair().await;
        let (conn, mut events) = Connection::start(client, |m, _| m != "Noise.event");
        let browser = tokio::spawn(async move {
            let mut ids = Vec::new();
            while ids.len() < 2 {
                if let Some(Ok(Message::Text(t))) = server.next().await {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    ids.push((v["id"].as_u64().unwrap(), v["method"].as_str().unwrap().to_string()));
                }
            }
            server.send(Message::text(r#"{"method":"Noise.event","params":{}}"#)).await.unwrap();
            server.send(Message::text(r#"{"method":"Page.loadEventFired","params":{"timestamp":1}}"#)).await.unwrap();
            // Answer the second call first.
            for (id, method) in ids.iter().rev() {
                let reply = if method == "Fails" {
                    json!({"id": id, "error": {"code": -1, "message": "nope"}})
                } else {
                    json!({"id": id, "result": {"echo": method}})
                };
                server.send(Message::text(reply.to_string())).await.unwrap();
            }
            server
        });
        let (a, b) = tokio::join!(conn.call("Works", json!({}), None), conn.call("Fails", json!({}), Some("S")));
        assert_eq!(a.unwrap(), json!({"echo": "Works"}));
        assert_eq!(b.unwrap_err(), CdpError::Protocol { code: -1, message: "nope".into() });
        let event = events.next().await.unwrap();
        assert_eq!(event.method, "Page.loadEventFired", "filtered events are dropped");
        // The browser goes away: calls fail instead of hanging, events end.
        drop(browser.await.unwrap());
        assert_eq!(conn.call("Late", json!({}), None).await.unwrap_err(), CdpError::Closed);
        assert!(events.next().await.is_none());
    }
}
