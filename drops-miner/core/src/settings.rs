//! The miner's settings, saved as settings.json in the app's data folder (settings.py).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PriorityMode {
    /// Only mine games on the priority list.
    #[default]
    PriorityOnly,
    /// Priority list first, then every other game, the campaign that ends soonest first.
    EndingSoonest,
    /// Priority list first, then every other game, the campaign with the least time to spare first.
    LowAvailability,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Games to mine, most wanted first.
    pub priority: Vec<String>,
    /// Games never to mine.
    pub exclude: BTreeSet<String>,
    pub priority_mode: PriorityMode,
    /// Mine campaigns whose rewards are badges or emotes (they need no account linking).
    pub enable_badges_emotes: bool,
    /// Double-check each stream really has drops before watching it (slower, more accurate).
    pub available_drops_check: bool,
    /// Show a Windows notification when a drop is claimed.
    pub notifications: bool,
    /// 1 (fast connection) to 6 (slow connection): how long to wait before a request times out.
    pub connection_quality: u8,
    /// An HTTP proxy for Twitch requests, e.g. http://user:pass@host:port. Empty for none.
    pub proxy: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            priority: Vec::new(),
            exclude: BTreeSet::new(),
            priority_mode: PriorityMode::PriorityOnly,
            enable_badges_emotes: false,
            available_drops_check: false,
            notifications: true,
            connection_quality: 1,
            proxy: String::new(),
        }
    }
}

impl Settings {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    fn read(path: &Path) -> Option<Settings> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut s: Settings = serde_json::from_str(&text).ok()?;
        s.connection_quality = s.connection_quality.clamp(1, 6);
        Some(s)
    }

    /// Loads settings.json. If it's missing or unreadable (say, the app data was deleted by an uninstall) and a
    /// backup copy exists, the backup is used and written back. The second value says a backup was restored.
    pub fn load(dir: &Path, backup: Option<&Path>) -> (Self, bool) {
        if let Some(s) = Self::read(&Self::path(dir)) {
            return (s, false);
        }
        if let Some(s) = backup.and_then(|b| Self::read(&b.join("settings.json"))) {
            let _ = s.save(dir, None);
            return (s, true);
        }
        (Settings::default(), false)
    }

    /// Saves settings.json, and the same file to the backup folder when there is one.
    pub fn save(&self, dir: &Path, backup: Option<&Path>) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("settings.json.tmp");
        std::fs::write(&tmp, &text)?;
        std::fs::rename(tmp, Self::path(dir))?;
        if let Some(b) = backup {
            // the backup is a convenience: a failure here doesn't fail the save
            if std::fs::create_dir_all(b).is_ok() {
                let _ = std::fs::write(b.join("settings.json"), &text);
            }
        }
        Ok(())
    }
}

/// The sign-in the miner keeps between runs (upstream keeps the same in cookies.jar).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub device_id: String,
    pub access_token: String,
    pub user_id: u64,
    pub login: String,
}

impl Session {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("session.json")
    }
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(Self::path(dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            Self::path(dir),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_fill_defaults() {
        let dir = std::env::temp_dir().join(format!("dm-settings-{}", std::process::id()));
        let mut s = Settings::default();
        s.priority = vec!["Rust".into(), "Marvel Rivals".into()];
        s.exclude.insert("Fortnite".into());
        s.priority_mode = PriorityMode::EndingSoonest;
        s.save(&dir, None).unwrap();
        let (back, restored) = Settings::load(&dir, None);
        assert!(!restored);
        assert_eq!(back.priority, s.priority);
        assert!(back.exclude.contains("Fortnite"));
        assert_eq!(back.priority_mode, PriorityMode::EndingSoonest);
        // a partial file still loads, with defaults for the rest
        std::fs::write(
            Settings::path(&dir),
            r#"{"priority":["X"],"connection_quality":9}"#,
        )
        .unwrap();
        let (partial, _) = Settings::load(&dir, None);
        assert_eq!(partial.priority, vec!["X".to_string()]);
        assert!(partial.notifications);
        assert_eq!(partial.connection_quality, 6);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn game_lists_come_back_from_the_backup_after_app_data_is_wiped() {
        let base = std::env::temp_dir().join(format!("dm-backup-{}", std::process::id()));
        let (dir, backup) = (base.join("appdata"), base.join("documents"));
        let mut s = Settings::default();
        s.priority = vec!["Overwatch".into(), "Rust".into()];
        s.exclude.insert("Fortnite".into());
        s.save(&dir, Some(&backup)).unwrap();
        assert!(backup.join("settings.json").exists());
        // an uninstall deletes the app data folder
        std::fs::remove_dir_all(&dir).unwrap();
        let (back, restored) = Settings::load(&dir, Some(&backup));
        assert!(restored);
        assert_eq!(back.priority, s.priority);
        assert!(back.exclude.contains("Fortnite"));
        // and the restored copy is written back to the app data folder
        assert!(Settings::path(&dir).exists());
        let _ = std::fs::remove_dir_all(base);
    }
}
