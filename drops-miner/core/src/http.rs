//! Talking to Twitch over HTTP: retries with backoff, the GQL rate limit and error handling (twitch.py request/gql_request).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::twitch::{CLIENT, GQL_URL};
use crate::util::{nonce, Backoff, CHARS_HEX_LOWER};

/// A response, already read in full.
pub struct Resp {
    pub status: u16,
    pub text: String,
}

impl Resp {
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.text)
            .map_err(|e| Error::Http(format!("unexpected answer from Twitch ({e})")))
    }
}

/// GQL is fragile: more than 5 requests a second gets everything rate-limited. Upstream keeps the same limit.
struct Limiter {
    sent: Mutex<VecDeque<Instant>>,
}

impl Limiter {
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut sent = self.sent.lock().unwrap();
                let now = Instant::now();
                while sent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
                {
                    sent.pop_front();
                }
                if sent.len() < 5 {
                    sent.push_back(now);
                    return;
                }
                Duration::from_secs(1).saturating_sub(now.duration_since(*sent.front().unwrap()))
            };
            tokio::time::sleep(wait + Duration::from_millis(5)).await;
        }
    }
}

#[derive(Default, Clone)]
struct Auth {
    device_id: String,
    token: String,
}

pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

pub struct Http {
    client: reqwest::Client,
    jar: Arc<Jar>,
    pub user_agent: String,
    session_id: String,
    auth: RwLock<Auth>,
    limiter: Limiter,
    log: LogFn,
    pub exiting: Arc<AtomicBool>,
}

impl Http {
    /// `quality` 1–6 stretches the timeouts for slow connections; `proxy` is optional.
    pub fn new(quality: u8, proxy: &str, log: LogFn, exiting: Arc<AtomicBool>) -> Result<Http> {
        let user_agent = {
            use rand::seq::SliceRandom;
            CLIENT
                .user_agents
                .choose(&mut rand::thread_rng())
                .copied()
                .unwrap_or_default()
                .to_string()
        };
        let q = quality.clamp(1, 6) as u64;
        let jar = Arc::new(Jar::default());
        let mut builder = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .cookie_provider(jar.clone())
            .gzip(true)
            .connect_timeout(Duration::from_secs(5 * q))
            .timeout(Duration::from_secs(10 * q))
            .pool_max_idle_per_host(10);
        if !proxy.trim().is_empty() {
            let p = reqwest::Proxy::all(proxy.trim())
                .map_err(|e| Error::Http(format!("the proxy address isn't valid ({e})")))?;
            builder = builder.proxy(p);
        }
        let client = builder.build()?;
        Ok(Http {
            client,
            jar,
            user_agent,
            session_id: nonce(CHARS_HEX_LOWER, 16),
            auth: RwLock::new(Auth::default()),
            limiter: Limiter {
                sent: Mutex::new(VecDeque::new()),
            },
            log,
            exiting,
        })
    }

    pub fn set_auth(&self, device_id: &str, token: &str) {
        let mut a = self.auth.write().unwrap();
        a.device_id = device_id.to_string();
        a.token = token.to_string();
    }

    pub fn token(&self) -> String {
        self.auth.read().unwrap().token.clone()
    }

    /// The headers every request to Twitch carries; `gql` adds the sign-in.
    pub fn headers(&self, gql: bool) -> HeaderMap {
        let auth = self.auth.read().unwrap().clone();
        let mut h = HeaderMap::new();
        let mut put = |k: &'static str, v: &str| {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(HeaderName::from_static(k), v);
            }
        };
        put("accept", "*/*");
        put("accept-language", "en-US");
        put("pragma", "no-cache");
        put("cache-control", "no-cache");
        put("client-id", CLIENT.client_id);
        put("client-session-id", &self.session_id);
        if !auth.device_id.is_empty() {
            put("x-device-id", &auth.device_id);
        }
        if gql {
            put("origin", CLIENT.url);
            put("referer", CLIENT.url);
            put("authorization", &format!("OAuth {}", auth.token));
        }
        h
    }

    /// Sends a request, retrying with backoff while Twitch is down (5xx) or unreachable.
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        headers: Option<HeaderMap>,
        json: Option<&Value>,
    ) -> Result<Resp> {
        let mut backoff = Backoff::new(180.0);
        loop {
            if self.exiting.load(Ordering::Relaxed) {
                return Err(Error::Exit);
            }
            let mut req = self.client.request(method.clone(), url);
            if let Some(h) = &headers {
                req = req.headers(h.clone());
            }
            if let Some(j) = json {
                req = req.json(j);
            }
            let delay = backoff.next_delay();
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status < 500 {
                        let text = resp.text().await.unwrap_or_default();
                        return Ok(Resp { status, text });
                    }
                    (self.log)(format!(
                        "Twitch seems to be having problems, retrying in {}s…",
                        delay.as_secs().max(1)
                    ));
                }
                Err(e) => {
                    if e.is_builder() {
                        return Err(e.into());
                    }
                    if backoff.steps > 1 {
                        let host = reqwest::Url::parse(url)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_string))
                            .unwrap_or_default();
                        (self.log)(format!(
                            "No connection to {host}, retrying in {}s…",
                            delay.as_secs().max(1)
                        ));
                    }
                }
            }
            tokio::time::sleep(delay).await;
        }
    }

    /// A GQL request: one operation (returns an object) or a batch (returns a list).
    pub async fn gql(&self, ops: Value) -> Result<Value> {
        let mut backoff = Backoff::new(60.0);
        let mut single_retry = true;
        loop {
            self.limiter.acquire().await;
            let resp = self
                .request(Method::POST, GQL_URL, Some(self.headers(true)), Some(&ops))
                .await?;
            if resp.status == 401 {
                return Err(Error::Auth(
                    "Twitch no longer accepts the saved sign-in".into(),
                ));
            }
            let mut body = resp.json()?;
            let mut retry = false;
            let mut retry_delay = backoff.next_delay();
            {
                let items: Vec<&mut Value> = match &mut body {
                    Value::Array(a) => a.iter_mut().collect(),
                    v => vec![v],
                };
                'outer: for item in items {
                    if let Some(errors) = item.get("errors").and_then(Value::as_array).cloned() {
                        for err in &errors {
                            let msg = err["message"].as_str().unwrap_or("");
                            if single_retry
                                && (msg == "service error" || msg == "PersistedQueryNotFound")
                            {
                                single_retry = false;
                                retry_delay = retry_delay.max(Duration::from_secs(5));
                                retry = true;
                                break 'outer;
                            } else if msg == "server error" {
                                // blank out the part of the data the error points at, keep the rest
                                if let Some(path) = err["path"].as_array() {
                                    let mut target = &mut item["data"];
                                    for key in &path[..path.len().saturating_sub(1)] {
                                        target = match key {
                                            Value::String(s) => &mut target[s.as_str()],
                                            Value::Number(n) => {
                                                &mut target[n.as_u64().unwrap_or(0) as usize]
                                            }
                                            _ => break,
                                        };
                                    }
                                    if let Some(last) = path.last() {
                                        match last {
                                            Value::String(s) => target[s.as_str()] = Value::Null,
                                            Value::Number(n) => {
                                                target[n.as_u64().unwrap_or(0) as usize] =
                                                    Value::Null
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                continue 'outer;
                            } else if matches!(
                                msg,
                                "service timeout"
                                    | "request cancelled"
                                    | "service unavailable"
                                    | "context deadline exceeded"
                            ) {
                                retry = true;
                                break 'outer;
                            }
                        }
                        return Err(Error::Gql(
                            serde_json::to_string(&errors).unwrap_or_default(),
                        ));
                    } else if let Some(e) = item.get("error") {
                        return Err(Error::Gql(format!("{}: {}", e, item["message"])));
                    }
                }
            }
            if !retry {
                return Ok(body);
            }
            tokio::time::sleep(retry_delay).await;
        }
    }

    /// Twitch hands out a device ID as the "unique_id" cookie on its home page.
    pub async fn fetch_device_id(&self) -> Result<String> {
        let mut h = self.headers(false);
        h.remove("authorization");
        self.request(Method::GET, CLIENT.url, Some(h), None).await?;
        let url = reqwest::Url::parse(CLIENT.url).unwrap();
        let cookies = self
            .jar
            .cookies(&url)
            .and_then(|v| v.to_str().ok().map(str::to_string))
            .unwrap_or_default();
        cookies
            .split(';')
            .filter_map(|c| c.trim().split_once('='))
            .find(|(k, _)| *k == "unique_id")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| Error::Http("Twitch didn't hand out a device ID".into()))
    }

    /// Checks a token. None if Twitch says it's no longer valid; otherwise Twitch's answer
    /// (client_id, login, user_id).
    pub async fn validate(&self, token: &str) -> Result<Option<Value>> {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("OAuth {token}"))
                .map_err(|_| Error::Auth("bad token".into()))?,
        );
        let resp = self
            .request(
                Method::GET,
                "https://id.twitch.tv/oauth2/validate",
                Some(h),
                None,
            )
            .await?;
        match resp.status {
            200 => Ok(Some(resp.json()?)),
            401 => Ok(None),
            s => Err(Error::Http(format!("sign-in check answered {s}"))),
        }
    }
}
