//! Delta cursor persistence to ~/.cache/teams-tui/cursors.json.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConversationKey {
    Chat {
        id: String,
    },
    Channel {
        team_id: String,
        channel_id: String,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cursors {
    pub entries: HashMap<ConversationKey, String>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: ConversationKey,
    cursor: String,
}

impl Cursors {
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let wire: Vec<Entry> = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse cursors at {}", path.display()))?;
                let entries = wire.into_iter().map(|e| (e.key, e.cursor)).collect();
                Ok(Cursors { entries })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursors::default()),
            Err(e) => {
                Err(e).with_context(|| format!("failed to read cursors at {}", path.display()))
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create cursors dir {}", parent.display())
            })?;
        }
        let wire: Vec<Entry> = self
            .entries
            .iter()
            .map(|(k, v)| Entry {
                key: k.clone(),
                cursor: v.clone(),
            })
            .collect();
        let bytes = serde_json::to_vec_pretty(&wire).context("failed to serialize cursors")?;
        let mut tmp_os = path.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("failed to write cursors tmp at {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("failed to rename cursors into place at {}", path.display())
        })?;
        Ok(())
    }
}

pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "teams-tui")
        .map(|d| d.cache_dir().join("cursors.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("teams-tui-cursors-{tag}-{n}"));
        p
    }

    #[test]
    fn round_trip_chat_and_channel() {
        let dir = tempdir("roundtrip");
        let path = dir.join("cursors.json");

        let mut cursors = Cursors::default();
        cursors.entries.insert(
            ConversationKey::Chat {
                id: "chat-123".into(),
            },
            "cursor-abc".into(),
        );
        cursors.entries.insert(
            ConversationKey::Channel {
                team_id: "team-1".into(),
                channel_id: "ch-1".into(),
            },
            "cursor-xyz".into(),
        );

        cursors.save_to(&path).unwrap();
        let loaded = Cursors::load_from(&path).unwrap();
        assert_eq!(loaded, cursors);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_returns_empty() {
        let dir = tempdir("missing");
        let path = dir.join("cursors.json");
        let loaded = Cursors::load_from(&path).unwrap();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn creates_parent_dir_on_save() {
        let dir = tempdir("parent");
        let path = dir.join("deep/nested/cursors.json");
        Cursors::default().save_to(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
