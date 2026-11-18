//! Games, campaigns, drops and channels, and the rules for when a drop can be earned (inventory.py, channel.py).
//! Everything here is plain data plus logic, so it can be tested without Twitch.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::twitch::{slugify, MAX_EXTRA_MINUTES, SPECIAL_GAME_IDS};
use crate::util::parse_ts;

#[derive(Clone, Debug)]
pub struct Game {
    pub id: u64,
    pub name: String,
    pub slug: String,
}

impl PartialEq for Game {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Game {}

impl Game {
    pub fn from_json(v: &Value) -> Option<Game> {
        let id = id_of(&v["id"])?;
        let name = v["displayName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| v["name"].as_str())?
            .to_string();
        let slug = v["slug"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| slugify(&name));
        Some(Game { id, name, slug })
    }
    /// Games whose drops can be earned on any channel.
    pub fn is_special(&self) -> bool {
        SPECIAL_GAME_IDS.contains(&self.id)
    }
}

/// Twitch sends IDs as strings or numbers.
pub fn id_of(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn ts(v: &Value) -> DateTime<Utc> {
    v.as_str()
        .and_then(parse_ts)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BenefitKind {
    Badge,
    Emote,
    DirectEntitlement,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Benefit {
    pub id: String,
    pub name: String,
    pub kind: BenefitKind,
    pub image_url: String,
}

impl Benefit {
    fn from_json(edge: &Value) -> Benefit {
        let b = &edge["benefit"];
        let kind = match b["distributionType"].as_str().unwrap_or("") {
            "BADGE" => BenefitKind::Badge,
            "EMOTE" => BenefitKind::Emote,
            "DIRECT_ENTITLEMENT" => BenefitKind::DirectEntitlement,
            _ => BenefitKind::Unknown,
        };
        Benefit {
            id: b["id"].as_str().unwrap_or("").to_string(),
            name: b["name"].as_str().unwrap_or("").to_string(),
            kind,
            image_url: b["imageAssetURL"].as_str().unwrap_or("").to_string(),
        }
    }
}

/// A timed drop: watch `required_minutes` of the game to earn its benefits.
#[derive(Clone, Debug)]
pub struct Drop {
    pub id: String,
    pub name: String,
    pub benefits: Vec<Benefit>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub claim_id: Option<String>,
    pub is_claimed: bool,
    pub preconditions: Vec<String>,
    pub real_minutes: i64,
    pub extra_minutes: i64,
    pub required_minutes: i64,
}

impl Drop {
    fn from_json(d: &Value, claimed_benefits: &HashMap<String, DateTime<Utc>>) -> Drop {
        let benefits: Vec<Benefit> = d["benefitEdges"]
            .as_array()
            .map(|a| a.iter().map(Benefit::from_json).collect())
            .unwrap_or_default();
        let starts_at = ts(&d["startAt"]);
        let ends_at = ts(&d["endAt"]);
        let mut claim_id = None;
        let mut is_claimed = false;
        let self_edge = &d["self"];
        if self_edge.is_object() {
            claim_id = self_edge["dropInstanceID"].as_str().map(str::to_string);
            is_claimed = self_edge["isClaimed"].as_bool().unwrap_or(false);
        } else {
            // No self edge: if every benefit was awarded while this drop ran, the drop was claimed.
            let dts: Vec<&DateTime<Utc>> = benefits
                .iter()
                .filter_map(|b| claimed_benefits.get(&b.id))
                .collect();
            if !dts.is_empty() && dts.iter().all(|dt| starts_at <= **dt && **dt < ends_at) {
                is_claimed = true;
            }
        }
        let required_minutes = d["requiredMinutesWatched"].as_i64().unwrap_or(0);
        let mut real_minutes = if self_edge.is_object() {
            self_edge["currentMinutesWatched"].as_i64().unwrap_or(0)
        } else {
            0
        };
        if is_claimed {
            // claimed drops may report odd minutes
            real_minutes = required_minutes;
        }
        Drop {
            id: d["id"].as_str().unwrap_or("").to_string(),
            name: d["name"].as_str().unwrap_or("").to_string(),
            benefits,
            starts_at,
            ends_at,
            claim_id,
            is_claimed,
            preconditions: d["preconditionDrops"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            real_minutes,
            extra_minutes: 0,
            required_minutes,
        }
    }

    pub fn current_minutes(&self) -> i64 {
        self.real_minutes + self.extra_minutes
    }
    pub fn remaining_minutes(&self) -> i64 {
        self.required_minutes - self.current_minutes()
    }
    pub fn progress(&self) -> f64 {
        let (c, r) = (self.current_minutes(), self.required_minutes);
        if c <= 0 || r <= 0 {
            0.0
        } else if c >= r {
            1.0
        } else {
            c as f64 / r as f64
        }
    }
    pub fn rewards_text(&self) -> String {
        self.benefits
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn update_real_minutes(&mut self, delta: i64) {
        self.real_minutes = (self.real_minutes + delta).clamp(0, self.required_minutes.max(0));
        self.extra_minutes = 0;
    }
}

/// A channel from a campaign's allow-list.
#[derive(Clone, Debug)]
pub struct AclChannel {
    pub id: u64,
    pub login: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Campaign {
    pub id: String,
    pub name: String,
    pub game: Game,
    pub linked: bool,
    pub link_url: String,
    pub image_url: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub valid: bool,
    pub allowed_channels: Vec<AclChannel>,
    pub drops: Vec<Drop>,
    /// Whether badge and emote campaigns may be mined (a setting, read when the inventory is loaded).
    pub badges_emotes_enabled: bool,
}

/// The parts of a channel the earning rules look at.
pub struct ChannelRef<'a> {
    pub id: u64,
    pub game: Option<&'a Game>,
}

impl Campaign {
    pub fn from_json(
        c: &Value,
        claimed_benefits: &HashMap<String, DateTime<Utc>>,
        badges_emotes_enabled: bool,
    ) -> Option<Campaign> {
        let game = Game::from_json(&c["game"])?;
        let image = c["game"]["boxArtURL"].as_str().unwrap_or("");
        let dims = regex::Regex::new(r"(?i)-\d+x\d+(\.(?:jpg|png|gif))$").unwrap();
        let image_url = dims.replace(image, "$1").into_owned();
        let allow = &c["allow"];
        let enabled = allow["isEnabled"].as_bool().unwrap_or(true);
        let allowed_channels = match allow["channels"].as_array() {
            Some(list) if enabled && !list.is_empty() => list
                .iter()
                .filter_map(|ch| {
                    Some(AclChannel {
                        id: id_of(&ch["id"])?,
                        login: ch["name"].as_str()?.to_string(),
                        display_name: ch["displayName"].as_str().map(str::to_string),
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        let drops = c["timeBasedDrops"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|d| Drop::from_json(d, claimed_benefits))
                    .collect()
            })
            .unwrap_or_default();
        Some(Campaign {
            id: c["id"].as_str()?.to_string(),
            name: c["name"].as_str().unwrap_or("").to_string(),
            game,
            linked: c["self"]["isAccountConnected"].as_bool().unwrap_or(false),
            link_url: c["accountLinkURL"].as_str().unwrap_or("").to_string(),
            image_url,
            starts_at: ts(&c["startAt"]),
            ends_at: ts(&c["endAt"]),
            valid: c["status"].as_str() != Some("EXPIRED"),
            allowed_channels,
            drops,
            badges_emotes_enabled,
        })
    }

    pub fn drop_index(&self, id: &str) -> Option<usize> {
        self.drops.iter().position(|d| d.id == id)
    }
    fn drop_by_id(&self, id: &str) -> Option<&Drop> {
        self.drops.iter().find(|d| d.id == id)
    }

    // ---- campaign state ----
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.valid && self.starts_at <= now && now < self.ends_at
    }
    pub fn upcoming(&self, now: DateTime<Utc>) -> bool {
        self.valid && now < self.starts_at
    }
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        !self.valid || self.ends_at <= now
    }
    pub fn has_badge_or_emote(&self) -> bool {
        self.drops
            .iter()
            .flat_map(|d| &d.benefits)
            .any(|b| matches!(b.kind, BenefitKind::Badge | BenefitKind::Emote))
    }
    pub fn eligible(&self) -> bool {
        if self.has_badge_or_emote() {
            self.badges_emotes_enabled
        } else {
            self.linked
        }
    }
    pub fn finished(&self) -> bool {
        self.drops
            .iter()
            .all(|d| d.is_claimed || d.required_minutes <= 0)
    }
    pub fn claimed_drops(&self) -> usize {
        self.drops.iter().filter(|d| d.is_claimed).count()
    }
    pub fn total_drops(&self) -> usize {
        self.drops.len()
    }
    pub fn progress(&self) -> f64 {
        if self.drops.is_empty() {
            return 0.0;
        }
        self.drops.iter().map(Drop::progress).sum::<f64>() / self.drops.len() as f64
    }
    pub fn remaining_minutes(&self) -> i64 {
        self.drops
            .iter()
            .map(|d| self.total_remaining(d))
            .max()
            .unwrap_or(0)
    }
    pub fn availability(&self, now: DateTime<Utc>) -> f64 {
        self.drops
            .iter()
            .map(|d| self.drop_availability(d, now))
            .fold(f64::INFINITY, f64::min)
    }
    pub fn time_triggers(&self) -> Vec<DateTime<Utc>> {
        let mut t = vec![self.starts_at, self.ends_at];
        for d in &self.drops {
            t.push(d.starts_at);
            t.push(d.ends_at);
        }
        t
    }
    fn preconditions_chain(&self) -> HashSet<&str> {
        self.drops
            .iter()
            .filter(|d| !d.is_claimed)
            .flat_map(|d| d.preconditions.iter().map(String::as_str))
            .collect()
    }

    // ---- per-drop rules ----
    pub fn total_required(&self, d: &Drop) -> i64 {
        d.required_minutes
            + d.preconditions
                .iter()
                .filter_map(|p| self.drop_by_id(p))
                .map(|p| self.total_required(p))
                .max()
                .unwrap_or(0)
    }
    pub fn total_remaining(&self, d: &Drop) -> i64 {
        d.remaining_minutes()
            + d.preconditions
                .iter()
                .filter_map(|p| self.drop_by_id(p))
                .map(|p| self.total_remaining(p))
                .max()
                .unwrap_or(0)
    }
    fn drop_availability(&self, d: &Drop, now: DateTime<Utc>) -> f64 {
        let remaining = self.total_remaining(d);
        if d.required_minutes > 0 && remaining > 0 && now < d.ends_at {
            ((d.ends_at - now).num_seconds() as f64 / 60.0) / remaining as f64
        } else {
            f64::INFINITY
        }
    }
    fn preconditions_met(&self, d: &Drop) -> bool {
        d.preconditions
            .iter()
            .all(|p| self.drop_by_id(p).map(|x| x.is_claimed).unwrap_or(true))
    }
    fn base_earn_conditions(&self, d: &Drop) -> bool {
        self.preconditions_met(d)
            && !d.is_claimed
            && (!d.benefits.is_empty() || self.preconditions_chain().contains(d.id.as_str()))
            && d.required_minutes > 0
            && d.extra_minutes < MAX_EXTRA_MINUTES
    }
    fn drop_base_can_earn(&self, d: &Drop, now: DateTime<Utc>) -> bool {
        self.base_earn_conditions(d) && d.starts_at <= now && now < d.ends_at
    }
    fn drop_can_earn_within(&self, d: &Drop, stamp: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        self.base_earn_conditions(d) && d.ends_at > now && d.starts_at < stamp
    }
    pub fn drop_can_earn(
        &self,
        d: &Drop,
        channel: Option<&ChannelRef>,
        ignore_channel_status: bool,
        now: DateTime<Utc>,
    ) -> bool {
        self.drop_base_can_earn(d, now) && self.base_can_earn(channel, ignore_channel_status, now)
    }
    pub fn can_claim(&self, d: &Drop, now: DateTime<Utc>) -> bool {
        d.claim_id.is_some() && !d.is_claimed && now < self.ends_at + Duration::hours(24)
    }

    // ---- campaign-wide rules ----
    fn base_can_earn(
        &self,
        channel: Option<&ChannelRef>,
        ignore_channel_status: bool,
        now: DateTime<Utc>,
    ) -> bool {
        self.eligible()
            && self.active(now)
            && match channel {
                None => true,
                Some(ch) => {
                    (self.allowed_channels.is_empty()
                        || self.allowed_channels.iter().any(|a| a.id == ch.id))
                        && (ignore_channel_status
                            || ch.game.map(|g| *g == self.game).unwrap_or(false)
                            || self.game.is_special())
                }
            }
    }
    /// True if any of the campaign's drops can be earned (on this channel, if given).
    pub fn can_earn(
        &self,
        channel: Option<&ChannelRef>,
        ignore_channel_status: bool,
        now: DateTime<Utc>,
    ) -> bool {
        self.base_can_earn(channel, ignore_channel_status, now)
            && self.drops.iter().any(|d| self.drop_base_can_earn(d, now))
    }
    /// Like can_earn, ignoring channels, but for a moment up to `stamp` (usually an hour from now).
    pub fn can_earn_within(&self, stamp: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        self.eligible()
            && self.valid
            && self.ends_at > now
            && self.starts_at < stamp
            && self
                .drops
                .iter()
                .any(|d| self.drop_can_earn_within(d, stamp, now))
    }
    /// The drop being mined: the earnable one closest to done.
    pub fn first_drop(&self, now: DateTime<Utc>) -> Option<usize> {
        self.drops
            .iter()
            .enumerate()
            .filter(|(_, d)| self.drop_can_earn(d, None, false, now))
            .min_by_key(|(_, d)| d.remaining_minutes())
            .map(|(i, _)| i)
    }
    /// Moves every drop's watched minutes by `delta` (they progress together).
    pub fn update_real_minutes(&mut self, delta: i64) {
        for d in &mut self.drops {
            d.update_real_minutes(delta);
        }
    }
    /// Sets the watched minutes reported for drop `idx`; the other drops move by the same amount.
    pub fn update_minutes(&mut self, idx: usize, new_minutes: i64) {
        let d = &self.drops[idx];
        let mut delta = new_minutes - d.real_minutes;
        if d.real_minutes + delta < 0 {
            delta = -d.real_minutes;
        } else if d.real_minutes + delta > d.required_minutes {
            delta = d.required_minutes - d.real_minutes;
        }
        self.update_real_minutes(delta);
    }
    /// Guesses a minute of progress when Twitch stops reporting. True if a drop hit the guessing limit.
    pub fn bump_minutes(&mut self, channel: &ChannelRef, now: DateTime<Utc>) -> bool {
        let earnable: Vec<bool> = self
            .drops
            .iter()
            .map(|d| self.drop_can_earn(d, Some(channel), false, now))
            .collect();
        let mut hit_limit = false;
        for (d, ok) in self.drops.iter_mut().zip(earnable) {
            if ok {
                d.extra_minutes += 1;
                if d.extra_minutes >= MAX_EXTRA_MINUTES {
                    hit_limit = true;
                }
            }
        }
        hit_limit
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Channels

#[derive(Clone, Debug)]
pub struct Stream {
    pub broadcast_id: u64,
    pub viewers: i64,
    pub drops_enabled: bool,
    pub game: Option<Game>,
    pub title: String,
    pub stream_url: Option<String>,
}

impl Stream {
    /// From GetStreamInfo's `user` object (stream + broadcastSettings).
    pub fn from_get_stream(user: &Value, drops_enabled_default: bool) -> Option<Stream> {
        let stream = &user["stream"];
        if !stream.is_object() {
            return None;
        }
        let settings = &user["broadcastSettings"];
        Some(Stream {
            broadcast_id: id_of(&stream["id"])?,
            viewers: stream["viewersCount"].as_i64().unwrap_or(0),
            drops_enabled: drops_enabled_default,
            game: Game::from_json(&settings["game"]),
            title: settings["title"].as_str().unwrap_or("").to_string(),
            stream_url: None,
        })
    }
    /// From a game directory listing.
    pub fn from_directory(node: &Value, drops_enabled: bool) -> Option<Stream> {
        Some(Stream {
            broadcast_id: id_of(&node["id"])?,
            viewers: node["viewersCount"].as_i64().unwrap_or(0),
            drops_enabled,
            game: Game::from_json(&node["game"]),
            title: node["title"].as_str().unwrap_or("").to_string(),
            stream_url: None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: u64,
    pub login: String,
    pub display_name: Option<String>,
    /// From a campaign's allow-list: tried first, and kept through cleanups.
    pub acl_based: bool,
    pub stream: Option<Stream>,
    /// A "stream-up" came in; the stream is checked again after a short delay.
    pub pending_online: bool,
}

impl Channel {
    pub fn from_acl(a: &AclChannel) -> Channel {
        Channel {
            id: a.id,
            login: a.login.clone(),
            display_name: a.display_name.clone(),
            acl_based: true,
            stream: None,
            pending_online: false,
        }
    }
    pub fn from_directory(node: &Value, drops_enabled: bool) -> Option<Channel> {
        let b = &node["broadcaster"];
        Some(Channel {
            id: id_of(&b["id"])?,
            login: b["login"].as_str()?.to_string(),
            display_name: b["displayName"].as_str().map(str::to_string),
            acl_based: false,
            stream: Stream::from_directory(node, drops_enabled),
            pending_online: false,
        })
    }
    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.login)
    }
    pub fn online(&self) -> bool {
        self.stream.is_some()
    }
    pub fn offline(&self) -> bool {
        self.stream.is_none() && !self.pending_online
    }
    pub fn game(&self) -> Option<&Game> {
        self.stream.as_ref().and_then(|s| s.game.as_ref())
    }
    pub fn viewers(&self) -> Option<i64> {
        self.stream.as_ref().map(|s| s.viewers)
    }
    pub fn drops_enabled(&self) -> bool {
        self.stream
            .as_ref()
            .map(|s| s.drops_enabled)
            .unwrap_or(false)
    }
    pub fn as_ref(&self) -> ChannelRef<'_> {
        ChannelRef {
            id: self.id,
            game: self.game(),
        }
    }
    pub fn url(&self) -> String {
        format!("https://www.twitch.tv/{}", self.login)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        parse_ts("2026-11-18T12:00:00Z").unwrap()
    }

    fn campaign_json() -> Value {
        json!({
            "id": "camp1", "name": "Winter Drops", "status": "ACTIVE",
            "startAt": "2026-11-01T00:00:00Z", "endAt": "2026-12-01T00:00:00Z",
            "accountLinkURL": "https://example.com/link",
            "self": { "isAccountConnected": true },
            "game": { "id": "263490", "displayName": "Rust", "slug": "rust", "boxArtURL": "https://cdn/rust-285x380.jpg" },
            "allow": { "isEnabled": true, "channels": null },
            "timeBasedDrops": [
                { "id": "d1", "name": "Hoodie", "startAt": "2026-11-01T00:00:00Z", "endAt": "2026-12-01T00:00:00Z",
                  "requiredMinutesWatched": 60, "preconditionDrops": null,
                  "benefitEdges": [{ "benefit": { "id": "b1", "name": "Hoodie", "distributionType": "DIRECT_ENTITLEMENT", "imageAssetURL": "https://cdn/b1.png" } }],
                  "self": { "dropInstanceID": null, "isClaimed": false, "currentMinutesWatched": 20 } },
                { "id": "d2", "name": "Pants", "startAt": "2026-11-01T00:00:00Z", "endAt": "2026-12-01T00:00:00Z",
                  "requiredMinutesWatched": 120, "preconditionDrops": [{ "id": "d1" }],
                  "benefitEdges": [{ "benefit": { "id": "b2", "name": "Pants", "distributionType": "DIRECT_ENTITLEMENT", "imageAssetURL": "https://cdn/b2.png" } }],
                  "self": { "dropInstanceID": null, "isClaimed": false, "currentMinutesWatched": 20 } }
            ]
        })
    }

    fn campaign() -> Campaign {
        Campaign::from_json(&campaign_json(), &HashMap::new(), false).unwrap()
    }

    #[test]
    fn parses_campaign_and_strips_image_size() {
        let c = campaign();
        assert_eq!(c.game.name, "Rust");
        assert_eq!(c.image_url, "https://cdn/rust.jpg");
        assert_eq!(c.drops.len(), 2);
        assert!(c.eligible());
        assert!(c.active(now()));
    }

    #[test]
    fn preconditions_gate_the_second_drop() {
        let c = campaign();
        // d2 needs d1 claimed first, so only d1 is mined now
        assert_eq!(c.first_drop(now()), Some(0));
        assert_eq!(c.total_remaining(&c.drops[1]), 100 + 40);
        assert_eq!(c.remaining_minutes(), 140);
    }

    #[test]
    fn channel_rules_follow_game_and_allow_list() {
        let mut c = campaign();
        let rust = c.game.clone();
        let other = Game {
            id: 1,
            name: "Other".into(),
            slug: "other".into(),
        };
        assert!(c.can_earn(
            Some(&ChannelRef {
                id: 5,
                game: Some(&rust)
            }),
            false,
            now()
        ));
        assert!(!c.can_earn(
            Some(&ChannelRef {
                id: 5,
                game: Some(&other)
            }),
            false,
            now()
        ));
        assert!(c.can_earn(Some(&ChannelRef { id: 5, game: None }), true, now()));
        c.allowed_channels = vec![AclChannel {
            id: 9,
            login: "nine".into(),
            display_name: None,
        }];
        assert!(!c.can_earn(
            Some(&ChannelRef {
                id: 5,
                game: Some(&rust)
            }),
            false,
            now()
        ));
        assert!(c.can_earn(
            Some(&ChannelRef {
                id: 9,
                game: Some(&rust)
            }),
            false,
            now()
        ));
    }

    #[test]
    fn unlinked_campaigns_are_not_earnable() {
        let mut j = campaign_json();
        j["self"]["isAccountConnected"] = json!(false);
        let c = Campaign::from_json(&j, &HashMap::new(), false).unwrap();
        assert!(!c.eligible());
        assert!(!c.can_earn(None, false, now()));
    }

    #[test]
    fn minutes_move_together_and_clamp() {
        let mut c = campaign();
        c.update_minutes(0, 50);
        assert_eq!(c.drops[0].real_minutes, 50);
        assert_eq!(c.drops[1].real_minutes, 50);
        c.update_minutes(0, 500);
        assert_eq!(c.drops[0].real_minutes, 60);
        assert_eq!(c.drops[1].real_minutes, 60);
        assert!((c.drops[0].progress() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn guessed_minutes_stop_at_the_limit() {
        let mut c = campaign();
        let rust = c.game.clone();
        let ch = ChannelRef {
            id: 1,
            game: Some(&rust),
        };
        let mut hit = false;
        for _ in 0..MAX_EXTRA_MINUTES {
            hit = c.bump_minutes(&ch, now());
        }
        assert!(hit);
        // a drop at the limit can't be earned any more until the next reload
        assert!(c.first_drop(now()).is_none());
    }

    #[test]
    fn claimed_benefits_mark_drops_without_self_edge() {
        let mut j = campaign_json();
        j["timeBasedDrops"][0]
            .as_object_mut()
            .unwrap()
            .remove("self");
        let mut claimed = HashMap::new();
        claimed.insert("b1".to_string(), parse_ts("2026-11-10T00:00:00Z").unwrap());
        let c = Campaign::from_json(&j, &claimed, false).unwrap();
        assert!(c.drops[0].is_claimed);
        assert_eq!(c.drops[0].real_minutes, 60);
        // with d1 claimed, d2 becomes the one to mine
        assert_eq!(c.first_drop(now()), Some(1));
    }

    #[test]
    fn claims_allowed_for_a_day_after_the_end() {
        let mut c = campaign();
        c.drops[0].claim_id = Some("x".into());
        let after = c.ends_at + Duration::hours(23);
        assert!(c.can_claim(&c.drops[0], after));
        assert!(!c.can_claim(&c.drops[0], c.ends_at + Duration::hours(25)));
    }

    #[test]
    fn directory_channel_parses() {
        let node = json!({ "id": "777", "viewersCount": 1234, "title": "hi",
            "game": { "id": "263490", "displayName": "Rust" },
            "broadcaster": { "id": "42", "login": "streamer", "displayName": "Streamer" } });
        let ch = Channel::from_directory(&node, true).unwrap();
        assert_eq!(ch.id, 42);
        assert_eq!(ch.name(), "Streamer");
        assert!(ch.online() && ch.drops_enabled());
        assert_eq!(ch.viewers(), Some(1234));
    }
}
