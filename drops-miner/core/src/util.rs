//! Small async helpers the engine shares: an asyncio-style Event, exponential backoff, nonces and timestamps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use rand::Rng;
use tokio::sync::Notify;

/// A flag tasks can wait on, like Python's asyncio.Event: `set` wakes every waiter and stays set until `clear`.
#[derive(Default)]
pub struct Event {
    flag: AtomicBool,
    notify: Notify,
}

impl Event {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    /// Returns as soon as the flag is set (right away if it already is).
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
    /// Waits for the flag up to `timeout`. True if it was set.
    pub async fn wait_for(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.wait()).await.is_ok()
    }
}

/// Exponential backoff with ±10% jitter: 1, 2, 4, 8 … seconds, capped at `maximum`.
pub struct Backoff {
    pub steps: u32,
    maximum: f64,
}

impl Backoff {
    pub fn new(maximum_secs: f64) -> Self {
        Self {
            steps: 0,
            maximum: maximum_secs,
        }
    }
    pub fn reset(&mut self) {
        self.steps = 0;
    }
    pub fn next_delay(&mut self) -> Duration {
        let jitter: f64 = rand::thread_rng().gen_range(0.9..1.1);
        let value = 2f64.powi(self.steps as i32) * jitter;
        if value < self.maximum {
            self.steps += 1;
            Duration::from_secs_f64(value)
        } else {
            Duration::from_secs_f64(self.maximum)
        }
    }
}

pub const CHARS_ASCII: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
pub const CHARS_HEX_LOWER: &[u8] = b"0123456789abcdef";

pub fn nonce(chars: &[u8], len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| chars[rng.gen_range(0..chars.len())] as char)
        .collect()
}

/// Twitch timestamps: "2026-11-18T14:00:00Z" or with fractional seconds.
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ")
        .ok()
        .map(|n| n.and_utc())
}

/// Splits a list into chunks of `size` (GQL batches are limited to 20 operations at a time).
pub fn chunks<T: Clone>(items: &[T], size: usize) -> Vec<Vec<T>> {
    items.chunks(size).map(|c| c.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_with_and_without_fraction() {
        assert!(parse_ts("2026-11-18T14:00:00Z").is_some());
        assert!(parse_ts("2026-11-18T14:00:00.123Z").is_some());
        assert!(parse_ts("nope").is_none());
    }

    #[test]
    fn backoff_caps_at_maximum() {
        let mut b = Backoff::new(10.0);
        let delays: Vec<f64> = (0..8).map(|_| b.next_delay().as_secs_f64()).collect();
        assert!(delays[0] < 1.2);
        assert!(delays.iter().all(|d| *d <= 10.0));
        assert_eq!(*delays.last().unwrap(), 10.0);
    }

    #[tokio::test]
    async fn event_wakes_waiters_and_stays_set() {
        let e = std::sync::Arc::new(Event::new());
        let e2 = e.clone();
        let h = tokio::spawn(async move { e2.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        e.set();
        h.await.unwrap();
        assert!(e.wait_for(Duration::from_millis(5)).await);
        e.clear();
        assert!(!e.wait_for(Duration::from_millis(5)).await);
    }
}
