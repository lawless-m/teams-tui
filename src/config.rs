//! Config loading: TOML at ~/.config/teams-tui/config.toml.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub auth: AuthConfig,
    #[serde(default)]
    pub follow: FollowConfig,
    pub notifications: NotificationsConfig,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct AuthConfig {
    pub tenant_id: String,
    pub client_id: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Default)]
pub struct FollowConfig {
    #[serde(default)]
    pub channels: Vec<ChannelFollow>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct ChannelFollow {
    pub team: String,
    pub channel: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct NotificationsConfig {
    pub mention_desktop: bool,
    pub status_file: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config at {}", path.display()))?;
        cfg.notifications.status_file = expand_tilde(&cfg.notifications.status_file);
        Ok(cfg)
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut pb = PathBuf::from(home);
        pb.push(rest);
        return pb;
    }
    if s == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempfile_path(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("teams-tui-cfg-{tag}-{n}.toml"));
        p
    }

    #[test]
    fn parses_valid_config() {
        let toml = r#"
[auth]
tenant_id = "tid"
client_id = "cid"

[[follow.channels]]
team = "IT Department"
channel = "IT-Support"

[[follow.channels]]
team = "Engineering"
channel = "general"

[notifications]
mention_desktop = true
status_file = "/tmp/teams-tui-status"
"#;
        let tmp = tempfile_path("valid");
        std::fs::write(&tmp, toml).unwrap();
        let cfg = Config::load(&tmp).unwrap();
        assert_eq!(cfg.auth.tenant_id, "tid");
        assert_eq!(cfg.auth.client_id, "cid");
        assert_eq!(cfg.follow.channels.len(), 2);
        assert_eq!(cfg.follow.channels[0].team, "IT Department");
        assert_eq!(cfg.follow.channels[0].channel, "IT-Support");
        assert_eq!(cfg.follow.channels[1].team, "Engineering");
        assert_eq!(cfg.follow.channels[1].channel, "general");
        assert!(cfg.notifications.mention_desktop);
        assert_eq!(
            cfg.notifications.status_file,
            PathBuf::from("/tmp/teams-tui-status")
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn missing_file_gives_clear_error() {
        let tmp = tempfile_path("missing");
        let err = Config::load(&tmp).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(tmp.to_str().unwrap()),
            "error should mention path, got: {msg}"
        );
    }

    #[test]
    fn malformed_toml_gives_clear_error() {
        let tmp = tempfile_path("malformed");
        std::fs::write(&tmp, "this = is = not valid toml\n[unclosed").unwrap();
        let err = Config::load(&tmp).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(tmp.to_str().unwrap()),
            "error should mention path, got: {msg}"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tilde_expands_in_status_file() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let toml = r#"
[auth]
tenant_id = "t"
client_id = "c"

[follow]
channels = []

[notifications]
mention_desktop = false
status_file = "~/.cache/teams-tui/status"
"#;
        let tmp = tempfile_path("tilde");
        std::fs::write(&tmp, toml).unwrap();
        let cfg = Config::load(&tmp).unwrap();
        assert_eq!(
            cfg.notifications.status_file,
            PathBuf::from(format!("{home}/.cache/teams-tui/status"))
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
