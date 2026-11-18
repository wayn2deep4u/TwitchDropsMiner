//! Twitch's private API details, ported from DevilXD's TwitchDropsMiner (constants.py).
//! When Twitch changes something (a GQL hash, a client ID, a user agent), this is the file to update:
//! compare it with constants.py in https://github.com/DevilXD/TwitchDropsMiner.

use serde_json::{json, Value};

pub struct ClientInfo {
    pub url: &'static str,
    pub client_id: &'static str,
    pub user_agents: &'static [&'static str],
}

/// The miner signs in and talks to Twitch as the Android app, like upstream does.
pub const ANDROID_APP: ClientInfo = ClientInfo {
    url: "https://www.twitch.tv",
    client_id: "kd1unb4b3q4t58fwlpcbzcbnm76a8fp",
    user_agents: &[
        "Dalvik/2.1.0 (Linux; U; Android 16; SM-S911B Build/TP1A.220624.014) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; U; Android 16; SM-S938B Build/BP2A.250605.031) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; Android 16; SM-X716N Build/UP1A.231005.007) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; U; Android 15; SM-G990B Build/AP3A.240905.015.A2) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; U; Android 15; SM-G970F Build/AP3A.241105.008) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; U; Android 15; SM-A566E Build/AP3A.240905.015.A2) tv.twitch.android.app/31.8.0/3108006",
        "Dalvik/2.1.0 (Linux; U; Android 14; SM-X306B Build/UP1A.231005.007) tv.twitch.android.app/31.8.0/3108006",
    ],
};

pub const CLIENT: &ClientInfo = &ANDROID_APP;

/// The page the sign-in window opens. After signing in, Twitch redirects to
/// https://www.twitch.tv/#access_token=… and the app picks the token up from that address.
pub fn login_url() -> String {
    let id = CLIENT.client_id;
    format!(
        "https://www.twitch.tv/login?client_id={id}&redirect_params=client_id%3D{id}%26\
         redirect_uri%3Dhttps%253A%252F%252Fwww.twitch.tv%252F%26response_type%3Dtoken"
    )
}

/// Pulls the access token out of the address Twitch redirects to after sign-in.
pub fn token_from_url(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)#access_token=(\w+)").ok()?;
    re.captures(url).map(|c| c[1].to_string())
}

// Timings and limits (constants.py)
pub const MAX_EXTRA_MINUTES: i64 = 15;
pub const MAX_WEBSOCKETS: usize = 8;
pub const WS_TOPICS_LIMIT: usize = 50;
pub const BASE_TOPICS: usize = 2;
pub const TOPICS_PER_CHANNEL: usize = 2;
pub const MAX_CHANNELS: usize =
    (MAX_WEBSOCKETS * WS_TOPICS_LIMIT - BASE_TOPICS) / TOPICS_PER_CHANNEL;
pub const PING_INTERVAL_SECS: u64 = 180;
pub const PING_TIMEOUT_SECS: u64 = 10;
pub const ONLINE_DELAY_SECS: u64 = 120;
pub const WATCH_INTERVAL_SECS: u64 = 20;
pub const SPECIAL_GAME_IDS: [u64; 2] = [509663, 509672];

pub const PUBSUB_URL: &str = "wss://pubsub-edge.twitch.tv/v1";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";

// PubSub topics (WEBSOCKET_TOPICS)
pub const TOPIC_USER_DROPS: &str = "user-drop-events";
pub const TOPIC_USER_NOTIFICATIONS: &str = "onsite-notifications";
pub const TOPIC_STREAM_STATE: &str = "video-playback-by-id";
pub const TOPIC_STREAM_UPDATE: &str = "broadcast-settings-update";

pub fn topic(name: &str, target_id: u64) -> String {
    format!("{name}.{target_id}")
}

/// A persisted GQL operation: the name, its hash and default variables (GQL_QUERIES).
fn persisted(name: &str, sha256: &str, variables: Option<Value>) -> Value {
    let mut op = json!({
        "operationName": name,
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": sha256 } }
    });
    if let Some(v) = variables {
        op["variables"] = v;
    }
    op
}

/// The GQL operations the miner uses, by the names upstream gives them.
pub fn gql(name: &str) -> Value {
    match name {
        "GetStreamInfo" => persisted(
            "VideoPlayerStreamInfoOverlayChannel",
            "198492e0857f6aedead9665c81c5a06d67b25b58034649687124083ff288597d",
            Some(json!({ "channel": "" })),
        ),
        "ClaimDrop" => persisted(
            "DropsPage_ClaimDropRewards",
            "a455deea71bdc9015b78eb49f4acfbce8baa7ccbedd28e549bb025bd0f751930",
            Some(json!({ "input": { "dropInstanceID": "" } })),
        ),
        "Inventory" => persisted(
            "Inventory",
            "8337eb8541b314040b0edde0c09c5c7a2783ba1960aa9edfbf3bac16d0fec404",
            Some(json!({ "fetchRewardCampaigns": false })),
        ),
        "CurrentDrop" => persisted(
            "DropCurrentSessionContext",
            "4d06b702d25d652afb9ef835d2a550031f1cf762b193523a92166f40ea3d142b",
            Some(json!({ "channelID": "", "channelLogin": "" })),
        ),
        "Campaigns" => persisted(
            "ViewerDropsDashboard",
            "c16bb890cc8ce7647a96ee69cd313d423a378a3dedadf630a1017cde18975feb",
            Some(json!({ "fetchRewardCampaigns": false })),
        ),
        "CampaignDetails" => persisted(
            "DropCampaignDetails",
            "039277bf98f3130929262cc7c6efd9c141ca3749cb6dca442fc8ead9a53f77c1",
            Some(json!({ "channelLogin": "", "dropID": "" })),
        ),
        "AvailableDrops" => persisted(
            "DropsHighlightService_AvailableDrops",
            "782dad0f032942260171d2d80a654f88bdd0c5a9dddc392e9bc92218a0f42d20",
            Some(json!({ "channelID": "" })),
        ),
        "PlaybackAccessToken" => persisted(
            "PlaybackAccessToken",
            "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9",
            Some(json!({
                "isLive": true, "isVod": false, "login": "", "platform": "web",
                "playerType": "site", "vodID": ""
            })),
        ),
        "GameDirectory" => persisted(
            "DirectoryPage_Game",
            "86bcceb4e8b1a51256ff8eed8bd8aae4acacf80d737efe904f84f3aeadf8cafd",
            Some(json!({
                "limit": 30,
                "slug": "",
                "imageWidth": 50,
                "includeCostreaming": false,
                "options": {
                    "broadcasterLanguages": [],
                    "freeformTags": null,
                    "includeRestricted": ["SUB_ONLY_LIVE"],
                    "recommendationsContext": { "platform": "web" },
                    "sort": "RELEVANCE",
                    "systemFilters": [],
                    "tags": [],
                    "requestID": "JIRA-VXP-2397"
                },
                "sortTypeIsRecency": false
            })),
        ),
        "NotificationsDelete" => persisted(
            "OnsiteNotifications_DeleteNotification",
            "13d463c831f28ffe17dccf55b3148ed8b3edbbd0ebadd56352f1ff0160616816",
            Some(json!({ "input": { "id": "" } })),
        ),
        other => panic!("unknown GQL operation {other}"),
    }
}

/// Deep-merges `extra` into `base` (objects merge key by key; anything else is replaced).
pub fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(b), Value::Object(e)) => {
            for (k, v) in e {
                match b.get_mut(&k) {
                    Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, e) => *b = e,
    }
}

/// An operation with its variables filled in.
pub fn gql_with(name: &str, variables: Value) -> Value {
    let mut op = gql(name);
    let vars = op.get_mut("variables").expect("operation has variables");
    merge(vars, variables);
    op
}

/// A game's directory slug, worked out from its name when Twitch didn't send one (utils.Game.slug).
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase().replace('\'', "");
    let re = regex::Regex::new(r"\W+").unwrap();
    let dashed = re.replace_all(&lower, "-");
    let re2 = regex::Regex::new(r"-{2,}").unwrap();
    re2.replace_all(dashed.trim_matches('-'), "-").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_merge_deeply() {
        let op = gql_with(
            "GameDirectory",
            json!({ "slug": "rust", "options": { "systemFilters": ["DROPS_ENABLED"] } }),
        );
        assert_eq!(op["variables"]["slug"], "rust");
        assert_eq!(
            op["variables"]["options"]["systemFilters"][0],
            "DROPS_ENABLED"
        );
        // untouched defaults survive
        assert_eq!(op["variables"]["options"]["sort"], "RELEVANCE");
        assert_eq!(op["extensions"]["persistedQuery"]["version"], 1);
    }

    #[test]
    fn token_comes_out_of_the_redirect() {
        assert_eq!(
            token_from_url(
                "https://www.twitch.tv/#access_token=abc123xyz&scope=&token_type=bearer"
            )
            .as_deref(),
            Some("abc123xyz")
        );
        assert_eq!(token_from_url("https://www.twitch.tv/login"), None);
    }

    #[test]
    fn slugs_match_upstream() {
        assert_eq!(
            slugify("Tom Clancy's Rainbow Six Siege"),
            "tom-clancys-rainbow-six-siege"
        );
        assert_eq!(slugify("  Path of Exile 2 "), "path-of-exile-2");
        assert_eq!(
            slugify("World of Warcraft: The War Within"),
            "world-of-warcraft-the-war-within"
        );
    }

    #[test]
    fn channel_limit_matches_upstream() {
        assert_eq!(MAX_CHANNELS, 199);
    }
}
