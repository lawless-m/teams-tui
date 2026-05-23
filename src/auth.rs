//! OAuth2 device code flow, token refresh and persistence.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCOPES: &[&str] = &[
    "User.Read",
    "Chat.ReadWrite",
    "ChannelMessage.Send",
    "ChannelMessage.Read.All",
    "Team.ReadBasic.All",
    "Channel.ReadBasic.All",
    "offline_access",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

pub async fn device_code_flow(tenant_id: &str, client_id: &str) -> Result<Tokens> {
    let client = reqwest::Client::new();
    let scope = SCOPES.join(" ");

    let dc_url =
        format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/devicecode");
    let dc_resp: DeviceCodeResponse = client
        .post(&dc_url)
        .form(&[("client_id", client_id), ("scope", &scope)])
        .send()
        .await
        .context("device code request failed")?
        .error_for_status()
        .context("device code request returned error status")?
        .json()
        .await
        .context("device code response was not JSON")?;

    println!("{}", dc_resp.message);

    let token_url = format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token");
    let mut interval = Duration::from_secs(dc_resp.interval.max(1));
    let deadline = std::time::Instant::now() + Duration::from_secs(dc_resp.expires_in);

    loop {
        tokio::time::sleep(interval).await;
        if std::time::Instant::now() > deadline {
            bail!("device code expired before user authorized");
        }

        let resp = client
            .post(&token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &dc_resp.device_code),
            ])
            .send()
            .await
            .context("token poll request failed")?;

        if resp.status().is_success() {
            let tok: TokenResponse = resp.json().await.context("token response parse")?;
            return Ok(token_response_to_tokens(tok));
        }

        let err: ErrorResponse = resp.json().await.context("token error parse")?;
        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                interval += Duration::from_secs(5);
                continue;
            }
            "expired_token" | "code_expired" => bail!("device code expired"),
            "authorization_declined" => bail!("user declined authorization"),
            other => bail!(
                "device flow error: {other}: {}",
                err.error_description.unwrap_or_default()
            ),
        }
    }
}

pub fn microsoft_token_url(tenant_id: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token")
}

pub async fn refresh_at(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let client = reqwest::Client::new();
    let scope = SCOPES.join(" ");
    let resp = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", &scope),
        ])
        .send()
        .await
        .context("refresh request failed")?;
    if !resp.status().is_success() {
        let err: ErrorResponse = resp.json().await.context("refresh error parse")?;
        bail!(
            "refresh failed: {}: {}",
            err.error,
            err.error_description.unwrap_or_default()
        );
    }
    let tok: TokenResponse = resp.json().await.context("refresh response parse")?;
    Ok(token_response_to_tokens(tok))
}

pub async fn refresh(tenant_id: &str, client_id: &str, refresh_token: &str) -> Result<Tokens> {
    refresh_at(&microsoft_token_url(tenant_id), client_id, refresh_token).await
}

fn token_response_to_tokens(t: TokenResponse) -> Tokens {
    Tokens {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: Utc::now() + chrono::Duration::seconds(t.expires_in as i64),
    }
}

impl Tokens {
    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let t: Tokens = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse token at {}", path.display()))?;
                Ok(Some(t))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("failed to read token at {}", path.display()))
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create token dir {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("serialize tokens")?;
        let mut tmp_os = path.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("failed to write token tmp at {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp, perm)
                .with_context(|| format!("failed to set 0600 on {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename token into place at {}", path.display()))?;
        Ok(())
    }

    pub fn is_fresh(&self) -> bool {
        self.expires_at > Utc::now() + chrono::Duration::minutes(5)
    }

    pub async fn ensure_fresh(&mut self, tenant_id: &str, client_id: &str) -> Result<()> {
        if !self.is_fresh() {
            let new_tokens = refresh(tenant_id, client_id, &self.refresh_token).await?;
            *self = new_tokens;
        }
        Ok(())
    }
}

pub fn default_token_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "teams-tui")
        .map(|d| d.config_dir().join("token.json"))
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
        p.push(format!("teams-tui-auth-{tag}-{n}"));
        p
    }

    fn sample_tokens() -> Tokens {
        Tokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn save_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir("mode");
        let path = dir.join("token.json");
        sample_tokens().save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_returns_none() {
        let dir = tempdir("missing");
        let path = dir.join("token.json");
        assert!(Tokens::load_from(&path).unwrap().is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempdir("roundtrip");
        let path = dir.join("token.json");
        let t = sample_tokens();
        t.save_to(&path).unwrap();
        let loaded = Tokens::load_from(&path).unwrap().unwrap();
        assert_eq!(loaded, t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_fresh_true_when_expires_well_in_future() {
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        };
        assert!(t.is_fresh());
    }

    #[test]
    fn is_fresh_false_within_5_minute_window() {
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(4),
        };
        assert!(!t.is_fresh());
    }
}
