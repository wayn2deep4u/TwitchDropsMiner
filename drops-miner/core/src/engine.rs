//! The miner: signs in, loads the drops inventory, picks a channel, "watches" it, follows progress and
//! claims drops. A port of twitch.py's state machine and watch loop; the window talks to it through
//! [`Engine`]'s methods and hears back through a [`Sink`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use indexmap::IndexMap;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{id_of, Campaign, Channel, Game, Stream};
use crate::pubsub::{PubSub, SocketStatus};
use crate::settings::{PriorityMode, Session, Settings};
use crate::twitch::*;
use crate::util::{chunks, Event};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------------------------------------------
// What the engine tells the window

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayIcon {
    Pickaxe,
    Active,
    Idle,
    Maint,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub time: String,
    pub text: String,
}

/// How the engine reaches the window. Every method must return quickly.
pub trait Sink: Send + Sync + 'static {
    /// Something on screen changed; the window should ask for a new snapshot.
    fn changed(&self);
    fn log(&self, line: &LogLine);
    /// A drop was claimed (and the person wants notifications).
    fn notify(&self, title: &str, body: &str);
    /// Sign-in needed: the window should open Twitch's sign-in page.
    fn login_needed(&self, url: &str);
    fn tray(&self, icon: TrayIcon, tooltip: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MinerState {
    Idle,
    InventoryFetch,
    GamesUpdate,
    ChannelsCleanup,
    ChannelsFetch,
    ChannelSwitch,
    Restart,
    Exit,
}

#[derive(Clone, Debug)]
struct DropDisplay {
    drop_id: String,
    /// When the current minute started counting down (None: not counting).
    timer: Option<Instant>,
    timer_wall_ms: Option<i64>,
    subone: bool,
}

struct Shared {
    settings: Settings,
    session: Session,
    state: MinerState,
    status: String,
    icon: TrayIcon,
    inventory: Vec<Campaign>,
    drop_index: HashMap<String, (usize, usize)>,
    campaign_index: HashMap<String, usize>,
    wanted: Vec<Game>,
    channels: IndexMap<u64, Channel>,
    watching: Option<u64>,
    pinned: Option<u64>,
    display: Option<DropDisplay>,
    login_url: Option<String>,
    log: VecDeque<LogLine>,
    log_seq: u64,
    mnt_triggers: VecDeque<DateTime<Utc>>,
    pending: HashMap<u64, tokio::task::AbortHandle>,
    signed_in: bool,
}

impl Shared {
    fn priority(&self, ch: &Channel) -> usize {
        match ch.game() {
            Some(g) => self
                .wanted
                .iter()
                .position(|w| w == g)
                .unwrap_or(usize::MAX),
            None => usize::MAX,
        }
    }

    fn can_watch(&self, ch: &Channel, now: DateTime<Utc>) -> bool {
        if !ch.online() {
            return false;
        }
        let r = ch.as_ref();
        self.inventory.iter().any(|c| {
            c.can_earn(Some(&r), false, now)
                && ((ch.game().is_some_and(|g| self.wanted.contains(g)) && ch.drops_enabled())
                    || c.game.is_special())
        })
    }

    fn should_switch(&self, ch: &Channel, now: DateTime<Utc>) -> bool {
        if !self.can_watch(ch, now) {
            return false;
        }
        let Some(w) = self.watching.and_then(|id| self.channels.get(&id)) else {
            return true;
        };
        if !self.can_watch(w, now) {
            return true;
        }
        let (a, b) = (self.priority(ch), self.priority(w));
        a < b || (a == b && ch.acl_based && !w.acl_based)
    }

    /// The campaign most likely being mined on `channel` (or the watched one): the earnable one closest to done.
    fn active_campaign(&self, channel: Option<u64>, now: DateTime<Utc>) -> Option<usize> {
        if self.wanted.is_empty() {
            return None;
        }
        let ch = self
            .watching
            .or(channel)
            .and_then(|id| self.channels.get(&id))?;
        let r = ch.as_ref();
        self.inventory
            .iter()
            .enumerate()
            .filter(|(_, c)| c.can_earn(Some(&r), false, now))
            .min_by_key(|(_, c)| c.remaining_minutes())
            .map(|(i, _)| i)
    }

    fn drops_enabled_from(&self, ch: &Channel, available: &[Value], now: DateTime<Utc>) -> bool {
        let r = ch.as_ref();
        available.iter().any(|c| {
            c["id"]
                .as_str()
                .and_then(|id| self.campaign_index.get(id))
                .is_some_and(|i| self.inventory[*i].can_earn(Some(&r), true, now))
        })
    }
}

enum Flow {
    Exit,
    Restart,
}

pub struct Engine {
    dir: PathBuf,
    sink: Arc<dyn Sink>,
    shared: Mutex<Shared>,
    state_change: Event,
    watching_restart: Event,
    watching_set: Event,
    exit_event: Event,
    exiting: Arc<AtomicBool>,
    http: RwLock<Option<Arc<Http>>>,
    pubsub: RwLock<Option<PubSub>>,
    token_tx: Mutex<Option<oneshot::Sender<String>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    mnt_task: Mutex<Option<JoinHandle<()>>>,
    rev: AtomicU64,
    log_file: Mutex<Option<std::fs::File>>,
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

impl Engine {
    pub fn new(dir: PathBuf, sink: Arc<dyn Sink>) -> Arc<Engine> {
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("miner.log");
        if std::fs::metadata(&log_path)
            .map(|m| m.len() > 2_000_000)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&log_path, dir.join("miner.old.log"));
        }
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        Arc::new(Engine {
            shared: Mutex::new(Shared {
                settings: Settings::load(&dir),
                session: Session::load(&dir),
                state: MinerState::Idle,
                status: "Starting…".into(),
                icon: TrayIcon::Pickaxe,
                inventory: Vec::new(),
                drop_index: HashMap::new(),
                campaign_index: HashMap::new(),
                wanted: Vec::new(),
                channels: IndexMap::new(),
                watching: None,
                pinned: None,
                display: None,
                login_url: None,
                log: VecDeque::new(),
                log_seq: 0,
                mnt_triggers: VecDeque::new(),
                pending: HashMap::new(),
                signed_in: false,
            }),
            dir,
            sink,
            state_change: Event::new(),
            watching_restart: Event::new(),
            watching_set: Event::new(),
            exit_event: Event::new(),
            exiting: Arc::new(AtomicBool::new(false)),
            http: RwLock::new(None),
            pubsub: RwLock::new(None),
            token_tx: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            mnt_task: Mutex::new(None),
            rev: AtomicU64::new(1),
            log_file: Mutex::new(log_file),
        })
    }

    fn s(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn http(&self) -> Result<Arc<Http>> {
        self.http.read().unwrap().clone().ok_or(Error::Exit)
    }

    fn bump(&self) {
        self.rev.fetch_add(1, Ordering::Relaxed);
        self.sink.changed();
    }

    // ---- messages ------------------------------------------------------------------------------------------

    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        let line = {
            let mut s = self.s();
            s.log_seq += 1;
            let line = LogLine {
                seq: s.log_seq,
                time: Local::now().format("%H:%M:%S").to_string(),
                text,
            };
            s.log.push_back(line.clone());
            while s.log.len() > 600 {
                s.log.pop_front();
            }
            line
        };
        if let Some(f) = self.log_file.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = writeln!(
                f,
                "{} {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                line.text
            );
        }
        self.sink.log(&line);
    }

    fn status(&self, text: impl Into<String>) {
        self.s().status = text.into();
        self.sink.changed();
    }

    fn icon(&self, icon: TrayIcon) {
        let tip = {
            let mut s = self.s();
            s.icon = icon;
            tray_tooltip(&s)
        };
        self.sink.tray(icon, &tip);
    }

    // ---- what the window can ask for ----------------------------------------------------------------------------

    /// Hands over the token from the sign-in window.
    pub fn submit_token(&self, token: String) {
        if let Some(tx) = self.token_tx.lock().unwrap().take() {
            let _ = tx.send(token);
        }
    }

    pub fn login_url(&self) -> Option<String> {
        self.s().login_url.clone()
    }

    /// Forget the sign-in and ask for a new one.
    pub fn logout(&self) {
        {
            let mut s = self.s();
            s.session.access_token.clear();
            s.session.user_id = 0;
            s.session.login.clear();
            let _ = s.session.save(&self.dir);
        }
        self.log("Signed out.");
        self.change_state(MinerState::Restart);
    }

    /// Reload the inventory (and apply settings changes).
    pub fn reload(&self) {
        self.change_state(MinerState::InventoryFetch);
    }

    /// Pin a channel to watch (None: let the miner choose).
    pub fn switch_to(&self, channel: Option<u64>) {
        self.s().pinned = channel;
        self.change_state(MinerState::ChannelSwitch);
    }

    pub fn settings(&self) -> Settings {
        self.s().settings.clone()
    }

    /// Changes and saves the settings. Most take effect on the next reload.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        {
            let mut s = self.s();
            f(&mut s.settings);
            s.settings.connection_quality = s.settings.connection_quality.clamp(1, 6);
            let _ = s.settings.save(&self.dir);
        }
        self.bump();
    }

    /// Stops everything; `run` returns soon after.
    pub fn exit(&self) {
        self.change_state(MinerState::Exit);
        self.exiting.store(true, Ordering::SeqCst);
        self.exit_event.set();
    }

    fn change_state(&self, state: MinerState) {
        {
            let mut s = self.s();
            if s.state != MinerState::Exit {
                s.state = state;
            }
        }
        self.state_change.set();
    }

    // ---- the main loop ---------------------------------------------------------------------------------------

    /// Runs until `exit` is called. Problems are logged and retried; a lost sign-in asks for a new one.
    pub async fn run(self: Arc<Self>) {
        self.log(format!("Drops Miner {VERSION} started."));
        let mut failures: u32 = 0;
        loop {
            let result = self.clone().run_once().await;
            self.stop_run().await;
            if self.exiting.load(Ordering::SeqCst) {
                break;
            }
            match result {
                Ok(Flow::Exit) | Err(Error::Exit) => break,
                Ok(Flow::Restart) => {
                    failures = 0;
                    continue;
                }
                Err(Error::Auth(m)) => {
                    self.log(format!("{m}. Please sign in again."));
                    let mut s = self.s();
                    s.session.access_token.clear();
                    let _ = s.session.save(&self.dir);
                }
                Err(e) => {
                    failures += 1;
                    let wait = (30 * failures).min(300) as u64;
                    self.log(format!("{e}. Trying again in {wait}s."));
                    self.status(format!("Problem: {e}"));
                    self.icon(TrayIcon::Error);
                    if self.exit_event.wait_for(Duration::from_secs(wait)).await {
                        break;
                    }
                }
            }
        }
        self.status("Stopped");
        let s = self.s();
        let _ = s.settings.save(&self.dir);
        let _ = s.session.save(&self.dir);
    }

    async fn stop_run(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        if let Some(t) = self.mnt_task.lock().unwrap().take() {
            t.abort();
        }
        if let Some(p) = self.pubsub.write().unwrap().take() {
            p.stop(true);
        }
        *self.http.write().unwrap() = None;
        {
            let mut s = self.s();
            for (_, h) in s.pending.drain() {
                h.abort();
            }
            s.channels.clear();
            s.inventory.clear();
            s.drop_index.clear();
            s.campaign_index.clear();
            s.wanted.clear();
            s.mnt_triggers.clear();
            s.watching = None;
            s.display = None;
            s.signed_in = false;
            let st = s.state;
            if st != MinerState::Exit {
                s.state = MinerState::Idle;
            }
        }
        self.watching_set.clear();
        self.bump();
    }

    async fn run_once(self: Arc<Self>) -> Result<Flow> {
        let (quality, proxy) = {
            let s = self.s();
            (s.settings.connection_quality, s.settings.proxy.clone())
        };
        let me = self.clone();
        let http = Arc::new(Http::new(
            quality,
            &proxy,
            Arc::new(move |m| me.log(m)),
            self.exiting.clone(),
        )?);
        *self.http.write().unwrap() = Some(http.clone());

        self.sign_in(&http).await?;
        let user_id = self.s().session.user_id;

        // PubSub: messages are handled as they arrive
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
        let sink = self.sink.clone();
        let me = self.clone();
        let pubsub = PubSub::new(
            tx,
            Arc::new(move || sink.changed()),
            Arc::new(move |m| me.log(m)),
        );
        pubsub.set_token(&http.token());
        pubsub.start();
        pubsub.add_topics(vec![
            topic(TOPIC_USER_DROPS, user_id),
            topic(TOPIC_USER_NOTIFICATIONS, user_id),
        ]);
        *self.pubsub.write().unwrap() = Some(pubsub);

        let me = self.clone();
        let dispatcher = tokio::spawn(async move {
            while let Some((t, msg)) = rx.recv().await {
                let me2 = me.clone();
                tokio::spawn(async move { me2.on_pubsub(t, msg).await });
            }
        });
        let me = self.clone();
        let watcher = tokio::spawn(async move { me.watch_loop().await });
        self.tasks.lock().unwrap().extend([dispatcher, watcher]);

        let mut full_cleanup = false;
        self.change_state(MinerState::InventoryFetch);
        loop {
            let state = self.s().state;
            match state {
                MinerState::Idle => {
                    self.icon(TrayIcon::Idle);
                    self.status("Idle");
                    self.stop_watching();
                    self.state_change.clear();
                }
                MinerState::InventoryFetch => {
                    self.icon(TrayIcon::Maint);
                    if let Some(p) = self.pubsub.read().unwrap().as_ref() {
                        p.start();
                    }
                    self.fetch_inventory().await?;
                    let _ = self.s().settings.save(&self.dir);
                    self.change_state(MinerState::GamesUpdate);
                }
                MinerState::GamesUpdate => {
                    // claim anything ready from active and ended campaigns
                    let claimable: Vec<String> = {
                        let s = self.s();
                        let n = now();
                        s.inventory
                            .iter()
                            .filter(|c| !c.upcoming(n))
                            .flat_map(|c| {
                                c.drops
                                    .iter()
                                    .filter(|d| c.can_claim(d, n))
                                    .map(|d| d.id.clone())
                            })
                            .collect()
                    };
                    for id in claimable {
                        self.claim(&id).await;
                    }
                    self.pick_wanted_games();
                    full_cleanup = true;
                    self.restart_watching();
                    self.change_state(MinerState::ChannelsCleanup);
                }
                MinerState::ChannelsCleanup => {
                    self.status("Cleaning up channels…");
                    let (removed, has_wanted) = {
                        let mut s = self.s();
                        let remove: Vec<u64> = if s.wanted.is_empty() || full_cleanup {
                            s.channels.keys().copied().collect()
                        } else {
                            s.channels
                                .values()
                                .filter(|c| {
                                    !c.acl_based
                                        && (c.offline()
                                            || c.game().is_none_or(|g| !s.wanted.contains(g)))
                                })
                                .map(|c| c.id)
                                .collect()
                        };
                        for id in &remove {
                            s.channels.shift_remove(id);
                            if let Some(h) = s.pending.remove(id) {
                                h.abort();
                            }
                        }
                        (remove, !s.wanted.is_empty())
                    };
                    full_cleanup = false;
                    self.unsubscribe_channels(&removed);
                    self.bump();
                    if has_wanted {
                        self.change_state(MinerState::ChannelsFetch);
                    } else {
                        self.log("No active campaigns to mine drops for.");
                        self.change_state(MinerState::Idle);
                    }
                }
                MinerState::ChannelsFetch => {
                    self.status("Gathering channels…");
                    self.fetch_channels().await?;
                    self.change_state(MinerState::ChannelSwitch);
                }
                MinerState::ChannelSwitch => {
                    self.status("Switching channel…");
                    let n = now();
                    let (target, keep) = {
                        let mut s = self.s();
                        if s.pinned.is_some_and(|p| !s.channels.contains_key(&p)) {
                            s.pinned = None;
                        }
                        let pinned = s
                            .pinned
                            .and_then(|p| s.channels.get(&p))
                            .filter(|c| s.can_watch(c, n))
                            .map(|c| c.id);
                        let target = pinned.or_else(|| {
                            let mut order: Vec<&Channel> = s.channels.values().collect();
                            order.sort_by_key(|c| s.priority(c));
                            order
                                .into_iter()
                                .find(|c| s.should_switch(c, n))
                                .map(|c| c.id)
                        });
                        let keep = s
                            .watching
                            .and_then(|w| s.channels.get(&w))
                            .filter(|c| s.can_watch(c, n))
                            .map(|c| c.name().to_string());
                        (target, keep)
                    };
                    if let Some(id) = target {
                        self.watch(id, true);
                        self.state_change.clear();
                    } else if let Some(name) = keep {
                        self.status(format!("Watching {name}"));
                        self.state_change.clear();
                    } else {
                        self.log("No channel to watch right now. Waiting for one to go live…");
                        self.change_state(MinerState::Idle);
                    }
                }
                MinerState::Restart => return Ok(Flow::Restart),
                MinerState::Exit => {
                    self.icon(TrayIcon::Pickaxe);
                    self.status("Exiting…");
                    return Ok(Flow::Exit);
                }
            }
            self.state_change.wait().await;
        }
    }

    // ---- sign-in -------------------------------------------------------------------------------------------

    async fn sign_in(&self, http: &Http) -> Result<()> {
        self.status("Signing in…");
        let mut session = self.s().session.clone();
        if session.device_id.is_empty() {
            session.device_id = http.fetch_device_id().await?;
        }
        for _ in 0..4 {
            if session.access_token.is_empty() {
                session.access_token = self.ask_for_token().await?;
            }
            http.set_auth(&session.device_id, &session.access_token);
            match http.validate(&session.access_token).await? {
                None => {
                    self.log("The saved sign-in has expired.");
                    session.access_token.clear();
                }
                Some(v) if v["client_id"].as_str() != Some(CLIENT.client_id) => {
                    self.log("The saved sign-in was for a different app; signing in again.");
                    session.access_token.clear();
                }
                Some(v) => {
                    session.user_id = id_of(&v["user_id"]).unwrap_or(0);
                    session.login = v["login"].as_str().unwrap_or("").to_string();
                    {
                        let mut s = self.s();
                        s.session = session.clone();
                        s.signed_in = true;
                        s.login_url = None;
                        let _ = s.session.save(&self.dir);
                    }
                    self.log(format!(
                        "Signed in as {} ({}).",
                        session.login, session.user_id
                    ));
                    self.bump();
                    return Ok(());
                }
            }
        }
        Err(Error::Auth("Couldn't sign in".into()))
    }

    async fn ask_for_token(&self) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        *self.token_tx.lock().unwrap() = Some(tx);
        let url = login_url();
        self.s().login_url = Some(url.clone());
        self.status("Waiting for you to sign in to Twitch");
        self.log("Sign in to Twitch to start mining.");
        self.sink.login_needed(&url);
        self.bump();
        tokio::select! {
            t = rx => {
                self.s().login_url = None;
                t.map_err(|_| Error::Exit)
            }
            _ = self.exit_event.wait() => Err(Error::Exit),
        }
    }

    // ---- inventory -----------------------------------------------------------------------------------------

    async fn fetch_inventory(self: &Arc<Self>) -> Result<()> {
        let http = self.http()?;
        self.status("Loading your drops inventory…");
        let inv = http.gql(gql("Inventory")).await?;
        let inventory = &inv["data"]["currentUser"]["inventory"];
        let mut claimed_benefits: HashMap<String, DateTime<Utc>> = HashMap::new();
        for b in inventory["gameEventDrops"].as_array().into_iter().flatten() {
            if let (Some(id), Some(at)) = (
                b["id"].as_str(),
                b["lastAwardedAt"].as_str().and_then(crate::util::parse_ts),
            ) {
                claimed_benefits.insert(id.to_string(), at);
            }
        }
        let mut data: IndexMap<String, Value> = IndexMap::new();
        for c in inventory["dropCampaignsInProgress"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = c["id"].as_str() {
                data.insert(id.to_string(), c.clone());
            }
        }
        self.status("Loading campaigns…");
        let camps = http.gql(gql("Campaigns")).await?;
        let available: Vec<(String, Value)> = camps["data"]["currentUser"]["dropCampaigns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| matches!(c["status"].as_str(), Some("ACTIVE") | Some("UPCOMING")))
            .filter_map(|c| Some((c["id"].as_str()?.to_string(), c.clone())))
            .collect();
        let user_id = self.s().session.user_id;
        let mut jobs = Vec::new();
        for batch in chunks(&available, 20) {
            let http = http.clone();
            jobs.push(tokio::spawn(async move {
                let ops: Vec<Value> = batch
                    .iter()
                    .map(|(id, _)| {
                        gql_with(
                            "CampaignDetails",
                            json!({ "channelLogin": user_id.to_string(), "dropID": id }),
                        )
                    })
                    .collect();
                let resp = http.gql(Value::Array(ops)).await?;
                let mut out: Vec<(String, Value)> = Vec::new();
                let base: HashMap<String, Value> = batch.into_iter().collect();
                for r in resp.as_array().into_iter().flatten() {
                    let details = &r["data"]["user"]["dropCampaign"];
                    if let Some(id) = details["id"].as_str() {
                        let merged = merge_data(base.get(id).unwrap_or(&Value::Null), details);
                        out.push((id.to_string(), merged));
                    }
                }
                Ok::<_, Error>(out)
            }));
        }
        for job in jobs {
            let chunk = job.await.map_err(|e| Error::Http(e.to_string()))??;
            for (id, v) in chunk {
                let merged = match data.get(&id) {
                    Some(existing) => merge_data(existing, &v),
                    None => v,
                };
                data.insert(id, merged);
            }
        }
        let badges = self.s().settings.enable_badges_emotes;
        let n = now();
        let mut campaigns: Vec<Campaign> = data
            .values()
            .filter(|c| !c["game"].is_null())
            .filter_map(|c| Campaign::from_json(c, &claimed_benefits, badges))
            .collect();
        // eligible first, then by end (or start, for upcoming), then active first
        campaigns.sort_by(|a, b| {
            let key = |c: &Campaign| {
                if c.upcoming(n) {
                    c.starts_at
                } else {
                    c.ends_at
                }
            };
            b.eligible()
                .cmp(&a.eligible())
                .then(key(a).cmp(&key(b)))
                .then(b.active(n).cmp(&a.active(n)))
        });
        let next_hour = n + chrono::Duration::hours(1);
        {
            let mut s = self.s();
            s.drop_index.clear();
            s.campaign_index.clear();
            let mut triggers: Vec<DateTime<Utc>> = Vec::new();
            for (ci, c) in campaigns.iter().enumerate() {
                for (di, d) in c.drops.iter().enumerate() {
                    s.drop_index.insert(d.id.clone(), (ci, di));
                }
                s.campaign_index.insert(c.id.clone(), ci);
                if c.can_earn_within(next_hour, n) {
                    triggers.extend(c.time_triggers());
                }
            }
            triggers.sort();
            triggers.dedup();
            s.mnt_triggers = triggers.into_iter().filter(|t| *t > n).collect();
            s.inventory = campaigns;
        }
        let count = self.s().inventory.len();
        self.log(format!("Loaded {count} campaigns."));
        self.bump();
        // the maintenance timer restarts with every inventory load
        if let Some(t) = self.mnt_task.lock().unwrap().take() {
            t.abort();
        }
        let me = self.clone();
        *self.mnt_task.lock().unwrap() = Some(tokio::spawn(async move { me.maintenance().await }));
        Ok(())
    }

    /// Which games to mine, best first (GAMES_UPDATE).
    fn pick_wanted_games(&self) {
        let mut s = self.s();
        let n = now();
        let next_hour = n + chrono::Duration::hours(1);
        let priority = s.settings.priority.clone();
        let exclude = s.settings.exclude.clone();
        let mode = s.settings.priority_mode;
        let mut order: Vec<usize> = (0..s.inventory.len()).collect();
        match mode {
            PriorityMode::EndingSoonest => order.sort_by_key(|i| s.inventory[*i].ends_at),
            PriorityMode::LowAvailability => order.sort_by(|a, b| {
                s.inventory[*a]
                    .availability(n)
                    .partial_cmp(&s.inventory[*b].availability(n))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            PriorityMode::PriorityOnly => {}
        }
        order.sort_by_key(|i| {
            priority
                .iter()
                .position(|p| *p == s.inventory[*i].game.name)
                .unwrap_or(usize::MAX)
        });
        let mut wanted: Vec<Game> = Vec::new();
        for i in order {
            let c = &s.inventory[i];
            if !wanted.contains(&c.game)
                && !exclude.contains(&c.game.name)
                && (mode != PriorityMode::PriorityOnly || priority.contains(&c.game.name))
                && c.can_earn_within(next_hour, n)
            {
                wanted.push(c.game.clone());
            }
        }
        s.wanted = wanted;
    }

    async fn maintenance(self: Arc<Self>) {
        let end = now() + chrono::Duration::hours(1);
        loop {
            let n = now();
            if n >= end {
                break;
            }
            let mut next = end;
            {
                let mut s = self.s();
                while s.mnt_triggers.front().is_some_and(|t| *t <= next) {
                    next = s.mnt_triggers.pop_front().unwrap();
                }
            }
            let secs = (next - n).num_milliseconds().max(0) as u64;
            tokio::time::sleep(Duration::from_millis(secs)).await;
            if now() >= end {
                break;
            }
            if next != end {
                self.change_state(MinerState::ChannelsCleanup);
            }
        }
        // hourly: reload everything
        self.change_state(MinerState::InventoryFetch);
    }

    // ---- channels ------------------------------------------------------------------------------------------

    fn unsubscribe_channels(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let topics: Vec<String> = ids
            .iter()
            .flat_map(|id| {
                [
                    topic(TOPIC_STREAM_STATE, *id),
                    topic(TOPIC_STREAM_UPDATE, *id),
                ]
            })
            .collect();
        if let Some(p) = self.pubsub.read().unwrap().as_ref() {
            p.remove_topics(topics);
        }
    }

    async fn fetch_channels(&self) -> Result<()> {
        let n = now();
        let next_hour = n + chrono::Duration::hours(1);
        let (mut list, acl, no_acl) = {
            let mut s = self.s();
            let existing: Vec<Channel> = s.channels.values().cloned().collect();
            s.channels.clear();
            let mut acl: Vec<Channel> = Vec::new();
            let mut no_acl: Vec<Game> = Vec::new();
            for c in &s.inventory {
                if s.wanted.contains(&c.game) && c.can_earn_within(next_hour, n) {
                    if c.allowed_channels.is_empty() {
                        if !no_acl.contains(&c.game) {
                            no_acl.push(c.game.clone());
                        }
                    } else {
                        for a in &c.allowed_channels {
                            if !acl.iter().any(|x| x.id == a.id)
                                && !existing.iter().any(|x| x.id == a.id)
                            {
                                acl.push(Channel::from_acl(a));
                            }
                        }
                    }
                }
            }
            (existing, acl, no_acl)
        };
        self.bump();
        let acl = self.bulk_check_online(acl).await?;
        for ch in acl {
            if !list.iter().any(|x| x.id == ch.id) {
                list.push(ch);
            }
        }
        for game in no_acl {
            for ch in self.live_streams(&game).await? {
                if !list.iter().any(|x| x.id == ch.id) {
                    list.push(ch);
                }
            }
        }
        let n = now();
        let trimmed: Vec<u64> = {
            let mut s = self.s();
            // by game priority, then allow-listed first, then most viewers (online first)
            list.sort_by(|a, b| {
                s.priority(a)
                    .cmp(&s.priority(b))
                    .then(b.acl_based.cmp(&a.acl_based))
                    .then(b.viewers().unwrap_or(-1).cmp(&a.viewers().unwrap_or(-1)))
            });
            let trimmed: Vec<u64> = list.iter().skip(MAX_CHANNELS).map(|c| c.id).collect();
            list.truncate(MAX_CHANNELS);
            for ch in list {
                s.channels.insert(ch.id, ch);
            }
            trimmed
        };
        self.unsubscribe_channels(&trimmed);
        let ids: Vec<u64> = self.s().channels.keys().copied().collect();
        if let Some(p) = self.pubsub.read().unwrap().as_ref() {
            p.add_topics(
                ids.iter()
                    .flat_map(|id| {
                        [
                            topic(TOPIC_STREAM_STATE, *id),
                            topic(TOPIC_STREAM_UPDATE, *id),
                        ]
                    })
                    .collect(),
            );
        }
        // keep watching the same channel if it still qualifies
        let (keep, preview) = {
            let s = self.s();
            let keep = s
                .watching
                .map(|w| s.channels.get(&w).is_some_and(|c| s.can_watch(c, n)));
            // show the drop that's about to be mined
            let mut preview = None;
            for ch in s.channels.values() {
                if s.can_watch(ch, n) {
                    if let Some(ci) = s.active_campaign(Some(ch.id), n) {
                        if let Some(di) = s.inventory[ci].first_drop(n) {
                            preview = Some(s.inventory[ci].drops[di].id.clone());
                        }
                    }
                    break;
                }
            }
            (keep, preview)
        };
        if keep == Some(false) {
            self.stop_watching();
        }
        if let Some(id) = preview {
            self.display(&id, false, true);
        }
        let count = self.s().channels.len();
        self.log(format!("Following {count} channels."));
        self.bump();
        Ok(())
    }

    async fn live_streams(&self, game: &Game) -> Result<Vec<Channel>> {
        let http = self.http()?;
        let op = gql_with(
            "GameDirectory",
            json!({ "limit": 20, "slug": game.slug, "options": { "includeRestricted": ["SUB_ONLY_LIVE"], "systemFilters": ["DROPS_ENABLED"] } }),
        );
        let resp = match http.gql(op).await {
            Ok(r) => r,
            Err(Error::Gql(e)) => {
                self.log(format!("Couldn't list channels for {}: {e}", game.name));
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        Ok(resp["data"]["game"]["streams"]["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| !e["node"]["broadcaster"].is_null())
            .filter_map(|e| Channel::from_directory(&e["node"], true))
            .collect())
    }

    /// Checks a batch of (allow-listed) channels for being live, 20 at a time.
    async fn bulk_check_online(&self, mut channels: Vec<Channel>) -> Result<Vec<Channel>> {
        if channels.is_empty() {
            return Ok(channels);
        }
        let http = self.http()?;
        let mut users: HashMap<u64, Value> = HashMap::new();
        for batch in channels.chunks(20) {
            let ops: Vec<Value> = batch
                .iter()
                .map(|c| gql_with("GetStreamInfo", json!({ "channel": c.login })))
                .collect();
            let resp = http.gql(Value::Array(ops)).await?;
            for r in resp.as_array().into_iter().flatten() {
                let u = &r["data"]["user"];
                if let Some(id) = id_of(&u["id"]) {
                    users.insert(id, u.clone());
                }
            }
        }
        let check = self.s().settings.available_drops_check;
        let mut available: HashMap<u64, Vec<Value>> = HashMap::new();
        if check {
            let online: Vec<u64> = users
                .iter()
                .filter(|(_, u)| u["stream"].is_object())
                .map(|(id, _)| *id)
                .collect();
            for batch in online.chunks(20) {
                let ops: Vec<Value> = batch
                    .iter()
                    .map(|id| gql_with("AvailableDrops", json!({ "channelID": id.to_string() })))
                    .collect();
                let resp = http.gql(Value::Array(ops)).await?;
                for r in resp.as_array().into_iter().flatten() {
                    let c = &r["data"]["channel"];
                    if let Some(id) = id_of(&c["id"]) {
                        available.insert(
                            id,
                            c["viewerDropCampaigns"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default(),
                        );
                    }
                }
            }
        }
        let n = now();
        for ch in channels.iter_mut() {
            let Some(u) = users.get(&ch.id) else { continue };
            if ch.display_name.is_none() {
                ch.display_name = u["displayName"].as_str().map(str::to_string);
            }
            let Some(stream) = Stream::from_get_stream(u, !check) else {
                continue;
            };
            ch.stream = Some(stream);
            if !ch.drops_enabled() {
                let list = available.get(&ch.id).cloned().unwrap_or_default();
                let enabled = self.s().drops_enabled_from(ch, &list, n);
                if let Some(st) = ch.stream.as_mut() {
                    st.drops_enabled = enabled;
                }
            }
        }
        Ok(channels)
    }

    /// Fetches a channel's live stream (None when offline), checking drops when needed (Channel.get_stream).
    async fn get_stream(&self, id: u64) -> Result<Option<Stream>> {
        let http = self.http()?;
        let Some(login) = self.s().channels.get(&id).map(|c| c.login.clone()) else {
            return Ok(None);
        };
        let resp = http
            .gql(gql_with("GetStreamInfo", json!({ "channel": login })))
            .await?;
        let u = &resp["data"]["user"];
        if !u.is_object() {
            return Ok(None);
        }
        let check = self.s().settings.available_drops_check;
        {
            let mut s = self.s();
            if let Some(ch) = s.channels.get_mut(&id) {
                if ch.display_name.is_none() {
                    ch.display_name = u["displayName"].as_str().map(str::to_string);
                }
            }
        }
        let Some(mut stream) = Stream::from_get_stream(u, !check) else {
            return Ok(None);
        };
        if !stream.drops_enabled {
            if let Ok(r) = http
                .gql(gql_with(
                    "AvailableDrops",
                    json!({ "channelID": id.to_string() }),
                ))
                .await
            {
                let list = r["data"]["channel"]["viewerDropCampaigns"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let s = self.s();
                if let Some(ch) = s.channels.get(&id) {
                    let mut probe = ch.clone();
                    probe.stream = Some(stream.clone());
                    stream.drops_enabled = s.drops_enabled_from(&probe, &list, now());
                }
            }
        }
        Ok(Some(stream))
    }

    async fn update_stream(&self, id: u64) {
        match self.get_stream(id).await {
            Ok(after) => {
                let before = {
                    let mut s = self.s();
                    match s.channels.get_mut(&id) {
                        Some(ch) => {
                            ch.pending_online = false;
                            std::mem::replace(&mut ch.stream, after.clone())
                        }
                        None => return,
                    }
                };
                self.on_channel_update(id, before, after);
            }
            Err(e) => self.log(format!("Couldn't check a channel's stream: {e}")),
        }
    }

    /// A "stream-up" or settings change: look again in a couple of minutes (Channel.check_online).
    fn check_online(self: &Arc<Self>, id: u64) {
        let mut s = self.s();
        if s.pending.contains_key(&id) {
            return;
        }
        let Some(ch) = s.channels.get_mut(&id) else {
            return;
        };
        ch.pending_online = true;
        let me = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ONLINE_DELAY_SECS)).await;
            me.s().pending.remove(&id);
            me.update_stream(id).await;
        });
        s.pending.insert(id, handle.abort_handle());
        drop(s);
        self.sink.changed();
    }

    fn set_offline(&self, id: u64) {
        let before = {
            let mut s = self.s();
            if let Some(h) = s.pending.remove(&id) {
                h.abort();
            }
            let Some(ch) = s.channels.get_mut(&id) else {
                return;
            };
            ch.pending_online = false;
            ch.stream.take()
        };
        if before.is_some() {
            self.on_channel_update(id, before, None);
        } else {
            self.sink.changed();
        }
    }

    /// A channel went up, down, or changed (Twitch.on_channel_update).
    fn on_channel_update(&self, id: u64, before: Option<Stream>, after: Option<Stream>) {
        let n = now();
        enum Act {
            Watch,
            Switch,
            Nothing,
        }
        let (act, msg) = {
            let s = self.s();
            let Some(ch) = s.channels.get(&id) else {
                return;
            };
            let name = ch.name().to_string();
            match (&before, &after) {
                (None, Some(_)) => {
                    if s.should_switch(ch, n) {
                        (
                            Act::Watch,
                            Some(format!("{name} went live, switching to it.")),
                        )
                    } else {
                        (Act::Nothing, None)
                    }
                }
                (None, None) => (Act::Nothing, None),
                (Some(_), after) => {
                    if s.watching == Some(id) {
                        if !s.can_watch(ch, n) {
                            let msg = if after.is_none() {
                                format!("{name} went offline, switching…")
                            } else {
                                format!("{name} can't be mined any more, switching…")
                            };
                            (Act::Switch, Some(msg))
                        } else {
                            (Act::Nothing, None)
                        }
                    } else if after.is_some() && s.should_switch(ch, n) {
                        (Act::Watch, None)
                    } else {
                        (Act::Nothing, None)
                    }
                }
            }
        };
        if let Some(m) = msg {
            self.log(m);
        }
        match act {
            Act::Watch => self.watch(id, true),
            Act::Switch => self.change_state(MinerState::ChannelSwitch),
            Act::Nothing => {}
        }
        self.sink.changed();
    }

    // ---- watching ------------------------------------------------------------------------------------------

    fn watch(&self, id: u64, update_status: bool) {
        let name = {
            let mut s = self.s();
            s.watching = Some(id);
            s.channels
                .get(&id)
                .map(|c| c.name().to_string())
                .unwrap_or_default()
        };
        self.watching_set.set();
        self.icon(TrayIcon::Active);
        if update_status {
            self.log(format!("Watching {name}"));
            self.status(format!("Watching {name}"));
        }
        self.bump();
    }

    fn stop_watching(&self) {
        {
            let mut s = self.s();
            s.watching = None;
            s.display = None;
        }
        self.watching_set.clear();
        self.bump();
    }

    fn restart_watching(&self) {
        if let Some(d) = self.s().display.as_mut() {
            d.timer = None;
            d.timer_wall_ms = None;
        }
        self.watching_restart.set();
    }

    /// Shows a drop as the one being mined; `countdown` starts the minute timer.
    fn display(&self, drop_id: &str, countdown: bool, subone: bool) {
        let tip = {
            let mut s = self.s();
            let remaining = s
                .drop_index
                .get(drop_id)
                .map(|(c, d)| s.inventory[*c].drops[*d].remaining_minutes())
                .unwrap_or(0);
            let run = countdown && remaining > 0;
            s.display = Some(DropDisplay {
                drop_id: drop_id.to_string(),
                timer: run.then(Instant::now),
                timer_wall_ms: run.then(|| Utc::now().timestamp_millis()),
                subone,
            });
            tray_tooltip(&s)
        };
        let icon = self.s().icon;
        self.sink.tray(icon, &tip);
        self.bump();
    }

    fn minute_almost_done(&self) -> bool {
        match self.s().display.as_ref().and_then(|d| d.timer) {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_secs(50),
        }
    }

    fn display_first_drop(&self, ci: usize) {
        let id = {
            let s = self.s();
            s.inventory
                .get(ci)
                .and_then(|c| c.first_drop(now()).map(|d| c.drops[d].id.clone()))
        };
        if let Some(id) = id {
            self.display(&id, true, false);
        }
    }

    async fn watch_loop(self: Arc<Self>) {
        let interval = Duration::from_secs(WATCH_INTERVAL_SECS);
        loop {
            self.watching_set.wait().await;
            let Some((id, online)) = ({
                let s = self.s();
                s.watching
                    .map(|id| (id, s.channels.get(&id).is_some_and(|c| c.online())))
            }) else {
                self.watching_set.clear();
                continue;
            };
            if !online {
                self.stop_watching();
                continue;
            }
            let sent = Instant::now();
            match self.send_watch(id).await {
                Ok(true) => {}
                Ok(false) => {}
                Err(Error::Exit) => return,
                Err(e) => self.log(format!("Watching failed: {e}")),
            }
            tokio::time::sleep(Duration::from_secs(15).saturating_sub(sent.elapsed())).await;
            if self.minute_almost_done() {
                // Twitch hasn't reported progress for a while: ask for it, or as a last resort, guess a minute
                let mut handled = false;
                if let Ok(http) = self.http() {
                    if let Ok(r) = http
                        .gql(gql_with(
                            "CurrentDrop",
                            json!({ "channelID": id.to_string() }),
                        ))
                        .await
                    {
                        let d = &r["data"]["currentUser"]["dropCurrentSession"];
                        if let (Some(drop_id), Some(minutes)) =
                            (d["dropID"].as_str(), d["currentMinutesWatched"].as_i64())
                        {
                            handled = self.update_drop_minutes(drop_id, minutes, true);
                        }
                    }
                }
                if !handled {
                    let n = now();
                    let bumped = {
                        let mut s = self.s();
                        match s.active_campaign(Some(id), n) {
                            Some(ci) => {
                                let ch = s.channels.get(&id).cloned();
                                let hit = ch
                                    .map(|c| s.inventory[ci].bump_minutes(&c.as_ref(), n))
                                    .unwrap_or(false);
                                Some((ci, hit))
                            }
                            None => None,
                        }
                    };
                    if let Some((ci, hit)) = bumped {
                        if hit {
                            self.log("Twitch stopped reporting progress for this drop; trying another channel.");
                            self.change_state(MinerState::ChannelSwitch);
                        }
                        self.display_first_drop(ci);
                    }
                }
            }
            self.watching_restart.clear();
            self.watching_restart
                .wait_for(interval.saturating_sub(sent.elapsed()))
                .await;
        }
    }

    /// Updates a drop's minutes from Twitch. `check_channel`: only if it can be earned on the watched channel.
    fn update_drop_minutes(&self, drop_id: &str, minutes: i64, check_channel: bool) -> bool {
        let n = now();
        let ci = {
            let mut s = self.s();
            let Some((ci, di)) = s.drop_index.get(drop_id).copied() else {
                return false;
            };
            if check_channel {
                let ok = {
                    let ch = s.watching.and_then(|w| s.channels.get(&w));
                    let r = ch.map(|c| c.as_ref());
                    let c = &s.inventory[ci];
                    c.drop_can_earn(&c.drops[di], r.as_ref(), false, n)
                };
                if !ok {
                    return false;
                }
            }
            s.inventory[ci].update_minutes(di, minutes);
            ci
        };
        self.display_first_drop(ci);
        true
    }

    /// "Watches" a stream: asks for its playlist and touches the newest video chunks (Channel.send_watch).
    async fn send_watch(&self, id: u64) -> Result<bool> {
        let http = self.http()?;
        let (login, cached) = {
            let s = self.s();
            let Some(ch) = s.channels.get(&id) else {
                return Ok(false);
            };
            let Some(st) = &ch.stream else {
                return Ok(false);
            };
            (ch.login.clone(), st.stream_url.clone())
        };
        let url = match cached {
            Some(u) => u,
            None => {
                let r = http
                    .gql(gql_with("PlaybackAccessToken", json!({ "login": login })))
                    .await?;
                let tok = &r["data"]["streamPlaybackAccessToken"];
                let (Some(value), Some(sig)) = (tok["value"].as_str(), tok["signature"].as_str())
                else {
                    return Ok(false);
                };
                let mut usher = reqwest::Url::parse(&format!(
                    "https://usher.ttvnw.net/api/channel/hls/{login}.m3u8"
                ))
                .unwrap();
                usher
                    .query_pairs_mut()
                    .append_pair("sig", sig)
                    .append_pair("token", value);
                let resp = http
                    .request(Method::GET, usher.as_str(), None, None)
                    .await?;
                if let Ok(j) = serde_json::from_str::<Value>(&resp.text) {
                    let j = if let Value::Array(a) = &j {
                        a.first().cloned().unwrap_or(Value::Null)
                    } else {
                        j
                    };
                    if let Some(err) = j.get("error") {
                        self.log(format!("Couldn't open the stream: {err}"));
                        self.set_offline(id);
                    }
                    return Ok(false);
                }
                let Some(last) = resp.text.trim().lines().last().map(str::to_string) else {
                    return Ok(false);
                };
                if let Some(st) = self
                    .s()
                    .channels
                    .get_mut(&id)
                    .and_then(|c| c.stream.as_mut())
                {
                    st.stream_url = Some(last.clone());
                }
                last
            }
        };
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("close"));
        let resp = http.request(Method::GET, &url, Some(h), None).await?;
        if resp.status >= 400 {
            return Ok(false);
        }
        let re = regex::Regex::new(r#""url": ?".+}","#).unwrap();
        let text = re.replace_all(&resp.text, "").into_owned();
        if serde_json::from_str::<Value>(&text).is_ok() {
            return Ok(false);
        }
        for chunk in text.trim().lines().filter(|l| l.starts_with("http")) {
            let r = http.request(Method::HEAD, chunk, None, None).await?;
            if r.status != 200 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ---- claiming ------------------------------------------------------------------------------------------

    async fn claim(&self, drop_id: &str) -> bool {
        let Ok(http) = self.http() else { return false };
        let claim_id = {
            let s = self.s();
            let Some((ci, di)) = s.drop_index.get(drop_id).copied() else {
                return false;
            };
            let c = &s.inventory[ci];
            let d = &c.drops[di];
            if d.is_claimed {
                return true;
            }
            if !c.can_claim(d, now()) {
                return false;
            }
            d.claim_id.clone().unwrap_or_default()
        };
        let ok = match http
            .gql(gql_with(
                "ClaimDrop",
                json!({ "input": { "dropInstanceID": claim_id } }),
            ))
            .await
        {
            Ok(r) => {
                let data = &r["data"];
                matches!(
                    data["claimDropRewards"]["status"].as_str(),
                    Some("ELIGIBLE_FOR_ALL") | Some("DROP_INSTANCE_ALREADY_CLAIMED")
                )
            }
            Err(_) => false,
        };
        if !ok {
            self.log(format!("A drop claim may have failed (drop {drop_id})."));
            return false;
        }
        let (text, notify) = {
            let mut s = self.s();
            let Some((ci, di)) = s.drop_index.get(drop_id).copied() else {
                return true;
            };
            let c = &mut s.inventory[ci];
            let d = &mut c.drops[di];
            d.is_claimed = true;
            d.real_minutes = d.required_minutes;
            d.extra_minutes = 0;
            let rewards = d.rewards_text();
            let text = format!(
                "{}: {} ({}/{})",
                c.game.name,
                rewards,
                c.claimed_drops(),
                c.total_drops()
            );
            (text, s.settings.notifications)
        };
        self.log(format!("Claimed drop: {text}"));
        if notify {
            self.sink.notify("Drop claimed", &text);
        }
        self.bump();
        true
    }

    // ---- PubSub messages -----------------------------------------------------------------------------------

    async fn on_pubsub(self: Arc<Self>, t: String, msg: Value) {
        let Some((name, target)) = t.rsplit_once('.') else {
            return;
        };
        let Ok(target) = target.parse::<u64>() else {
            return;
        };
        match name {
            TOPIC_STREAM_STATE => match msg["type"].as_str().unwrap_or("") {
                "viewcount" => {
                    let online = self.s().channels.get(&target).map(|c| c.online());
                    match online {
                        Some(false) => self.check_online(target),
                        Some(true) => {
                            if let Some(v) = msg["viewers"].as_i64() {
                                if let Some(st) = self
                                    .s()
                                    .channels
                                    .get_mut(&target)
                                    .and_then(|c| c.stream.as_mut())
                                {
                                    st.viewers = v;
                                }
                                self.sink.changed();
                            }
                        }
                        None => {}
                    }
                }
                "stream-down" => self.set_offline(target),
                "stream-up" => self.check_online(target),
                _ => {}
            },
            TOPIC_STREAM_UPDATE => self.check_online(target),
            TOPIC_USER_DROPS => self.on_drop_event(msg).await,
            TOPIC_USER_NOTIFICATIONS if msg["type"] == "create-notification" => {
                let n = &msg["data"]["notification"];
                if matches!(
                    n["type"].as_str(),
                    Some("user_drop_reward_reminder_notification")
                        | Some("quests_viewer_reward_campaign_earned_emote")
                ) {
                    self.change_state(MinerState::InventoryFetch);
                    if let (Ok(http), Some(id)) = (self.http(), n["id"].as_str()) {
                        let _ = http
                            .gql(gql_with(
                                "NotificationsDelete",
                                json!({ "input": { "id": id } }),
                            ))
                            .await;
                    }
                }
            }
            _ => {}
        }
    }

    async fn on_drop_event(&self, msg: Value) {
        let kind = msg["type"].as_str().unwrap_or("");
        let drop_id = msg["data"]["drop_id"].as_str().unwrap_or("").to_string();
        match kind {
            "drop-progress" => {
                if let Some(m) = msg["data"]["current_progress_min"].as_i64() {
                    self.update_drop_minutes(&drop_id, m, true);
                }
            }
            "drop-claim" => {
                let ci = {
                    let mut s = self.s();
                    let Some((ci, di)) = s.drop_index.get(&drop_id).copied() else {
                        drop(s);
                        self.log(format!(
                            "Twitch offered a claim for an unknown drop ({drop_id})"
                        ));
                        return;
                    };
                    s.inventory[ci].drops[di].claim_id =
                        msg["data"]["drop_instance_id"].as_str().map(str::to_string);
                    ci
                };
                self.claim(&drop_id).await;
                self.display(&drop_id, true, false);
                // the next drop starts a few seconds after the claim
                tokio::time::sleep(Duration::from_secs(4)).await;
                let watching = self.s().watching;
                if let (Some(w), Ok(http)) = (watching, self.http()) {
                    for _ in 0..8 {
                        match http
                            .gql(gql_with(
                                "CurrentDrop",
                                json!({ "channelID": w.to_string() }),
                            ))
                            .await
                        {
                            Ok(r) => {
                                let d = &r["data"]["currentUser"]["dropCurrentSession"];
                                if d.is_null() || d["dropID"].as_str() != Some(drop_id.as_str()) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                let can = {
                    let s = self.s();
                    let ch = s.watching.and_then(|w| s.channels.get(&w));
                    let r = ch.map(|c| c.as_ref());
                    s.inventory
                        .get(ci)
                        .is_some_and(|c| c.can_earn(r.as_ref(), false, now()))
                };
                if can {
                    self.restart_watching();
                } else {
                    self.change_state(MinerState::InventoryFetch);
                }
            }
            _ => {}
        }
    }

    // ---- what the window shows -----------------------------------------------------------------------------

    pub fn snapshot(&self) -> Snapshot {
        let pubsub = self
            .pubsub
            .read()
            .unwrap()
            .as_ref()
            .map(|p| p.statuses())
            .unwrap_or_default();
        let s = self.s();
        let n = now();
        let watching = s
            .watching
            .and_then(|w| s.channels.get(&w))
            .map(|c| WatchingView {
                id: c.id,
                name: c.name().to_string(),
                login: c.login.clone(),
                game: c.game().map(|g| g.name.clone()),
                viewers: c.viewers(),
                title: c
                    .stream
                    .as_ref()
                    .map(|st| st.title.clone())
                    .unwrap_or_default(),
                url: c.url(),
            });
        let drop = s.display.as_ref().and_then(|disp| {
            let (ci, di) = s.drop_index.get(&disp.drop_id).copied()?;
            let c = &s.inventory[ci];
            let d = &c.drops[di];
            Some(DropView {
                id: d.id.clone(),
                campaign_id: c.id.clone(),
                campaign: c.name.clone(),
                game: c.game.name.clone(),
                image: c.image_url.clone(),
                name: d.name.clone(),
                rewards: d
                    .benefits
                    .iter()
                    .map(|b| BenefitView {
                        name: b.name.clone(),
                        image: b.image_url.clone(),
                    })
                    .collect(),
                progress: d.progress(),
                current_minutes: d.current_minutes(),
                required_minutes: d.required_minutes,
                remaining_minutes: d.remaining_minutes(),
                campaign_progress: c.progress(),
                campaign_remaining_minutes: c.remaining_minutes(),
                claimed: c.claimed_drops(),
                total: c.total_drops(),
                timer_started_ms: disp.timer_wall_ms,
                subone: disp.subone,
                ends_at: c.ends_at.to_rfc3339(),
            })
        });
        let mut games: Vec<String> = s.inventory.iter().map(|c| c.game.name.clone()).collect();
        games.sort();
        games.dedup();
        Snapshot {
            version: VERSION.into(),
            rev: self.rev.load(Ordering::Relaxed),
            state: s.state,
            status: s.status.clone(),
            icon: s.icon,
            signed_in: s.signed_in,
            user: (s.session.user_id != 0).then(|| UserView {
                id: s.session.user_id,
                login: s.session.login.clone(),
            }),
            login_needed: s.login_url.is_some(),
            watching,
            pinned: s.pinned,
            drop,
            channels: s
                .channels
                .values()
                .map(|c| ChannelView {
                    id: c.id,
                    name: c.name().to_string(),
                    login: c.login.clone(),
                    status: if c.online() {
                        "online"
                    } else if c.pending_online {
                        "pending"
                    } else {
                        "offline"
                    },
                    game: c.game().map(|g| g.name.clone()),
                    viewers: c.viewers(),
                    drops: c.drops_enabled(),
                    acl: c.acl_based,
                    watching: s.watching == Some(c.id),
                    can_watch: s.can_watch(c, n),
                    priority: Some(s.priority(c)).filter(|p| *p != usize::MAX),
                })
                .collect(),
            wanted: s.wanted.iter().map(|g| g.name.clone()).collect(),
            games,
            websockets: pubsub,
            settings: s.settings.clone(),
            log_seq: s.log_seq,
            campaigns: s.inventory.len(),
        }
    }

    pub fn inventory(&self) -> Vec<CampaignView> {
        let s = self.s();
        let n = now();
        s.inventory
            .iter()
            .map(|c| CampaignView {
                id: c.id.clone(),
                name: c.name.clone(),
                game: c.game.name.clone(),
                image: c.image_url.clone(),
                status: if c.active(n) {
                    "active"
                } else if c.upcoming(n) {
                    "upcoming"
                } else {
                    "expired"
                },
                linked: c.linked,
                eligible: c.eligible(),
                link_url: c.link_url.clone(),
                starts_at: c.starts_at.to_rfc3339(),
                ends_at: c.ends_at.to_rfc3339(),
                excluded: s.settings.exclude.contains(&c.game.name),
                priority: s.settings.priority.iter().position(|p| *p == c.game.name),
                wanted: s.wanted.contains(&c.game),
                finished: c.finished(),
                claimed: c.claimed_drops(),
                total: c.total_drops(),
                progress: c.progress(),
                remaining_minutes: c.remaining_minutes(),
                acl: c
                    .allowed_channels
                    .iter()
                    .take(6)
                    .map(|a| a.display_name.clone().unwrap_or_else(|| a.login.clone()))
                    .collect(),
                acl_count: c.allowed_channels.len(),
                drops: c
                    .drops
                    .iter()
                    .map(|d| DropItemView {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        rewards: d
                            .benefits
                            .iter()
                            .map(|b| BenefitView {
                                name: b.name.clone(),
                                image: b.image_url.clone(),
                            })
                            .collect(),
                        progress: d.progress(),
                        current_minutes: d.current_minutes(),
                        required_minutes: d.required_minutes,
                        claimed: d.is_claimed,
                        can_claim: c.can_claim(d, n),
                        earnable: c.drop_can_earn(d, None, false, n),
                        starts_at: d.starts_at.to_rfc3339(),
                        ends_at: d.ends_at.to_rfc3339(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn log_since(&self, seq: u64) -> Vec<LogLine> {
        self.s()
            .log
            .iter()
            .filter(|l| l.seq > seq)
            .cloned()
            .collect()
    }
}

fn tray_tooltip(s: &Shared) -> String {
    let base = "Drops Miner".to_string();
    let Some(disp) = &s.display else { return base };
    let Some((ci, di)) = s.drop_index.get(&disp.drop_id).copied() else {
        return base;
    };
    let c = &s.inventory[ci];
    let d = &c.drops[di];
    let mut t = format!(
        "Drops Miner\n{}\n{} {:.1}%",
        c.game.name,
        d.rewards_text(),
        d.progress() * 100.0
    );
    if t.chars().count() > 127 {
        t = t.chars().take(124).collect::<String>() + "...";
    }
    t
}

/// Merges two campaign records, the first one winning where both have a value (Twitch._merge_data).
pub fn merge_data(primary: &Value, secondary: &Value) -> Value {
    match (primary, secondary) {
        (Value::Object(p), Value::Object(s)) => {
            let mut out = serde_json::Map::new();
            let keys: HashSet<&String> = p.keys().chain(s.keys()).collect();
            for k in keys {
                let v = match (p.get(k), s.get(k)) {
                    (Some(a), Some(b)) if a.is_object() && b.is_object() => merge_data(a, b),
                    (Some(a), _) => a.clone(),
                    (None, Some(b)) => b.clone(),
                    (None, None) => Value::Null,
                };
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        (Value::Null, s) => s.clone(),
        (p, _) => p.clone(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Views for the window

#[derive(Serialize, Clone)]
pub struct UserView {
    pub id: u64,
    pub login: String,
}

#[derive(Serialize, Clone)]
pub struct WatchingView {
    pub id: u64,
    pub name: String,
    pub login: String,
    pub game: Option<String>,
    pub viewers: Option<i64>,
    pub title: String,
    pub url: String,
}

#[derive(Serialize, Clone)]
pub struct BenefitView {
    pub name: String,
    pub image: String,
}

#[derive(Serialize, Clone)]
pub struct DropView {
    pub id: String,
    pub campaign_id: String,
    pub campaign: String,
    pub game: String,
    pub image: String,
    pub name: String,
    pub rewards: Vec<BenefitView>,
    pub progress: f64,
    pub current_minutes: i64,
    pub required_minutes: i64,
    pub remaining_minutes: i64,
    pub campaign_progress: f64,
    pub campaign_remaining_minutes: i64,
    pub claimed: usize,
    pub total: usize,
    pub timer_started_ms: Option<i64>,
    pub subone: bool,
    pub ends_at: String,
}

#[derive(Serialize, Clone)]
pub struct ChannelView {
    pub id: u64,
    pub name: String,
    pub login: String,
    pub status: &'static str,
    pub game: Option<String>,
    pub viewers: Option<i64>,
    pub drops: bool,
    pub acl: bool,
    pub watching: bool,
    pub can_watch: bool,
    pub priority: Option<usize>,
}

#[derive(Serialize, Clone)]
pub struct Snapshot {
    pub version: String,
    pub rev: u64,
    pub state: MinerState,
    pub status: String,
    pub icon: TrayIcon,
    pub signed_in: bool,
    pub user: Option<UserView>,
    pub login_needed: bool,
    pub watching: Option<WatchingView>,
    pub pinned: Option<u64>,
    pub drop: Option<DropView>,
    pub channels: Vec<ChannelView>,
    pub wanted: Vec<String>,
    pub games: Vec<String>,
    pub websockets: Vec<SocketStatus>,
    pub settings: Settings,
    pub log_seq: u64,
    pub campaigns: usize,
}

#[derive(Serialize, Clone)]
pub struct DropItemView {
    pub id: String,
    pub name: String,
    pub rewards: Vec<BenefitView>,
    pub progress: f64,
    pub current_minutes: i64,
    pub required_minutes: i64,
    pub claimed: bool,
    pub can_claim: bool,
    pub earnable: bool,
    pub starts_at: String,
    pub ends_at: String,
}

#[derive(Serialize, Clone)]
pub struct CampaignView {
    pub id: String,
    pub name: String,
    pub game: String,
    pub image: String,
    pub status: &'static str,
    pub linked: bool,
    pub eligible: bool,
    pub link_url: String,
    pub starts_at: String,
    pub ends_at: String,
    pub excluded: bool,
    pub priority: Option<usize>,
    pub wanted: bool,
    pub finished: bool,
    pub claimed: usize,
    pub total: usize,
    pub progress: f64,
    pub remaining_minutes: i64,
    pub acl: Vec<String>,
    pub acl_count: usize,
    pub drops: Vec<DropItemView>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_prefers_primary_and_fills_gaps() {
        let a = json!({ "id": "1", "self": { "isAccountConnected": true }, "timeBasedDrops": [1] });
        let b = json!({ "id": "1", "name": "N", "self": { "isAccountConnected": false, "extra": 2 }, "timeBasedDrops": [9, 9] });
        let m = merge_data(&a, &b);
        assert_eq!(m["name"], "N");
        assert_eq!(m["self"]["isAccountConnected"], true);
        assert_eq!(m["self"]["extra"], 2);
        assert_eq!(m["timeBasedDrops"], json!([1]));
    }
}
