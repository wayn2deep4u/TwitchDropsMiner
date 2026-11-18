//! Twitch PubSub: up to 8 websockets of 50 topics each, telling the miner about drop progress, claims and
//! streams going up or down (websocket.py).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::twitch::{
    MAX_WEBSOCKETS, PING_INTERVAL_SECS, PING_TIMEOUT_SECS, PUBSUB_URL, WS_TOPICS_LIMIT,
};
use crate::util::{nonce, Backoff, Event, CHARS_ASCII};

#[derive(Clone, Debug, Serialize)]
pub struct SocketStatus {
    pub index: usize,
    pub status: String,
    pub topics: usize,
}

struct Socket {
    index: usize,
    topics: Mutex<BTreeSet<String>>,
    topics_changed: Event,
    status: Mutex<String>,
    closed: AtomicBool,
    task: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    sockets: Mutex<Vec<Arc<Socket>>>,
    running: AtomicBool,
    token: RwLock<String>,
    tx: UnboundedSender<(String, Value)>,
    changed: Arc<dyn Fn() + Send + Sync>,
    log: Arc<dyn Fn(String) + Send + Sync>,
}

#[derive(Clone)]
pub struct PubSub {
    inner: Arc<Inner>,
}

impl PubSub {
    /// Messages arrive on `tx` as (topic, message). `changed` is called when a socket's status changes.
    pub fn new(
        tx: UnboundedSender<(String, Value)>,
        changed: Arc<dyn Fn() + Send + Sync>,
        log: Arc<dyn Fn(String) + Send + Sync>,
    ) -> PubSub {
        PubSub {
            inner: Arc::new(Inner {
                sockets: Mutex::new(Vec::new()),
                running: AtomicBool::new(false),
                token: RwLock::new(String::new()),
                tx,
                changed,
                log,
            }),
        }
    }

    pub fn set_token(&self, token: &str) {
        *self.inner.token.write().unwrap() = token.to_string();
    }

    pub fn start(&self) {
        self.inner.running.store(true, Ordering::SeqCst);
        for s in self.inner.sockets.lock().unwrap().iter() {
            spawn_socket(&self.inner, s.clone());
        }
    }

    pub fn stop(&self, clear_topics: bool) {
        self.inner.running.store(false, Ordering::SeqCst);
        {
            let mut sockets = self.inner.sockets.lock().unwrap();
            for s in sockets.iter() {
                s.closed.store(true, Ordering::SeqCst);
                if let Some(t) = s.task.lock().unwrap().take() {
                    t.abort();
                }
                *s.status.lock().unwrap() = "Disconnected".into();
            }
            if clear_topics {
                sockets.clear();
            }
        }
        (self.inner.changed)();
    }

    pub fn statuses(&self) -> Vec<SocketStatus> {
        self.inner
            .sockets
            .lock()
            .unwrap()
            .iter()
            .map(|s| SocketStatus {
                index: s.index,
                status: s.status.lock().unwrap().clone(),
                topics: s.topics.lock().unwrap().len(),
            })
            .collect()
    }

    pub fn add_topics(&self, topics: Vec<String>) {
        let mut todo: BTreeSet<String> = topics.into_iter().collect();
        let mut sockets = self.inner.sockets.lock().unwrap();
        for s in sockets.iter() {
            let t = s.topics.lock().unwrap();
            todo.retain(|x| !t.contains(x));
        }
        if todo.is_empty() {
            return;
        }
        for idx in 0..MAX_WEBSOCKETS {
            if idx >= sockets.len() {
                let s = Arc::new(Socket {
                    index: idx,
                    topics: Mutex::new(BTreeSet::new()),
                    topics_changed: Event::new(),
                    status: Mutex::new("Disconnected".into()),
                    closed: AtomicBool::new(false),
                    task: Mutex::new(None),
                });
                if self.inner.running.load(Ordering::SeqCst) {
                    spawn_socket(&self.inner, s.clone());
                }
                sockets.push(s);
            }
            let s = &sockets[idx];
            let mut t = s.topics.lock().unwrap();
            let mut added = false;
            while t.len() < WS_TOPICS_LIMIT {
                let Some(next) = todo.pop_first() else { break };
                t.insert(next);
                added = true;
            }
            drop(t);
            if added {
                s.topics_changed.set();
            }
            if todo.is_empty() {
                break;
            }
        }
        drop(sockets);
        if !todo.is_empty() {
            (self.inner.log)(format!(
                "Too many channels to follow: {} left out",
                todo.len()
            ));
        }
        (self.inner.changed)();
    }

    pub fn remove_topics(&self, topics: Vec<String>) {
        let remove: BTreeSet<String> = topics.into_iter().collect();
        if remove.is_empty() {
            return;
        }
        let mut recycled = Vec::new();
        {
            let mut sockets = self.inner.sockets.lock().unwrap();
            for s in sockets.iter() {
                let mut t = s.topics.lock().unwrap();
                let before = t.len();
                t.retain(|x| !remove.contains(x));
                if t.len() != before {
                    s.topics_changed.set();
                }
            }
            // more sockets than the topics need: retire the last one and hand its topics to the others
            loop {
                let count: usize = sockets.iter().map(|s| s.topics.lock().unwrap().len()).sum();
                if !sockets.is_empty() && count <= (sockets.len() - 1) * WS_TOPICS_LIMIT {
                    let s = sockets.pop().unwrap();
                    s.closed.store(true, Ordering::SeqCst);
                    if let Some(t) = s.task.lock().unwrap().take() {
                        t.abort();
                    }
                    recycled.extend(s.topics.lock().unwrap().iter().cloned());
                } else {
                    break;
                }
            }
        }
        if !recycled.is_empty() {
            self.add_topics(recycled);
        }
        (self.inner.changed)();
    }
}

fn spawn_socket(inner: &Arc<Inner>, s: Arc<Socket>) {
    let mut task = s.task.lock().unwrap();
    if task.as_ref().is_some_and(|t| !t.is_finished()) {
        return;
    }
    s.closed.store(false, Ordering::SeqCst);
    let inner = inner.clone();
    let s2 = s.clone();
    *task = Some(tokio::spawn(async move { run_socket(inner, s2).await }));
}

fn set_status(inner: &Inner, s: &Socket, status: &str) {
    *s.status.lock().unwrap() = status.to_string();
    (inner.changed)();
}

async fn run_socket(inner: Arc<Inner>, s: Arc<Socket>) {
    let mut backoff = Backoff::new(180.0);
    loop {
        if s.closed.load(Ordering::SeqCst) {
            return;
        }
        set_status(&inner, &s, "Connecting");
        let ws = match tokio_tungstenite::connect_async(PUBSUB_URL).await {
            Ok((ws, _)) => ws,
            Err(_) => {
                let d = backoff.next_delay();
                set_status(&inner, &s, "Reconnecting");
                tokio::time::sleep(d).await;
                continue;
            }
        };
        backoff.reset();
        set_status(&inner, &s, "Connected");
        let (mut write, mut read) = ws.split();
        let mut submitted: BTreeSet<String> = BTreeSet::new();
        s.topics_changed.set();
        let mut next_ping = Instant::now();
        let mut max_pong = next_ping + Duration::from_secs(PING_TIMEOUT_SECS);
        let result: Result<(), ()> = async {
            loop {
                if s.closed.load(Ordering::SeqCst) {
                    let _ = write.close().await;
                    return Ok(());
                }
                // keep-alive
                let now = Instant::now();
                if now >= next_ping {
                    next_ping = now + Duration::from_secs(PING_INTERVAL_SECS);
                    max_pong = now + Duration::from_secs(PING_TIMEOUT_SECS);
                    write.send(Message::Text(json!({"type": "PING"}).to_string())).await.map_err(|_| ())?;
                } else if now >= max_pong {
                    (inner.log)(format!("Websocket {} didn't answer, reconnecting", s.index + 1));
                    return Err(());
                }
                // topics
                if s.topics_changed.is_set() {
                    s.topics_changed.clear();
                    let current = s.topics.lock().unwrap().clone();
                    let token = inner.token.read().unwrap().clone();
                    let removed: Vec<String> = submitted.difference(&current).cloned().collect();
                    for chunk in removed.chunks(20) {
                        let msg = json!({"type": "UNLISTEN", "nonce": nonce(CHARS_ASCII, 30), "data": {"topics": chunk, "auth_token": token}});
                        write.send(Message::Text(msg.to_string())).await.map_err(|_| ())?;
                    }
                    for r in removed {
                        submitted.remove(&r);
                    }
                    let added: Vec<String> = current.difference(&submitted).cloned().collect();
                    for chunk in added.chunks(20) {
                        let msg = json!({"type": "LISTEN", "nonce": nonce(CHARS_ASCII, 30), "data": {"topics": chunk, "auth_token": token}});
                        write.send(Message::Text(msg.to_string())).await.map_err(|_| ())?;
                    }
                    submitted.extend(added);
                    (inner.changed)();
                }
                // receive for up to half a second
                match tokio::time::timeout(Duration::from_millis(500), read.next()).await {
                    Err(_) => {}
                    Ok(None) | Ok(Some(Err(_))) => return Err(()),
                    Ok(Some(Ok(Message::Close(_)))) => return Err(()),
                    Ok(Some(Ok(Message::Text(text)))) => {
                        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                        match msg["type"].as_str().unwrap_or("") {
                            "MESSAGE" => {
                                let topic = msg["data"]["topic"].as_str().unwrap_or("").to_string();
                                if s.topics.lock().unwrap().contains(&topic) {
                                    if let Some(body) = msg["data"]["message"].as_str().and_then(|m| serde_json::from_str::<Value>(m).ok()) {
                                        let _ = inner.tx.send((topic, body));
                                    }
                                }
                            }
                            "PONG" => max_pong = next_ping,
                            "RECONNECT" => {
                                (inner.log)(format!("Twitch asked websocket {} to reconnect", s.index + 1));
                                return Err(());
                            }
                            "RESPONSE" => {
                                if let Some(err) = msg["error"].as_str().filter(|e| !e.is_empty()) {
                                    (inner.log)(format!("Websocket {}: {err}", s.index + 1));
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                }
            }
        }
        .await;
        if result.is_ok() || s.closed.load(Ordering::SeqCst) {
            set_status(&inner, &s, "Disconnected");
            return;
        }
        set_status(&inner, &s, "Reconnecting");
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn topics_fill_sockets_and_recycle() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let p = PubSub::new(tx, Arc::new(|| {}), Arc::new(|_| {}));
        let topics: Vec<String> = (0..120).map(|i| format!("t.{i}")).collect();
        p.add_topics(topics.clone());
        let st = p.statuses();
        assert_eq!(st.len(), 3);
        assert_eq!(
            st.iter().map(|s| s.topics).collect::<Vec<_>>(),
            vec![50, 50, 20]
        );
        // dropping 60 topics leaves 60: two sockets are enough
        p.remove_topics(topics[..60].to_vec());
        let st = p.statuses();
        assert_eq!(st.len(), 2);
        assert_eq!(st.iter().map(|s| s.topics).sum::<usize>(), 60);
    }
}
