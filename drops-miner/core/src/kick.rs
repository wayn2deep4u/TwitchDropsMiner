//! Kick drops: campaigns, progress, claiming and watching on kick.com.
//!
//! Kick's firewall only lets real browsers through, so this module never talks to Kick itself. Every request and the
//! viewer websocket go through a [`KickBridge`], which the app implements with a hidden browser window on kick.com.
//! This module decides what to mine, on which channel, and when to claim. The API details follow the open-source Kick
//! miners that are known to work (kick-watcher, lurkloot, Twitch-Kick-Drops-Miner): if Kick changes something, the
//! URLs and field names are all in this file.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::engine::{BenefitView, CampaignView, ChannelView, DropItemView, DropView, WatchingView};
use crate::settings::{PriorityMode, Settings};
use crate::util::{parse_ts, Event};

// ---------------------------------------------------------------------------------------------------------------
// Kick's API

/// Kick's web client token; Kick's firewall blocks the drops and viewer-token endpoints without it.
pub const CLIENT_TOKEN: &str = "e1393935a959b4020a4491574f6490129f678acdaa92760471263db43487f823";
pub const USER_URL: &str = "https://kick.com/api/v1/user";
pub const CAMPAIGNS_URL: &str = "https://web.kick.com/api/v1/drops/campaigns";
pub const PROGRESS_URL: &str = "https://web.kick.com/api/v1/drops/progress";
pub const CLAIM_URL: &str = "https://web.kick.com/api/v1/drops/claim";
pub const VIEWER_TOKEN_URL: &str = "https://websockets.kick.com/viewer/v1/token";
pub const ASSET_BASE: &str = "https://ext.kick.com";

pub fn channel_url(slug: &str) -> String {
    format!("https://kick.com/api/v2/channels/{slug}")
}
pub fn livestreams_url(category_id: u64) -> String {
    format!("https://web.kick.com/api/v1/livestreams?limit=25&sort=viewer_count_desc&category_id={category_id}")
}

/// Viewer websocket messages (sent by the bridge on the open socket).
pub fn ws_handshake(channel_id: u64) -> String {
    json!({ "type": "channel_handshake", "data": { "message": { "channelId": channel_id } } })
        .to_string()
}
pub fn ws_ping() -> String {
    json!({ "type": "ping" }).to_string()
}
pub fn ws_watch_event(channel_id: u64, livestream_id: u64) -> String {
    json!({ "type": "user_event", "data": { "message": {
        "name": "tracking.user.watch.livestream", "channel_id": channel_id, "livestream_id": livestream_id } } })
    .to_string()
}

const HANDSHAKE_EVERY: Duration = Duration::from_secs(12);
const WATCH_EVENT_EVERY: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_secs(5 * 60);
const LIVE_CHECK_EVERY: Duration = Duration::from_secs(3 * 60);
/// No progress for this long while watching: try another channel.
const STALL_AFTER: Duration = Duration::from_secs(20 * 60);
/// A channel that stalled or failed is skipped for this long.
const COOLDOWN: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------------------------------------------------------
// The bridge to the Kick browser window

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A response from Kick, read in full.
#[derive(Clone, Debug)]
pub struct KickResp {
    pub status: u16,
    pub body: String,
}

impl KickResp {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// What the app gives the Kick engine: requests and a viewer websocket, both made by a real browser on kick.com.
pub trait KickBridge: Send + Sync + 'static {
    /// True while Kick can't be reached for now (the sign-in window is open); the engine waits instead of asking.
    fn paused(&self) -> bool {
        false
    }
    /// Sends a request from the Kick window (with its cookies and sign-in). `client_token` adds X-Client-Token.
    fn fetch(
        &self,
        method: &'static str,
        url: String,
        body: Option<String>,
        client_token: bool,
    ) -> BoxFut<'_, Result<KickResp, String>>;
    /// Opens the viewer websocket with this token (closing any open one).
    fn ws_open(&self, token: String) -> BoxFut<'_, Result<(), String>>;
    /// Sends a message on the websocket. Returns the socket's state afterwards: "open", "connecting" or "closed".
    fn ws_send(&self, message: String) -> BoxFut<'_, Result<String, String>>;
    fn ws_close(&self);
}

/// How the Kick engine reaches the window.
pub trait KickSink: Send + Sync + 'static {
    fn changed(&self);
    fn log(&self, text: String);
    fn notify(&self, title: &str, body: &str);
    /// The Kick sign-in worked; the sign-in window can close.
    fn signed_in(&self, username: &str);
}

// ---------------------------------------------------------------------------------------------------------------
// Campaigns

#[derive(Clone, Debug, PartialEq)]
pub struct KickReward {
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub required_minutes: i64,
    pub claimed: bool,
    /// Kick's own fraction for this reward, when it sends one.
    pub progress: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct KickCampaign {
    pub id: String,
    pub name: String,
    pub game: String,
    pub category_id: Option<u64>,
    pub image_url: String,
    pub status: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub connect_url: String,
    /// False when Kick says the game account isn't linked (rewards can't be claimed until it is).
    pub linked: bool,
    /// Allowed channels (slugs); empty means any channel streaming the game.
    pub channels: Vec<String>,
    /// Minutes watched for this campaign (all rewards progress together).
    pub minutes: i64,
    pub rewards: Vec<KickReward>,
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
fn u(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(x) => x.parse().ok(),
        _ => None,
    }
}
fn i(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(x) => x.parse().unwrap_or(0),
        _ => 0,
    }
}
fn first<'a>(v: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter()
        .map(|k| &v[*k])
        .find(|x| !x.is_null())
        .unwrap_or(&Value::Null)
}
pub fn asset(url: &str) -> String {
    if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("{ASSET_BASE}/{}", url.trim_start_matches('/'))
    }
}

/// Kick wraps lists differently from one endpoint (and month) to the next: a bare list, {data: [...]}, or
/// {data: {campaigns: [...]}}. This finds the list.
fn list_of<'a>(v: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    if let Some(a) = v.as_array() {
        return a.iter().collect();
    }
    if let Some(a) = v["data"].as_array() {
        return a.iter().collect();
    }
    for k in keys {
        if let Some(a) = v[*k].as_array() {
            return a.iter().collect();
        }
        if let Some(a) = v["data"][*k].as_array() {
            return a.iter().collect();
        }
    }
    Vec::new()
}

/// Parses /drops/campaigns.
pub fn parse_campaigns(v: &Value) -> Vec<KickCampaign> {
    list_of(v, &["campaigns", "active", "current"])
        .into_iter()
        .filter_map(|c| {
            let id = s(&c["id"]);
            if id.is_empty() {
                return None;
            }
            let starts_at = first(c, &["starts_at", "startsAt", "start_date"])
                .as_str()
                .and_then(parse_ts);
            let ends_at = first(c, &["ends_at", "endsAt", "end_date"])
                .as_str()
                .and_then(parse_ts);
            let rewards = first(c, &["rewards", "drops"])
                .as_array()
                .cloned()
                .unwrap_or_default();
            Some(KickCampaign {
                name: s(first(c, &["name", "title"])),
                game: s(&c["category"]["name"]),
                category_id: u(first(c, &["category_id", "game_id"]))
                    .or_else(|| u(&c["category"]["id"])),
                image_url: asset(c["category"]["image_url"].as_str().unwrap_or("")),
                status: c["status"].as_str().unwrap_or("").to_lowercase(),
                connect_url: c["connect_url"].as_str().unwrap_or("").to_string(),
                linked: true,
                channels: c["channels"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|ch| {
                                let slug = s(first(ch, &["slug", "username", "user_slug"]));
                                let slug = if slug.is_empty() {
                                    s(first(&ch["user"], &["slug", "username"]))
                                } else {
                                    slug
                                };
                                (!slug.is_empty()).then(|| slug.to_lowercase())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                minutes: 0,
                rewards: rewards
                    .iter()
                    .map(|r| {
                        let units = first(
                            r,
                            &["required_units", "required_minutes", "minutes_required"],
                        );
                        KickReward {
                            id: s(&r["id"]),
                            name: s(first(r, &["name", "title"])),
                            image_url: asset(
                                first(r, &["image_url", "image"]).as_str().unwrap_or(""),
                            ),
                            required_minutes: i(units),
                            claimed: r["claimed"]
                                .as_bool()
                                .or(r["is_claimed"].as_bool())
                                .unwrap_or(false),
                            progress: None,
                        }
                    })
                    .collect(),
                id,
                starts_at,
                ends_at,
            })
        })
        .collect()
}

/// Folds /drops/progress into the campaigns: minutes watched, claimed rewards and account linking.
pub fn merge_progress(campaigns: &mut [KickCampaign], v: &Value) {
    let items = list_of(v, &["progress", "campaigns", "active", "current"]);
    for c in campaigns.iter_mut() {
        let Some(p) = items
            .iter()
            .find(|p| s(first(p, &["id", "campaign_id"])) == c.id)
        else {
            continue;
        };
        if !p["progress_units"].is_null() {
            c.minutes = i(&p["progress_units"]);
        }
        match p["user_app_connected"].as_bool() {
            Some(false) => {
                c.linked =
                    c.connect_url.is_empty() && p["connect_url"].as_str().unwrap_or("").is_empty()
            }
            Some(true) => c.linked = true,
            None => {}
        }
        if let Some(url) = p["connect_url"].as_str().filter(|x| !x.is_empty()) {
            c.connect_url = url.to_string();
        }
        if c.game.is_empty() {
            c.game = s(&p["category"]["name"]);
        }
        for rp in p["rewards"].as_array().into_iter().flatten() {
            let rid = s(first(rp, &["id", "reward_id"]));
            if let Some(r) = c.rewards.iter_mut().find(|r| r.id == rid) {
                if rp["claimed"]
                    .as_bool()
                    .or(rp["is_claimed"].as_bool())
                    .unwrap_or(false)
                {
                    r.claimed = true;
                }
                r.progress = rp["progress"].as_f64();
                if r.required_minutes == 0 {
                    r.required_minutes = i(&rp["required_units"]);
                }
            }
        }
    }
}

impl KickCampaign {
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        !matches!(
            self.status.as_str(),
            "expired" | "ended" | "completed" | "upcoming" | "scheduled"
        ) && self.starts_at.is_none_or(|t| t <= now)
            && self.ends_at.is_none_or(|t| now < t)
    }
    pub fn upcoming(&self, now: DateTime<Utc>) -> bool {
        matches!(self.status.as_str(), "upcoming" | "scheduled")
            || self.starts_at.is_some_and(|t| now < t)
    }
    pub fn finished(&self) -> bool {
        self.rewards
            .iter()
            .all(|r| r.claimed || r.required_minutes <= 0)
    }
    /// Minutes watched toward a reward.
    pub fn reward_minutes(&self, r: &KickReward) -> i64 {
        if r.claimed {
            return r.required_minutes;
        }
        if self.minutes > 0 {
            return self.minutes.min(r.required_minutes);
        }
        r.progress
            .map(|p| (p * r.required_minutes as f64).round() as i64)
            .unwrap_or(0)
            .min(r.required_minutes)
    }
    pub fn reward_progress(&self, r: &KickReward) -> f64 {
        if r.claimed {
            1.0
        } else if r.required_minutes <= 0 {
            0.0
        } else {
            (self.reward_minutes(r) as f64 / r.required_minutes as f64).clamp(0.0, 1.0)
        }
    }
    pub fn claimable(&self, r: &KickReward) -> bool {
        !r.claimed
            && r.required_minutes > 0
            && self.linked
            && self.reward_minutes(r) >= r.required_minutes
    }
    /// The reward being worked toward: the unclaimed one needing the fewest minutes.
    pub fn next_reward(&self) -> Option<&KickReward> {
        self.rewards
            .iter()
            .filter(|r| !r.claimed && r.required_minutes > 0)
            .min_by_key(|r| r.required_minutes)
    }
    pub fn remaining_minutes(&self) -> i64 {
        self.rewards
            .iter()
            .filter(|r| !r.claimed)
            .map(|r| r.required_minutes - self.reward_minutes(r))
            .max()
            .unwrap_or(0)
            .max(0)
    }
    pub fn claimed_count(&self) -> usize {
        self.rewards.iter().filter(|r| r.claimed).count()
    }
    pub fn progress(&self) -> f64 {
        if self.rewards.is_empty() {
            return 0.0;
        }
        self.rewards
            .iter()
            .map(|r| self.reward_progress(r))
            .sum::<f64>()
            / self.rewards.len() as f64
    }
    /// Worth watching for: active, linked, something left to earn.
    pub fn minable(&self, now: DateTime<Utc>) -> bool {
        self.active(now) && self.linked && !self.finished() && self.next_reward().is_some()
    }
}

/// The campaigns to mine, best first, by the same game lists and priority mode as Twitch.
pub fn pick_order(
    campaigns: &[KickCampaign],
    settings: &Settings,
    now: DateTime<Utc>,
) -> Vec<usize> {
    let rank = |game: &str| {
        settings
            .priority
            .iter()
            .position(|p| p.eq_ignore_ascii_case(game))
            .unwrap_or(usize::MAX)
    };
    let mut order: Vec<usize> = (0..campaigns.len())
        .filter(|i| {
            let c = &campaigns[*i];
            c.minable(now)
                && !settings
                    .exclude
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(&c.game))
                && (settings.priority_mode != PriorityMode::PriorityOnly
                    || rank(&c.game) != usize::MAX)
        })
        .collect();
    let ends = |i: &usize| campaigns[*i].ends_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    match settings.priority_mode {
        PriorityMode::LowAvailability => order.sort_by(|a, b| {
            let spare = |i: &usize| {
                let c = &campaigns[*i];
                let left = (ends(i) - now).num_minutes().max(0) as f64;
                left / c.remaining_minutes().max(1) as f64
            };
            spare(a)
                .partial_cmp(&spare(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => order.sort_by_key(ends),
    }
    order.sort_by_key(|i| rank(&campaigns[*i].game));
    order
}

// ---------------------------------------------------------------------------------------------------------------
// Channels

#[derive(Clone, Debug, PartialEq)]
pub struct KickChannel {
    pub slug: String,
    pub channel_id: u64,
    pub livestream_id: Option<u64>,
    pub viewers: i64,
    pub title: String,
    pub game: String,
    pub acl: bool,
}

/// Parses /api/v2/channels/{slug}.
pub fn parse_channel(v: &Value, slug: &str) -> Option<KickChannel> {
    let d = if v["data"].is_object() { &v["data"] } else { v };
    let channel_id = u(&d["id"])?;
    let ls = &d["livestream"];
    Some(KickChannel {
        slug: Some(s(&d["slug"]).to_lowercase())
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| slug.to_lowercase()),
        channel_id,
        livestream_id: if ls.is_object() { u(&ls["id"]) } else { None },
        viewers: i(&ls["viewer_count"]),
        title: s(&ls["session_title"]),
        game: s(&ls["categories"][0]["name"]),
        acl: true,
    })
}

/// Parses /livestreams (the live channels of a category, most viewers first).
pub fn parse_livestreams(v: &Value) -> Vec<KickChannel> {
    list_of(v, &["livestreams"])
        .into_iter()
        .filter_map(|it| {
            let ch = if it["channel"].is_object() {
                &it["channel"]
            } else {
                it
            };
            let slug = s(first(ch, &["slug", "username"])).to_lowercase();
            if slug.is_empty() {
                return None;
            }
            Some(KickChannel {
                slug,
                channel_id: u(&ch["id"]).or_else(|| u(&it["channel_id"]))?,
                livestream_id: u(&it["id"]).or_else(|| u(&it["livestream_id"])),
                viewers: i(&it["viewer_count"]),
                title: s(first(it, &["title", "session_title"])),
                game: s(&it["category"]["name"]),
                acl: false,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// The engine

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KickState {
    Off,
    SigningIn,
    Loading,
    Idle,
    Watching,
    Problem,
}

struct Shared {
    state: KickState,
    status: String,
    user: Option<(u64, String)>,
    campaigns: Vec<KickCampaign>,
    loaded_at: Option<Instant>,
    target: Option<String>,
    candidates: Vec<KickChannel>,
    watching: Option<KickChannel>,
    pinned: Option<u64>,
    ws_state: String,
    cooldown: HashMap<u64, Instant>,
}

pub struct KickEngine {
    bridge: Arc<dyn KickBridge>,
    sink: Arc<dyn KickSink>,
    settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    shared: Mutex<Shared>,
    wake: Event,
    exiting: AtomicBool,
    reload_requested: AtomicBool,
    asked_sign_in: AtomicBool,
    rev: AtomicU64,
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

impl KickEngine {
    pub fn new(
        bridge: Arc<dyn KickBridge>,
        sink: Arc<dyn KickSink>,
        settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    ) -> Arc<KickEngine> {
        Arc::new(KickEngine {
            bridge,
            sink,
            settings,
            shared: Mutex::new(Shared {
                state: KickState::Off,
                status: "Kick is off".into(),
                user: None,
                campaigns: Vec::new(),
                loaded_at: None,
                target: None,
                candidates: Vec::new(),
                watching: None,
                pinned: None,
                ws_state: "closed".into(),
                cooldown: HashMap::new(),
            }),
            wake: Event::new(),
            exiting: AtomicBool::new(false),
            reload_requested: AtomicBool::new(true),
            asked_sign_in: AtomicBool::new(false),
            rev: AtomicU64::new(1),
        })
    }

    fn s(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn bump(&self) {
        self.rev.fetch_add(1, Ordering::Relaxed);
        self.sink.changed();
    }
    fn log(&self, text: impl Into<String>) {
        self.sink.log(format!("Kick: {}", text.into()));
    }
    fn set(&self, state: KickState, status: impl Into<String>) {
        {
            let mut s = self.s();
            s.state = state;
            s.status = status.into();
        }
        self.bump();
    }

    // ---- what the window can ask for ----

    /// Reload campaigns now (also picks up turning Kick on or off, and a new sign-in).
    pub fn reload(&self) {
        self.reload_requested.store(true, Ordering::SeqCst);
        self.wake.set();
    }
    /// Pin a channel (by its Kick channel ID) or let the miner choose.
    pub fn switch_to(&self, channel_id: Option<u64>) {
        self.s().pinned = channel_id;
        self.reload();
    }
    pub fn signed_out(&self) {
        self.s().user = None;
        self.reload();
    }
    pub fn exit(&self) {
        self.exiting.store(true, Ordering::SeqCst);
        self.wake.set();
    }

    /// Waits up to `d`, returning early when woken. True if woken by a reload or exit.
    async fn nap(&self, d: Duration) -> bool {
        let woke = self.wake.wait_for(d).await;
        self.wake.clear();
        woke
    }

    fn stop_watching(&self) {
        self.bridge.ws_close();
        let mut s = self.s();
        s.watching = None;
        s.ws_state = "closed".into();
    }

    // ---- the main loop ----

    pub async fn run(self: Arc<Self>) {
        while !self.exiting.load(Ordering::SeqCst) {
            if !(self.settings)().kick_enabled {
                self.stop_watching();
                self.set(KickState::Off, "Kick is off");
                self.nap(Duration::from_secs(3600)).await;
                continue;
            }
            if let Err(e) = self.clone().run_enabled().await {
                self.stop_watching();
                self.log(e.clone());
                self.set(KickState::Problem, e);
                self.nap(Duration::from_secs(60)).await;
            }
        }
        self.stop_watching();
    }

    /// One pass while Kick is on: sign-in, campaigns, claims, then watch until something changes.
    async fn run_enabled(self: Arc<Self>) -> Result<(), String> {
        if self.bridge.paused() {
            self.stop_watching();
            self.set(
                KickState::SigningIn,
                "Sign in on the Kick window, then close it",
            );
            self.nap(Duration::from_secs(2)).await;
            return Ok(());
        }
        // sign-in
        if self.s().user.is_none() {
            self.set(KickState::SigningIn, "Checking the Kick sign-in…");
            match self.fetch_user().await? {
                Some((id, name)) => {
                    self.asked_sign_in.store(false, Ordering::SeqCst);
                    self.s().user = Some((id, name.clone()));
                    self.log(format!("signed in as {name}."));
                    self.sink.signed_in(&name);
                }
                None => {
                    if !self.asked_sign_in.swap(true, Ordering::SeqCst) {
                        self.log("waiting for you to sign in to Kick (Settings › Kick › Sign in to Kick).");
                    }
                    self.set(KickState::SigningIn, "Sign in to Kick to mine Kick drops");
                    // look again soon: the sign-in window may be open
                    self.nap(Duration::from_secs(8)).await;
                    return Ok(());
                }
            }
        }
        // campaigns and claims
        if self.reload_requested.swap(false, Ordering::SeqCst)
            || self
                .s()
                .loaded_at
                .is_none_or(|t| t.elapsed() >= PROGRESS_EVERY)
        {
            self.set(KickState::Loading, "Loading Kick campaigns…");
            self.load().await?;
            self.claim_ready().await;
        }
        // pick a campaign and a channel
        let Some((cid, channel)) = self.pick().await? else {
            self.stop_watching();
            let st = if self.s().campaigns.iter().any(|c| c.minable(now())) {
                "No Kick channel to watch right now. Checking again in a few minutes."
            } else {
                "No Kick campaigns to mine for your games."
            };
            self.set(KickState::Idle, st);
            self.nap(PROGRESS_EVERY).await;
            return Ok(());
        };
        self.watch(&cid, channel).await
    }

    async fn get_json(&self, url: String, client_token: bool) -> Result<(u16, Value), String> {
        let r = self
            .bridge
            .fetch("GET", url.clone(), None, client_token)
            .await?;
        if r.status == 403 && r.body.contains("security policy") {
            return Err("Kick's firewall blocked a request. Open Account › Kick sign-in once so Kick can check the browser, then try again.".into());
        }
        Ok((r.status, r.json().unwrap_or(Value::Null)))
    }

    async fn fetch_user(&self) -> Result<Option<(u64, String)>, String> {
        let (status, v) = self.get_json(USER_URL.into(), false).await?;
        if status != 200 {
            return Ok(None);
        }
        Ok(u(&v["id"]).map(|id| (id, s(&v["username"]))))
    }

    async fn load(&self) -> Result<(), String> {
        let (st, camps) = self.get_json(CAMPAIGNS_URL.into(), false).await?;
        if st == 401 {
            self.s().user = None;
            return Err("Kick says you're signed out. Sign in to Kick again.".into());
        }
        if st != 200 {
            return Err(format!("Kick didn't send its campaigns (error {st})"));
        }
        let mut list = parse_campaigns(&camps);
        let (pst, prog) = self.get_json(PROGRESS_URL.into(), true).await?;
        if pst == 200 {
            merge_progress(&mut list, &prog);
        } else if pst == 401 {
            self.s().user = None;
            return Err("Kick says you're signed out. Sign in to Kick again.".into());
        } else {
            self.log(format!(
                "couldn't read drop progress (error {pst}); showing campaigns without it."
            ));
        }
        let n = list.len();
        {
            let mut s = self.s();
            s.campaigns = list;
            s.loaded_at = Some(Instant::now());
        }
        self.log(format!("loaded {n} campaigns."));
        self.bump();
        Ok(())
    }

    async fn claim_ready(&self) {
        let ready: Vec<(String, String, String, String)> = {
            let s = self.s();
            s.campaigns
                .iter()
                .flat_map(|c| {
                    c.rewards
                        .iter()
                        .filter(|r| c.claimable(r))
                        .map(|r| (c.id.clone(), r.id.clone(), c.game.clone(), r.name.clone()))
                })
                .collect()
        };
        for (cid, rid, game, name) in ready {
            let body = json!({ "campaign_id": cid, "reward_id": rid }).to_string();
            match self
                .bridge
                .fetch("POST", CLAIM_URL.into(), Some(body), true)
                .await
            {
                Ok(r) if r.status == 200 || r.status == 201 => {
                    {
                        let mut s = self.s();
                        if let Some(rw) = s
                            .campaigns
                            .iter_mut()
                            .find(|c| c.id == cid)
                            .and_then(|c| c.rewards.iter_mut().find(|r| r.id == rid))
                        {
                            rw.claimed = true;
                        }
                    }
                    let text = format!("{game}: {name}");
                    self.log(format!("claimed drop: {text}"));
                    if (self.settings)().notifications {
                        self.sink.notify("Kick drop claimed", &text);
                    }
                    self.bump();
                }
                Ok(r) if r.body.contains("INVALID_CLAIM") || r.status == 400 => {
                    self.log(format!(
                        "couldn't claim {name} for {game}: link your game account to Kick first."
                    ));
                    if let Some(c) = self.s().campaigns.iter_mut().find(|c| c.id == cid) {
                        c.linked = false;
                    }
                }
                Ok(r) => self.log(format!("claiming {name} failed (error {}).", r.status)),
                Err(e) => self.log(format!("claiming {name} failed: {e}")),
            }
        }
    }

    /// Chooses the campaign and channel to watch: the pinned channel if it qualifies, otherwise the best live
    /// channel for the first campaign (in priority order) that has one.
    async fn pick(&self) -> Result<Option<(String, KickChannel)>, String> {
        let settings = (self.settings)();
        let (order, campaigns, pinned) = {
            let s = self.s();
            (
                pick_order(&s.campaigns, &settings, now()),
                s.campaigns.clone(),
                s.pinned,
            )
        };
        for i in order {
            let c = &campaigns[i];
            let mut found = self.candidates(c).await?;
            {
                let mut s = self.s();
                s.cooldown.retain(|_, t| t.elapsed() < COOLDOWN);
                let cool: HashSet<u64> = s.cooldown.keys().copied().collect();
                found.retain(|ch| ch.livestream_id.is_some() && !cool.contains(&ch.channel_id));
                s.target = Some(c.id.clone());
                s.candidates = found.clone();
            }
            self.bump();
            let choice = pinned
                .and_then(|p| found.iter().find(|ch| ch.channel_id == p))
                .or_else(|| found.first())
                .cloned();
            if let Some(ch) = choice {
                return Ok(Some((c.id.clone(), ch)));
            }
        }
        let mut s = self.s();
        s.target = None;
        s.candidates.clear();
        Ok(None)
    }

    /// Live channels for a campaign: its allowed channels that are live, or the category's live channels.
    async fn candidates(&self, c: &KickCampaign) -> Result<Vec<KickChannel>, String> {
        let mut out = Vec::new();
        if !c.channels.is_empty() {
            for slug in c.channels.iter().take(15) {
                let (st, v) = self.get_json(channel_url(slug), false).await?;
                if st == 200 {
                    if let Some(ch) = parse_channel(&v, slug) {
                        out.push(ch);
                    }
                }
            }
        } else if let Some(cat) = c.category_id {
            let (st, v) = self.get_json(livestreams_url(cat), false).await?;
            if st == 200 {
                out = parse_livestreams(&v);
            }
        }
        out.sort_by(|a, b| b.acl.cmp(&a.acl).then(b.viewers.cmp(&a.viewers)));
        Ok(out)
    }

    async fn watch(&self, campaign_id: &str, ch: KickChannel) -> Result<(), String> {
        let Some(livestream_id) = ch.livestream_id else {
            return Ok(());
        };
        // the viewer websocket needs a fresh token
        let (st, v) = self.get_json(VIEWER_TOKEN_URL.into(), true).await?;
        let token = v["data"]["token"]
            .as_str()
            .map(str::to_string)
            .filter(|_| st == 200);
        let Some(token) = token else {
            self.s().cooldown.insert(ch.channel_id, Instant::now());
            return Err(format!("Kick didn't hand out a viewer token (error {st})"));
        };
        self.bridge.ws_open(token).await?;
        let game = self
            .s()
            .campaigns
            .iter()
            .find(|c| c.id == campaign_id)
            .map(|c| c.game.clone())
            .unwrap_or_default();
        self.s().watching = Some(ch.clone());
        self.log(format!("watching {} ({game}).", ch.slug));
        self.set(KickState::Watching, format!("Watching {} on Kick", ch.slug));

        let started = Instant::now();
        let mut last_watch_event: Option<Instant> = None;
        let mut last_live_check = Instant::now();
        let mut last_progress_minutes = self
            .s()
            .campaigns
            .iter()
            .find(|c| c.id == campaign_id)
            .map(|c| c.minutes)
            .unwrap_or(0);
        let mut last_progress_change = Instant::now();
        let mut alternate = false;
        let mut closed_strikes = 0;
        loop {
            // keep the socket alive: handshake and ping take turns every 12 seconds
            let msg = if alternate {
                ws_ping()
            } else {
                ws_handshake(ch.channel_id)
            };
            alternate = !alternate;
            let state = self
                .bridge
                .ws_send(msg)
                .await
                .unwrap_or_else(|_| "closed".into());
            if last_watch_event.is_none_or(|t| t.elapsed() >= WATCH_EVENT_EVERY) && state == "open"
            {
                let _ = self
                    .bridge
                    .ws_send(ws_watch_event(ch.channel_id, livestream_id))
                    .await;
                last_watch_event = Some(Instant::now());
            }
            if self.s().ws_state != state {
                self.s().ws_state = state.clone();
                self.bump();
            }
            if state == "closed" {
                closed_strikes += 1;
                if closed_strikes >= 3 {
                    self.log(format!(
                        "the connection to {} keeps closing; trying another channel.",
                        ch.slug
                    ));
                    self.s().cooldown.insert(ch.channel_id, Instant::now());
                    self.stop_watching();
                    return Ok(());
                }
            } else {
                closed_strikes = 0;
            }
            if self.nap(HANDSHAKE_EVERY).await
                || self.exiting.load(Ordering::SeqCst)
                || !(self.settings)().kick_enabled
            {
                self.stop_watching();
                return Ok(());
            }
            // still live?
            if last_live_check.elapsed() >= LIVE_CHECK_EVERY {
                last_live_check = Instant::now();
                if let Ok((200, v)) = self.get_json(channel_url(&ch.slug), false).await {
                    match parse_channel(&v, &ch.slug) {
                        Some(now_ch) if now_ch.livestream_id == Some(livestream_id) => {
                            if let Some(w) = self.s().watching.as_mut() {
                                w.viewers = now_ch.viewers;
                            }
                            self.bump();
                        }
                        _ => {
                            self.log(format!("{} went offline; switching.", ch.slug));
                            self.stop_watching();
                            return Ok(());
                        }
                    }
                }
            }
            // progress, claims, and whether this campaign is done
            if started.elapsed() >= PROGRESS_EVERY
                && self
                    .s()
                    .loaded_at
                    .is_none_or(|t| t.elapsed() >= PROGRESS_EVERY)
            {
                self.load().await?;
                self.claim_ready().await;
                let (minutes, minable) = {
                    let s = self.s();
                    let c = s.campaigns.iter().find(|c| c.id == campaign_id);
                    (
                        c.map(|c| c.minutes).unwrap_or(0),
                        c.is_some_and(|c| c.minable(now())),
                    )
                };
                if !minable {
                    self.log("this campaign is done; moving on.");
                    self.stop_watching();
                    return Ok(());
                }
                if minutes != last_progress_minutes {
                    last_progress_minutes = minutes;
                    last_progress_change = Instant::now();
                } else if last_progress_change.elapsed() >= STALL_AFTER {
                    self.log(format!(
                        "no progress on {} for 20 minutes; trying another channel.",
                        ch.slug
                    ));
                    self.s().cooldown.insert(ch.channel_id, Instant::now());
                    self.stop_watching();
                    return Ok(());
                }
            }
        }
    }

    // ---- what the window shows ----

    pub fn snapshot(&self) -> KickSnapshot {
        let settings = (self.settings)();
        let s = self.s();
        let n = now();
        let target = s
            .target
            .as_ref()
            .and_then(|t| s.campaigns.iter().find(|c| &c.id == t));
        let watching = s.watching.as_ref().map(|w| WatchingView {
            id: w.channel_id,
            name: w.slug.clone(),
            login: w.slug.clone(),
            game: target
                .map(|c| c.game.clone())
                .or_else(|| Some(w.game.clone()).filter(|g| !g.is_empty())),
            viewers: Some(w.viewers),
            title: w.title.clone(),
            url: format!("https://kick.com/{}", w.slug),
        });
        let drop = target.filter(|_| s.watching.is_some()).and_then(|c| {
            let r = c.next_reward()?;
            let current = c.reward_minutes(r);
            Some(DropView {
                id: r.id.clone(),
                campaign_id: c.id.clone(),
                campaign: c.name.clone(),
                game: c.game.clone(),
                image: c.image_url.clone(),
                name: r.name.clone(),
                rewards: vec![BenefitView {
                    name: r.name.clone(),
                    image: r.image_url.clone(),
                }],
                progress: c.reward_progress(r),
                current_minutes: current,
                required_minutes: r.required_minutes,
                remaining_minutes: (r.required_minutes - current).max(0),
                campaign_progress: c.progress(),
                campaign_remaining_minutes: c.remaining_minutes(),
                claimed: c.claimed_count(),
                total: c.rewards.len(),
                timer_started_ms: None,
                subone: false,
                ends_at: c.ends_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            })
        });
        let order = pick_order(&s.campaigns, &settings, n);
        let mut wanted: Vec<String> = Vec::new();
        for i in &order {
            let g = &s.campaigns[*i].game;
            if !wanted.contains(g) {
                wanted.push(g.clone());
            }
        }
        let mut games: Vec<String> = s
            .campaigns
            .iter()
            .map(|c| c.game.clone())
            .filter(|g| !g.is_empty())
            .collect();
        games.sort();
        games.dedup();
        let rank = |g: &str| {
            settings
                .priority
                .iter()
                .position(|p| p.eq_ignore_ascii_case(g))
        };
        KickSnapshot {
            rev: self.rev.load(Ordering::Relaxed),
            enabled: settings.kick_enabled,
            state: s.state,
            status: s.status.clone(),
            signed_in: s.user.is_some(),
            user: s.user.as_ref().map(|(id, name)| KickUser {
                id: *id,
                username: name.clone(),
            }),
            ws_state: s.ws_state.clone(),
            watching,
            pinned: s.pinned,
            drop,
            channels: s
                .candidates
                .iter()
                .map(|c| ChannelView {
                    id: c.channel_id,
                    name: c.slug.clone(),
                    login: c.slug.clone(),
                    status: if c.livestream_id.is_some() {
                        "online"
                    } else {
                        "offline"
                    },
                    game: target
                        .map(|t| t.game.clone())
                        .or_else(|| Some(c.game.clone()).filter(|g| !g.is_empty())),
                    viewers: c.livestream_id.map(|_| c.viewers),
                    drops: true,
                    acl: c.acl,
                    watching: s
                        .watching
                        .as_ref()
                        .is_some_and(|w| w.channel_id == c.channel_id),
                    can_watch: c.livestream_id.is_some(),
                    priority: target.and_then(|t| rank(&t.game)),
                })
                .collect(),
            wanted,
            games,
            campaigns: s.campaigns.len(),
        }
    }

    pub fn inventory(&self) -> Vec<CampaignView> {
        let settings = (self.settings)();
        let s = self.s();
        let n = now();
        let wanted: HashSet<String> = pick_order(&s.campaigns, &settings, n)
            .into_iter()
            .map(|i| s.campaigns[i].game.clone())
            .collect();
        s.campaigns
            .iter()
            .map(|c| CampaignView {
                id: c.id.clone(),
                name: c.name.clone(),
                game: c.game.clone(),
                image: c.image_url.clone(),
                status: if c.active(n) {
                    "active"
                } else if c.upcoming(n) {
                    "upcoming"
                } else {
                    "expired"
                },
                linked: c.linked,
                eligible: c.linked,
                link_url: c.connect_url.clone(),
                starts_at: c.starts_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                ends_at: c.ends_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                excluded: settings
                    .exclude
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(&c.game)),
                priority: settings
                    .priority
                    .iter()
                    .position(|p| p.eq_ignore_ascii_case(&c.game)),
                wanted: wanted.contains(&c.game),
                finished: c.finished(),
                claimed: c.claimed_count(),
                total: c.rewards.len(),
                progress: c.progress(),
                remaining_minutes: c.remaining_minutes(),
                acl: c.channels.iter().take(6).cloned().collect(),
                acl_count: c.channels.len(),
                drops: c
                    .rewards
                    .iter()
                    .map(|r| DropItemView {
                        id: r.id.clone(),
                        name: r.name.clone(),
                        rewards: vec![BenefitView {
                            name: r.name.clone(),
                            image: r.image_url.clone(),
                        }],
                        progress: c.reward_progress(r),
                        current_minutes: c.reward_minutes(r),
                        required_minutes: r.required_minutes,
                        claimed: r.claimed,
                        can_claim: c.claimable(r),
                        earnable: c.minable(n) && !r.claimed,
                        starts_at: c.starts_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                        ends_at: c.ends_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect()
    }
}

#[derive(Serialize, Clone)]
pub struct KickUser {
    pub id: u64,
    pub username: String,
}

#[derive(Serialize, Clone)]
pub struct KickSnapshot {
    pub rev: u64,
    pub enabled: bool,
    pub state: KickState,
    pub status: String,
    pub signed_in: bool,
    pub user: Option<KickUser>,
    pub ws_state: String,
    pub watching: Option<WatchingView>,
    pub pinned: Option<u64>,
    pub drop: Option<DropView>,
    pub channels: Vec<ChannelView>,
    pub wanted: Vec<String>,
    pub games: Vec<String>,
    pub campaigns: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn campaigns_json() -> Value {
        json!({ "data": [
            { "id": "c1", "name": "Rust x Kick Round 3", "status": "active",
              "starts_at": "2026-11-01T00:00:00Z", "ends_at": "2026-12-01T00:00:00Z",
              "category": { "id": 13, "name": "Rust", "slug": "rust", "image_url": "https://files.kick.com/rust.jpg" },
              "connect_url": "https://kick.facepunch.com", "channels": [ { "slug": "Blooprint" }, { "user": { "username": "hJune" } } ],
              "rewards": [ { "id": "r1", "name": "Kick Hoodie", "image_url": "drops/r1.png", "required_units": 60 },
                           { "id": "r2", "name": "Kick Door", "image_url": "drops/r2.png", "required_units": 180 } ] },
            { "id": "c2", "name": "General Drops", "status": "active",
              "category": { "id": 99, "name": "Overwatch" },
              "rewards": [ { "id": "o1", "name": "Spray", "required_units": 30 } ] },
            { "id": "c3", "name": "Later", "status": "upcoming", "category": { "id": 5, "name": "Rust" },
              "rewards": [ { "id": "u1", "name": "Hat", "required_units": 30 } ] }
        ] })
    }

    fn now_fixed() -> DateTime<Utc> {
        parse_ts("2026-11-18T12:00:00Z").unwrap()
    }

    #[test]
    fn parses_campaigns_with_kick_shapes() {
        let c = parse_campaigns(&campaigns_json());
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].game, "Rust");
        assert_eq!(c[0].category_id, Some(13));
        assert_eq!(c[0].channels, vec!["blooprint", "hjune"]);
        assert_eq!(
            c[0].rewards[0].image_url,
            "https://ext.kick.com/drops/r1.png"
        );
        assert_eq!(c[0].rewards[1].required_minutes, 180);
        // other wrappers work too
        let wrapped = json!({ "data": { "campaigns": campaigns_json()["data"].clone() } });
        assert_eq!(parse_campaigns(&wrapped).len(), 3);
    }

    #[test]
    fn progress_sets_minutes_claims_and_linking() {
        let mut c = parse_campaigns(&campaigns_json());
        merge_progress(
            &mut c,
            &json!({ "data": [
                { "id": "c1", "progress_units": 75, "user_app_connected": true,
                  "rewards": [ { "id": "r1", "progress": 1.0, "claimed": true }, { "id": "r2", "progress": 0.41, "claimed": false } ] },
                { "id": "c2", "progress_units": 30, "user_app_connected": false, "connect_url": "https://link.example" }
            ] }),
        );
        let rust = &c[0];
        assert_eq!(rust.minutes, 75);
        assert!(rust.rewards[0].claimed);
        assert_eq!(rust.reward_minutes(&rust.rewards[1]), 75);
        assert_eq!(rust.next_reward().unwrap().id, "r2");
        assert!(rust.linked);
        // not linked: nothing claimable, not minable
        assert!(!c[1].linked);
        assert!(!c[1].claimable(&c[1].rewards[0]));
        assert!(!c[1].minable(now_fixed()));
    }

    #[test]
    fn ready_rewards_are_claimable() {
        let mut c = parse_campaigns(&campaigns_json());
        merge_progress(
            &mut c,
            &json!({ "data": [ { "id": "c1", "progress_units": 60, "rewards": [] } ] }),
        );
        assert!(c[0].claimable(&c[0].rewards[0]));
        assert!(!c[0].claimable(&c[0].rewards[1]));
    }

    #[test]
    fn order_follows_the_shared_game_lists() {
        let c = parse_campaigns(&campaigns_json());
        let mut s = Settings::default();
        // priority-only with nothing listed: nothing to mine
        assert!(pick_order(&c, &s, now_fixed()).is_empty());
        s.priority = vec!["Overwatch".into(), "rust".into()];
        assert_eq!(pick_order(&c, &s, now_fixed()), vec![1, 0]);
        s.exclude.insert("Overwatch".into());
        assert_eq!(pick_order(&c, &s, now_fixed()), vec![0]);
        // other modes add unlisted games after the list; upcoming campaigns never count
        s.priority.clear();
        s.exclude.clear();
        s.priority_mode = PriorityMode::EndingSoonest;
        assert_eq!(pick_order(&c, &s, now_fixed()), vec![0, 1]);
    }

    #[test]
    fn parses_channels_and_livestreams() {
        let ch = parse_channel(
            &json!({ "id": 4242, "slug": "Blooprint", "livestream": { "id": 777, "viewer_count": 1500, "session_title": "wipe", "categories": [ { "name": "Rust" } ] } }),
            "blooprint",
        )
        .unwrap();
        assert_eq!(
            (ch.channel_id, ch.livestream_id, ch.viewers),
            (4242, Some(777), 1500)
        );
        assert_eq!(ch.slug, "blooprint");
        let off = parse_channel(&json!({ "id": 1, "slug": "x", "livestream": null }), "x").unwrap();
        assert_eq!(off.livestream_id, None);
        let list = parse_livestreams(&json!({ "data": { "livestreams": [
            { "id": 9, "viewer_count": 50, "channel": { "id": 3, "slug": "Alpha" }, "category": { "name": "Overwatch" } },
            { "id": 10, "viewer_count": 70, "channel": { "id": 4, "slug": "beta" } } ] } }));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].slug, "alpha");
        assert_eq!(list[1].livestream_id, Some(10));
    }

    #[test]
    fn websocket_messages_match_kick() {
        let w: Value = serde_json::from_str(&ws_watch_event(4, 9)).unwrap();
        assert_eq!(
            w["data"]["message"]["name"],
            "tracking.user.watch.livestream"
        );
        assert_eq!(w["data"]["message"]["livestream_id"], 9);
        let h: Value = serde_json::from_str(&ws_handshake(4)).unwrap();
        assert_eq!(h["data"]["message"]["channelId"], 4);
    }

    // ---- the whole loop against a fake Kick ----

    struct FakeKick {
        calls: Mutex<Vec<String>>,
        sent: Mutex<Vec<String>>,
    }

    impl KickBridge for FakeKick {
        fn fetch(
            &self,
            method: &'static str,
            url: String,
            body: Option<String>,
            _ct: bool,
        ) -> BoxFut<'_, Result<KickResp, String>> {
            self.calls.lock().unwrap().push(format!(
                "{method} {url} {}",
                body.clone().unwrap_or_default()
            ));
            let (status, body) = match (method, url.as_str()) {
                ("GET", USER_URL) => (200, json!({ "id": 55, "username": "jeremy" })),
                ("GET", CAMPAIGNS_URL) => (200, campaigns_json()),
                ("GET", PROGRESS_URL) => (
                    200,
                    json!({ "data": [ { "id": "c1", "progress_units": 60, "user_app_connected": true, "rewards": [] } ] }),
                ),
                ("POST", CLAIM_URL) => (200, json!({ "status": "ok" })),
                ("GET", VIEWER_TOKEN_URL) => (200, json!({ "data": { "token": "tok" } })),
                ("GET", u) if u.ends_with("/blooprint") => (
                    200,
                    json!({ "id": 4242, "slug": "blooprint", "livestream": { "id": 777, "viewer_count": 1500 } }),
                ),
                ("GET", u) if u.ends_with("/hjune") => (
                    200,
                    json!({ "id": 4343, "slug": "hjune", "livestream": null }),
                ),
                _ => (404, json!({})),
            };
            Box::pin(async move {
                Ok(KickResp {
                    status,
                    body: body.to_string(),
                })
            })
        }
        fn ws_open(&self, token: String) -> BoxFut<'_, Result<(), String>> {
            self.sent.lock().unwrap().push(format!("open {token}"));
            Box::pin(async { Ok(()) })
        }
        fn ws_send(&self, message: String) -> BoxFut<'_, Result<String, String>> {
            self.sent.lock().unwrap().push(message);
            Box::pin(async { Ok("open".to_string()) })
        }
        fn ws_close(&self) {}
    }

    struct Quiet;
    impl KickSink for Quiet {
        fn changed(&self) {}
        fn log(&self, _t: String) {}
        fn notify(&self, _a: &str, _b: &str) {}
        fn signed_in(&self, _u: &str) {}
    }

    #[tokio::test(start_paused = true)]
    async fn signs_in_claims_and_watches_the_allowed_channel() {
        let fake = Arc::new(FakeKick {
            calls: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
        });
        let settings = Arc::new(|| Settings {
            kick_enabled: true,
            priority: vec!["Rust".into()],
            ..Default::default()
        });
        let engine = KickEngine::new(fake.clone(), Arc::new(Quiet), settings);
        let run = tokio::spawn(engine.clone().run());
        tokio::time::sleep(Duration::from_secs(30)).await;
        let snap = engine.snapshot();
        engine.exit();
        let _ = run.await;
        let calls = fake.calls.lock().unwrap().join("\n");
        // the 60-minute reward was claimed
        assert!(calls.contains(r#"POST https://web.kick.com/api/v1/drops/claim {"campaign_id":"c1","reward_id":"r1"}"#), "{calls}");
        // the live allowed channel was chosen, the offline one skipped
        assert_eq!(
            snap.watching.as_ref().map(|w| w.name.as_str()),
            Some("blooprint")
        );
        assert_eq!(
            snap.user.as_ref().map(|u| u.username.as_str()),
            Some("jeremy")
        );
        assert_eq!(
            snap.drop.as_ref().map(|d| d.name.as_str()),
            Some("Kick Door")
        );
        let sent = fake.sent.lock().unwrap().join("\n");
        assert!(sent.starts_with("open tok"));
        assert!(sent.contains("tracking.user.watch.livestream"));
        assert!(sent.contains(r#""channelId":4242"#));
    }
}
