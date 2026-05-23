//! Typed wrappers around the Microsoft Graph endpoints used.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::auth::{self, Tokens};

#[derive(Debug)]
pub enum GraphError {
    CursorExpired,
    Other(anyhow::Error),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::CursorExpired => write!(f, "delta cursor expired (410 Gone)"),
            GraphError::Other(e) => std::fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<anyhow::Error> for GraphError {
    fn from(e: anyhow::Error) -> Self {
        GraphError::Other(e)
    }
}

impl From<reqwest::Error> for GraphError {
    fn from(e: reqwest::Error) -> Self {
        GraphError::Other(anyhow!(e))
    }
}

pub struct GraphClient {
    http: reqwest::Client,
    tokens: Arc<Mutex<Tokens>>,
    base_url: String,
    token_url: String,
    client_id: String,
}

impl GraphClient {
    pub fn new(
        tokens: Arc<Mutex<Tokens>>,
        base_url: String,
        token_url: String,
        client_id: String,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens,
            base_url,
            token_url,
            client_id,
        }
    }

    async fn refresh_tokens(&self) -> Result<(), GraphError> {
        let refresh_token = self.tokens.lock().await.refresh_token.clone();
        let new = auth::refresh_at(&self.token_url, &self.client_id, &refresh_token).await?;
        *self.tokens.lock().await = new;
        Ok(())
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path_or_url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, GraphError> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.base_url, path_or_url)
        };

        let mut refreshed = false;

        loop {
            let token = self.tokens.lock().await.access_token.clone();
            let mut req = self.http.request(method.clone(), &url).bearer_auth(&token);
            if let Some(b) = body.as_ref() {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status();

            if status.is_success() {
                let data: T = resp.json().await?;
                return Ok(data);
            }

            match status {
                StatusCode::UNAUTHORIZED if !refreshed => {
                    self.refresh_tokens().await?;
                    refreshed = true;
                    continue;
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let retry = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(5);
                    tokio::time::sleep(Duration::from_secs(retry)).await;
                    continue;
                }
                StatusCode::GONE => return Err(GraphError::CursorExpired),
                s => {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(GraphError::Other(anyhow!("graph error {s}: {text}")));
                }
            }
        }
    }

    pub async fn me(&self) -> Result<Me, GraphError> {
        self.request(Method::GET, "/me", None).await
    }

    pub async fn joined_teams(&self) -> Result<Vec<Team>, GraphError> {
        let r: ListResponse<Team> = self.request(Method::GET, "/me/joinedTeams", None).await?;
        Ok(r.value)
    }

    pub async fn team_channels(&self, team_id: &str) -> Result<Vec<Channel>, GraphError> {
        let r: ListResponse<Channel> = self
            .request(Method::GET, &format!("/teams/{team_id}/channels"), None)
            .await?;
        Ok(r.value)
    }

    pub async fn list_chats(&self) -> Result<Vec<Chat>, GraphError> {
        let r: ListResponse<Chat> = self
            .request(Method::GET, "/me/chats?$expand=members", None)
            .await?;
        Ok(r.value)
    }

    pub async fn chat_messages_delta(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
    ) -> Result<MessagesDelta, GraphError> {
        let url = match cursor {
            None => {
                let since = (chrono::Utc::now() - chrono::Duration::hours(24))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                format!(
                    "/chats/{chat_id}/messages?$top=50&$orderby=lastModifiedDateTime%20desc&$filter=lastModifiedDateTime%20gt%20{since}"
                )
            }
            Some(c) if c.starts_with("http") => c.to_string(),
            Some(ts) => format!(
                "/chats/{chat_id}/messages?$top=50&$orderby=lastModifiedDateTime%20desc&$filter=lastModifiedDateTime%20gt%20{ts}"
            ),
        };
        self.request(Method::GET, &url, None).await
    }

    pub async fn channel_messages_delta(
        &self,
        team_id: &str,
        channel_id: &str,
        cursor: Option<&str>,
    ) -> Result<MessagesDelta, GraphError> {
        let url = cursor
            .map(String::from)
            .unwrap_or_else(|| format!("/teams/{team_id}/channels/{channel_id}/messages/delta"));
        self.request(Method::GET, &url, None).await
    }

    pub async fn send_chat_message(
        &self,
        chat_id: &str,
        text: &str,
    ) -> Result<serde_json::Value, GraphError> {
        let body = json!({ "body": { "contentType": "text", "content": text } });
        self.request(
            Method::POST,
            &format!("/chats/{chat_id}/messages"),
            Some(body),
        )
        .await
    }

    pub async fn send_channel_message(
        &self,
        team_id: &str,
        channel_id: &str,
        text: &str,
    ) -> Result<serde_json::Value, GraphError> {
        let body = json!({ "body": { "contentType": "text", "content": text } });
        self.request(
            Method::POST,
            &format!("/teams/{team_id}/channels/{channel_id}/messages"),
            Some(body),
        )
        .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse<T> {
    pub value: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: String,
    pub topic: Option<String>,
    #[serde(rename = "chatType")]
    pub chat_type: String,
    #[serde(default)]
    pub members: Vec<ChatMember>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMember {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesDelta {
    #[serde(default)]
    pub value: Vec<GraphMessage>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphMessage {
    pub id: String,
    #[serde(rename = "messageType")]
    #[serde(default)]
    pub message_type: Option<String>,
    pub from: Option<GraphFrom>,
    pub body: Option<GraphBody>,
    pub subject: Option<String>,
    #[serde(rename = "createdDateTime")]
    pub created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime")]
    pub last_modified_date_time: Option<String>,
    #[serde(rename = "deletedDateTime")]
    pub deleted_date_time: Option<String>,
    #[serde(default)]
    pub mentions: Vec<GraphMention>,
    #[serde(default)]
    pub attachments: Vec<GraphAttachment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphFrom {
    pub user: Option<GraphUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphUser {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphBody {
    #[serde(rename = "contentType")]
    pub content_type: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphMention {
    pub mentioned: Option<Mentioned>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mentioned {
    pub user: Option<GraphUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphAttachment {
    pub name: Option<String>,
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn new_client(server: &MockServer) -> (Arc<Mutex<Tokens>>, GraphClient) {
        let tokens = Arc::new(Mutex::new(Tokens {
            access_token: "initial-access".into(),
            refresh_token: "initial-refresh".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        }));
        let client = GraphClient::new(
            tokens.clone(),
            server.uri(),
            format!("{}/oauth2/v2.0/token", server.uri()),
            "client-1".into(),
        );
        (tokens, client)
    }

    #[tokio::test]
    async fn happy_path_delta() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/c1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "m1"}],
                "@odata.deltaLink": "https://graph/delta-next"
            })))
            .mount(&server)
            .await;

        let (_tokens, client) = new_client(&server).await;
        let delta = client.chat_messages_delta("c1", None).await.unwrap();
        assert_eq!(delta.value.len(), 1);
        assert_eq!(delta.value[0].id, "m1");
        assert_eq!(
            delta.delta_link.as_deref(),
            Some("https://graph/delta-next")
        );
    }

    #[tokio::test]
    async fn refreshes_on_401_and_retries() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/chats/c1/messages"))
            .and(header("authorization", "Bearer initial-access"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access",
                "refresh_token": "refreshed-refresh",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/chats/c1/messages"))
            .and(header("authorization", "Bearer refreshed-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "m-after-refresh"}]
            })))
            .mount(&server)
            .await;

        let (tokens, client) = new_client(&server).await;
        let delta = client.chat_messages_delta("c1", None).await.unwrap();
        assert_eq!(delta.value.len(), 1);
        assert_eq!(delta.value[0].id, "m-after-refresh");

        let tokens = tokens.lock().await;
        assert_eq!(tokens.access_token, "refreshed-access");
        assert_eq!(tokens.refresh_token, "refreshed-refresh");
    }

    #[tokio::test]
    async fn retries_on_429_after_retry_after_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/me"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1",
                "displayName": "User"
            })))
            .mount(&server)
            .await;

        let (_tokens, client) = new_client(&server).await;
        let me = client.me().await.unwrap();
        assert_eq!(me.id, "u1");
        assert_eq!(me.display_name.as_deref(), Some("User"));
    }

    #[tokio::test]
    async fn returns_cursor_expired_on_410() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/c1/messages"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let (_tokens, client) = new_client(&server).await;
        let err = client.chat_messages_delta("c1", None).await.unwrap_err();
        assert!(
            matches!(err, GraphError::CursorExpired),
            "expected CursorExpired, got {err:?}"
        );
    }
}
