//! The two polling loops: 5s message delta, 30s chat discovery.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::DateTime;

use crate::cursors::{ConversationKey, Cursors};
use crate::graph::{GraphClient, GraphError, GraphMessage, MessagesDelta};
use crate::notify::{Notifier, StatusCounters};
use crate::render::{Conversation, html_to_text, prefix_for};
use crate::stream::{IncomingEvent, NotificationKind, Stream};

pub struct PostHandlers {
    pub notifier: Notifier,
    pub counters: StatusCounters,
}

pub const MESSAGE_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ChatFollowInfo {
    pub id: String,
    pub topic: Option<String>,
    pub chat_type: String,
    pub member_names: Vec<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelFollowInfo {
    pub team_id: String,
    pub channel_id: String,
    pub team_name: String,
    pub channel_name: String,
    pub cursor: Option<String>,
    pub disambiguate_team: bool,
}

#[derive(Debug, Default)]
pub struct FollowState {
    pub chats: HashMap<String, ChatFollowInfo>,
    pub channels: HashMap<(String, String), ChannelFollowInfo>,
    pub chat_tags: HashMap<char, String>,
}

impl FollowState {
    pub fn assign_chat_tags(&mut self) {
        let mut sorted_ids: Vec<String> = self.chats.keys().cloned().collect();
        sorted_ids.sort();
        let already_tagged: std::collections::HashSet<String> =
            self.chat_tags.values().cloned().collect();
        let mut used: std::collections::HashSet<char> = self.chat_tags.keys().copied().collect();
        let mut next: u8 = b'A';
        for id in sorted_ids {
            if already_tagged.contains(&id) {
                continue;
            }
            while next <= b'Z' && used.contains(&(next as char)) {
                next += 1;
            }
            if next > b'Z' {
                break;
            }
            let tag = next as char;
            self.chat_tags.insert(tag, id);
            used.insert(tag);
            next += 1;
        }
    }

    pub fn chat_for_tag(&self, tag: char) -> Option<&ChatFollowInfo> {
        self.chat_tags.get(&tag).and_then(|id| self.chats.get(id))
    }

    pub fn to_cursors(&self) -> Cursors {
        let mut entries = HashMap::new();
        for (id, chat) in &self.chats {
            if let Some(c) = &chat.cursor {
                entries.insert(ConversationKey::Chat { id: id.clone() }, c.clone());
            }
        }
        for ((tid, cid), ch) in &self.channels {
            if let Some(c) = &ch.cursor {
                entries.insert(
                    ConversationKey::Channel {
                        team_id: tid.clone(),
                        channel_id: cid.clone(),
                    },
                    c.clone(),
                );
            }
        }
        Cursors { entries }
    }

    pub fn apply_cursors(&mut self, cursors: &Cursors) {
        for (key, cursor) in &cursors.entries {
            match key {
                ConversationKey::Chat { id } => {
                    if let Some(chat) = self.chats.get_mut(id) {
                        chat.cursor = Some(cursor.clone());
                    }
                }
                ConversationKey::Channel {
                    team_id,
                    channel_id,
                } => {
                    if let Some(ch) = self
                        .channels
                        .get_mut(&(team_id.clone(), channel_id.clone()))
                    {
                        ch.cursor = Some(cursor.clone());
                    }
                }
            }
        }
    }
}

pub fn chat_to_conversation(chat: &ChatFollowInfo) -> Conversation {
    if let Some(topic) = chat.topic.as_deref()
        && !topic.is_empty()
    {
        Conversation::GroupWithTopic {
            topic: topic.to_string(),
        }
    } else if chat.chat_type == "oneOnOne" {
        Conversation::Dm
    } else {
        Conversation::GroupNoTopic {
            participants: chat.member_names.clone(),
        }
    }
}

pub fn channel_to_conversation(ch: &ChannelFollowInfo) -> Conversation {
    Conversation::Channel {
        name: ch.channel_name.clone(),
        team: if ch.disambiguate_team {
            Some(ch.team_name.clone())
        } else {
            None
        },
    }
}

fn iso_time_to_hhmm(iso: &str) -> String {
    DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| "--:--".to_string())
}

fn graph_message_to_event(
    msg: GraphMessage,
    conversation_key: ConversationKey,
    conversation: Conversation,
    self_id: &str,
) -> Option<IncomingEvent> {
    if let Some(mt) = &msg.message_type
        && mt != "message"
    {
        return None;
    }

    let deleted = msg.deleted_date_time.is_some();

    let (sender_id, sender_name) = if let Some(from) = &msg.from
        && let Some(u) = &from.user
    {
        (
            u.id.clone(),
            u.display_name.clone().unwrap_or_else(|| u.id.clone()),
        )
    } else {
        (String::new(), "(unknown)".to_string())
    };

    let time_hhmm = msg
        .last_modified_date_time
        .as_deref()
        .or(msg.created_date_time.as_deref())
        .map(iso_time_to_hhmm)
        .unwrap_or_else(|| "--:--".to_string());

    let body_html = msg
        .body
        .as_ref()
        .map(|b| b.content.clone())
        .unwrap_or_default();

    let graph_says_mentioned = msg.mentions.iter().any(|m| {
        m.mentioned
            .as_ref()
            .and_then(|x| x.user.as_ref())
            .map(|u| u.id == self_id)
            .unwrap_or(false)
    });

    Some(IncomingEvent {
        conversation_key,
        conversation,
        message_id: msg.id,
        sender_id,
        sender_name,
        time_hhmm,
        body_html,
        deleted,
        graph_says_mentioned,
    })
}

async fn poll_chat_once(
    graph: &GraphClient,
    chat: &ChatFollowInfo,
    stream: &Stream,
    self_id: &str,
    handlers: Option<&PostHandlers>,
) -> Result<Option<String>, GraphError> {
    let conv = chat_to_conversation(chat);
    let key = ConversationKey::Chat {
        id: chat.id.clone(),
    };

    let mut cursor = chat.cursor.clone();
    let is_bootstrap = cursor.is_none();
    let mut messages: Vec<GraphMessage> = Vec::new();
    let request_start =
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    loop {
        let resp: MessagesDelta = graph
            .chat_messages_delta(&chat.id, cursor.as_deref())
            .await?;
        messages.extend(resp.value);
        if let Some(next) = resp.next_link {
            cursor = Some(next);
            continue;
        }
        break;
    }

    let new_cursor = messages
        .iter()
        .filter_map(|m| {
            m.last_modified_date_time
                .as_deref()
                .or(m.created_date_time.as_deref())
        })
        .max()
        .map(String::from)
        .unwrap_or(request_start);

    if !is_bootstrap {
        for msg in messages {
            if let Some(event) = graph_message_to_event(msg, key.clone(), conv.clone(), self_id) {
                dispatch(stream, event, &conv, self_id, handlers);
            }
        }
    }

    Ok(Some(new_cursor))
}

async fn poll_channel_once(
    graph: &GraphClient,
    ch: &ChannelFollowInfo,
    stream: &Stream,
    self_id: &str,
    handlers: Option<&PostHandlers>,
) -> Result<Option<String>, GraphError> {
    let conv = channel_to_conversation(ch);
    let key = ConversationKey::Channel {
        team_id: ch.team_id.clone(),
        channel_id: ch.channel_id.clone(),
    };

    let mut cursor = ch.cursor.clone();
    let is_bootstrap = cursor.is_none();
    let mut messages: Vec<GraphMessage> = Vec::new();
    let delta_link;

    loop {
        let resp: MessagesDelta = graph
            .channel_messages_delta(&ch.team_id, &ch.channel_id, cursor.as_deref())
            .await?;
        messages.extend(resp.value);
        if let Some(next) = resp.next_link {
            cursor = Some(next);
            continue;
        }
        delta_link = resp.delta_link;
        break;
    }

    if !is_bootstrap {
        for msg in messages {
            if let Some(event) = graph_message_to_event(msg, key.clone(), conv.clone(), self_id) {
                dispatch(stream, event, &conv, self_id, handlers);
            }
        }
    }

    Ok(delta_link)
}

fn dispatch(
    stream: &Stream,
    event: IncomingEvent,
    conv: &Conversation,
    self_id: &str,
    handlers: Option<&PostHandlers>,
) {
    let sender_name = event.sender_name.clone();
    let body_html = event.body_html.clone();
    let kind = stream.on_incoming(event);
    let Some(h) = handlers else {
        return;
    };
    match kind {
        NotificationKind::Mention => {
            let (preview, _) = html_to_text(&body_html, self_id);
            let _ = h.notifier.notify(&prefix_for(conv), &sender_name, &preview);
            h.counters.add_mention();
            let _ = h.counters.write();
        }
        NotificationKind::NewOther => {
            h.counters.add_unread();
            let _ = h.counters.write();
        }
        NotificationKind::None => {}
    }
}

pub async fn message_poll_tick(
    graph: &GraphClient,
    follow: &Arc<StdMutex<FollowState>>,
    stream: &Stream,
    self_id: &str,
    cursors_path: &Path,
    handlers: Option<&PostHandlers>,
) -> bool {
    let mut any_error = false;

    let chat_list: Vec<ChatFollowInfo> = {
        follow.lock().unwrap().chats.values().cloned().collect()
    };

    for chat in &chat_list {
        match poll_chat_once(graph, chat, stream, self_id, handlers).await {
            Ok(new_cursor) => {
                if let Some(nc) = new_cursor {
                    let mut state = follow.lock().unwrap();
                    if let Some(c) = state.chats.get_mut(&chat.id) {
                        c.cursor = Some(nc);
                    }
                }
            }
            Err(GraphError::CursorExpired) => {
                let mut state = follow.lock().unwrap();
                if let Some(c) = state.chats.get_mut(&chat.id) {
                    c.cursor = None;
                }
            }
            Err(e) => {
                eprintln!("chat poll error {}: {e}", chat.id);
                any_error = true;
            }
        }
    }

    let channel_list: Vec<ChannelFollowInfo> = {
        follow.lock().unwrap().channels.values().cloned().collect()
    };

    for ch in &channel_list {
        let key = (ch.team_id.clone(), ch.channel_id.clone());
        match poll_channel_once(graph, ch, stream, self_id, handlers).await {
            Ok(new_cursor) => {
                if let Some(nc) = new_cursor {
                    let mut state = follow.lock().unwrap();
                    if let Some(c) = state.channels.get_mut(&key) {
                        c.cursor = Some(nc);
                    }
                }
            }
            Err(GraphError::CursorExpired) => {
                let mut state = follow.lock().unwrap();
                if let Some(c) = state.channels.get_mut(&key) {
                    c.cursor = None;
                }
            }
            Err(e) => {
                eprintln!("channel poll error {}/{}: {e}", ch.team_id, ch.channel_id);
                any_error = true;
            }
        }
    }

    if !any_error {
        let cursors = follow.lock().unwrap().to_cursors();
        if let Err(e) = cursors.save_to(cursors_path) {
            eprintln!("failed to persist cursors: {e}");
        }
    }

    !any_error
}

pub async fn run_message_poll(
    graph: Arc<GraphClient>,
    follow: Arc<StdMutex<FollowState>>,
    stream: Arc<Stream>,
    self_id: String,
    cursors_path: PathBuf,
    handlers: Option<Arc<PostHandlers>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut backoff_secs: u64 = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let ok = message_poll_tick(
            &graph,
            &follow,
            &stream,
            &self_id,
            &cursors_path,
            handlers.as_deref(),
        )
        .await;

        let sleep_secs = if ok {
            backoff_secs = 0;
            MESSAGE_POLL_INTERVAL.as_secs()
        } else {
            backoff_secs = next_backoff(backoff_secs);
            backoff_secs
        };

        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

fn next_backoff(current: u64) -> u64 {
    match current {
        0 => 5,
        5 => 10,
        10 => 30,
        _ => 60,
    }
}

pub async fn run_discovery_poll(
    graph: Arc<GraphClient>,
    follow: Arc<StdMutex<FollowState>>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        match graph.list_chats().await {
            Ok(chats) => {
                let mut state = follow.lock().unwrap();
                for chat in chats {
                    state
                        .chats
                        .entry(chat.id.clone())
                        .or_insert_with(|| ChatFollowInfo {
                            id: chat.id.clone(),
                            topic: chat.topic.clone(),
                            chat_type: chat.chat_type.clone(),
                            member_names: chat
                                .members
                                .iter()
                                .filter_map(|m| m.display_name.clone())
                                .collect(),
                            cursor: None,
                        });
                }
            }
            Err(e) => {
                eprintln!("discovery error: {e}");
            }
        }

        tokio::time::sleep(DISCOVERY_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Tokens;
    use crate::stream::{LinePrinter, Stream};
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct CapturePrinter(Arc<StdMutex<Vec<String>>>);
    impl LinePrinter for CapturePrinter {
        fn print_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_string());
        }
    }

    #[tokio::test]
    async fn tick_fetches_feeds_stream_and_saves_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/chat1/messages/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    {
                        "id": "m1",
                        "messageType": "message",
                        "from": {"user": {"id": "alice", "displayName": "Alice"}},
                        "body": {"contentType": "html", "content": "<p>hello world</p>"},
                        "createdDateTime": "2021-01-01T12:00:00Z"
                    }
                ],
                "@odata.deltaLink": "https://graph.example/delta-next"
            })))
            .mount(&server)
            .await;

        let tokens = Arc::new(TokioMutex::new(Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        }));
        let graph = GraphClient::new(
            tokens,
            server.uri(),
            format!("{}/oauth2/v2.0/token", server.uri()),
            "client".into(),
        );

        let sink = Arc::new(StdMutex::new(Vec::new()));
        let printer: Arc<dyn LinePrinter> = Arc::new(CapturePrinter(sink.clone()));
        let stream = Stream::new(printer, "self".into(), Arc::new(|| 200));

        let mut state = FollowState::default();
        state.chats.insert(
            "chat1".into(),
            ChatFollowInfo {
                id: "chat1".into(),
                topic: None,
                chat_type: "oneOnOne".into(),
                member_names: vec![],
                cursor: Some(format!(
                    "{}/chats/chat1/messages/delta?$deltatoken=prev",
                    server.uri()
                )),
            },
        );
        let follow = Arc::new(StdMutex::new(state));

        let tmpdir = std::env::temp_dir().join(format!(
            "teams-tui-poll-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cursors_path = tmpdir.join("cursors.json");

        let ok = message_poll_tick(&graph, &follow, &stream, "self", &cursors_path, None).await;
        assert!(ok, "tick should succeed");

        let lines = sink.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "expected one printed line, got {lines:?}");
        assert!(lines[0].contains("hello world"));
        assert!(lines[0].contains("Alice"));

        let state = follow.lock().unwrap();
        assert_eq!(
            state.chats["chat1"].cursor.as_deref(),
            Some("2021-01-01T12:00:00Z")
        );
        drop(state);

        assert!(cursors_path.exists(), "cursors file should exist");
        let loaded = Cursors::load_from(&cursors_path).unwrap();
        assert_eq!(loaded.entries.len(), 1);

        let _ = std::fs::remove_dir_all(&tmpdir);
    }
}
