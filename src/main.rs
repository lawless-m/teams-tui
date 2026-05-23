//! teams-tui — minimal terminal Teams client. See plan-docs/SPEC.md.

mod auth;
mod config;
mod cursors;
mod graph;
mod notify;
mod poll;
mod render;
mod stream;
mod tags;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::Mutex as TokioMutex;

use auth::Tokens;
use config::Config;
use cursors::{ConversationKey, Cursors};
use graph::GraphClient;
use notify::{Notifier, StatusCounters};
use poll::{
    ChannelFollowInfo, ChatFollowInfo, FollowState, PostHandlers, chat_to_conversation,
    channel_to_conversation, run_discovery_poll, run_message_poll,
};
use stream::{LinePrinter, Stream};
use tags::parse_reply;

struct RustylinePrinter {
    inner: StdMutex<Box<dyn rustyline::ExternalPrinter + Send + Sync>>,
}

impl LinePrinter for RustylinePrinter {
    fn print_line(&self, line: &str) {
        if let Ok(mut p) = self.inner.lock() {
            let _ = p.print(format!("{line}\n"));
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("teams-tui: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config_path = directories::ProjectDirs::from("", "", "teams-tui")
        .map(|d| d.config_dir().join("config.toml"))
        .context("could not resolve config dir")?;
    let cfg = Config::load(&config_path)?;

    let token_path = auth::default_token_path().context("could not resolve token path")?;
    let mut tokens = match Tokens::load_from(&token_path)? {
        Some(t) => t,
        None => {
            let t = auth::device_code_flow(&cfg.auth.tenant_id, &cfg.auth.client_id).await?;
            t.save_to(&token_path)?;
            t
        }
    };
    if !tokens.is_fresh() {
        match tokens
            .ensure_fresh(&cfg.auth.tenant_id, &cfg.auth.client_id)
            .await
        {
            Ok(()) => tokens.save_to(&token_path)?,
            Err(_) => {
                tokens = auth::device_code_flow(&cfg.auth.tenant_id, &cfg.auth.client_id).await?;
                tokens.save_to(&token_path)?;
            }
        }
    }

    let tokens = Arc::new(TokioMutex::new(tokens));
    let graph = Arc::new(GraphClient::new(
        tokens.clone(),
        "https://graph.microsoft.com/v1.0".into(),
        auth::microsoft_token_url(&cfg.auth.tenant_id),
        cfg.auth.client_id.clone(),
    ));

    let me = graph
        .me()
        .await
        .map_err(|e| anyhow::anyhow!("Graph /me failed: {e}"))?;
    let self_id = me.id.clone();

    let mut channels: HashMap<(String, String), ChannelFollowInfo> = HashMap::new();
    if !cfg.follow.channels.is_empty() {
        let teams = graph
            .joined_teams()
            .await
            .map_err(|e| anyhow::anyhow!("joinedTeams failed: {e}"))?;
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for fc in &cfg.follow.channels {
            *name_counts.entry(fc.channel.clone()).or_insert(0) += 1;
        }
        for fc in &cfg.follow.channels {
            let team = teams
                .iter()
                .find(|t| t.display_name == fc.team)
                .ok_or_else(|| anyhow::anyhow!("config: team '{}' not found", fc.team))?;
            let chans = graph
                .team_channels(&team.id)
                .await
                .map_err(|e| anyhow::anyhow!("channels for '{}' failed: {e}", team.display_name))?;
            let ch = chans.iter().find(|c| c.display_name == fc.channel).ok_or_else(|| {
                anyhow::anyhow!(
                    "config: channel '{}' not found in team '{}'",
                    fc.channel,
                    fc.team
                )
            })?;
            channels.insert(
                (team.id.clone(), ch.id.clone()),
                ChannelFollowInfo {
                    team_id: team.id.clone(),
                    channel_id: ch.id.clone(),
                    team_name: team.display_name.clone(),
                    channel_name: ch.display_name.clone(),
                    cursor: None,
                    disambiguate_team: name_counts[&fc.channel] > 1,
                },
            );
        }
    }

    let mut chats: HashMap<String, ChatFollowInfo> = HashMap::new();
    let chats_raw = graph
        .list_chats()
        .await
        .map_err(|e| anyhow::anyhow!("list_chats failed: {e}"))?;
    for chat in chats_raw {
        chats.insert(
            chat.id.clone(),
            ChatFollowInfo {
                id: chat.id.clone(),
                topic: chat.topic.clone(),
                chat_type: chat.chat_type.clone(),
                member_names: chat
                    .members
                    .iter()
                    .filter_map(|m| m.display_name.clone())
                    .collect(),
                cursor: None,
            },
        );
    }

    let mut follow_state = FollowState {
        chats,
        channels,
        chat_tags: HashMap::new(),
    };

    let cursors_path = cursors::default_path().context("could not resolve cursors path")?;
    let loaded_cursors = Cursors::load_from(&cursors_path)?;
    follow_state.apply_cursors(&loaded_cursors);

    let n_chats = follow_state.chats.len();
    let n_channels = follow_state.channels.len();
    let follow = Arc::new(StdMutex::new(follow_state));

    let mut editor: DefaultEditor =
        DefaultEditor::new().context("failed to create rustyline editor")?;
    let ext_printer = editor
        .create_external_printer()
        .context("failed to create external printer")?;
    let boxed: Box<dyn rustyline::ExternalPrinter + Send + Sync> = Box::new(ext_printer);
    let printer: Arc<dyn LinePrinter> = Arc::new(RustylinePrinter {
        inner: StdMutex::new(boxed),
    });

    let width_fn: Arc<dyn Fn() -> usize + Send + Sync> = Arc::new(|| 80);
    let stream = Arc::new(Stream::new(printer, self_id.clone(), width_fn));

    let handlers = Arc::new(PostHandlers {
        notifier: Notifier::new(),
        counters: StatusCounters::new(cfg.notifications.status_file.clone()),
    });
    let _ = handlers.counters.write();

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    println!("teams-tui ready — following {n_chats} chats and {n_channels} channels");

    let poll_msg_handle = {
        let graph = graph.clone();
        let follow = follow.clone();
        let stream = stream.clone();
        let self_id = self_id.clone();
        let cursors_path = cursors_path.clone();
        let handlers = handlers.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            run_message_poll(
                graph,
                follow,
                stream,
                self_id,
                cursors_path,
                Some(handlers),
                shutdown,
            )
            .await;
        })
    };
    let poll_disc_handle = {
        let graph = graph.clone();
        let follow = follow.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            run_discovery_poll(graph, follow, shutdown).await;
        })
    };

    prompt_loop(
        &mut editor,
        &stream,
        &graph,
        &follow,
        &handlers,
        &tokens,
        &token_path,
        &cfg,
        &shutdown,
    )
    .await;

    shutdown.store(true, Ordering::Relaxed);
    let _ = poll_msg_handle.await;
    let _ = poll_disc_handle.await;

    let cursors = follow.lock().unwrap().to_cursors();
    let _ = cursors.save_to(&cursors_path);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn prompt_loop(
    editor: &mut DefaultEditor,
    stream: &Arc<Stream>,
    graph: &Arc<GraphClient>,
    follow: &Arc<StdMutex<FollowState>>,
    handlers: &Arc<PostHandlers>,
    tokens: &Arc<TokioMutex<Tokens>>,
    token_path: &std::path::Path,
    cfg: &Config,
    shutdown: &Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        handlers.counters.reset();
        let _ = handlers.counters.write();

        let result = tokio::task::block_in_place(|| editor.readline("> "));
        match result {
            Ok(input) => {
                let trimmed = input.trim();
                if trimmed == "/quit" {
                    shutdown.store(true, Ordering::Relaxed);
                    return;
                }
                if trimmed == "/reauth" {
                    match auth::device_code_flow(&cfg.auth.tenant_id, &cfg.auth.client_id).await {
                        Ok(new_tokens) => {
                            if let Err(e) = new_tokens.save_to(token_path) {
                                eprintln!("teams-tui: failed to save token: {e:#}");
                            } else {
                                *tokens.lock().await = new_tokens;
                                eprintln!("teams-tui: reauth complete");
                            }
                        }
                        Err(e) => {
                            eprintln!("teams-tui: reauth failed: {e:#}");
                        }
                    }
                    continue;
                }
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "/chats" {
                    handle_chats_list(stream, follow);
                    continue;
                }
                if let Some((tag, body)) = parse_chat_send(trimmed) {
                    handle_chat_send(tag, body, stream, graph, follow).await;
                    continue;
                }
                handle_reply(trimmed, stream, graph, follow).await;
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                eprintln!("teams-tui: readline error: {e:#}");
                return;
            }
        }
    }
}

async fn handle_reply(
    input: &str,
    stream: &Stream,
    graph: &GraphClient,
    follow: &Arc<StdMutex<FollowState>>,
) {
    let (tag, body) = parse_reply(input);
    let Some(msg_ref) = stream.resolve_reply(tag) else {
        eprintln!("teams-tui: no message to reply to");
        return;
    };
    let conv = {
        let state = follow.lock().unwrap();
        match &msg_ref.conversation_key {
            ConversationKey::Chat { id } => state.chats.get(id).map(chat_to_conversation),
            ConversationKey::Channel {
                team_id,
                channel_id,
            } => state
                .channels
                .get(&(team_id.clone(), channel_id.clone()))
                .map(channel_to_conversation),
        }
    };
    let Some(conv) = conv else {
        eprintln!("teams-tui: cannot resolve conversation for reply target");
        return;
    };
    stream.send_local_echo(&conv, &msg_ref.conversation_key, body);

    let send_result = match &msg_ref.conversation_key {
        ConversationKey::Chat { id } => graph.send_chat_message(id, body).await.map(|_| ()),
        ConversationKey::Channel {
            team_id,
            channel_id,
        } => graph
            .send_channel_message(team_id, channel_id, body)
            .await
            .map(|_| ()),
    };
    if let Err(e) = send_result {
        eprintln!("teams-tui: send failed: {e}");
    }
}

fn chat_label(chat: &ChatFollowInfo) -> String {
    if let Some(topic) = chat.topic.as_deref()
        && !topic.is_empty()
    {
        return format!("#{topic}");
    }
    if chat.chat_type == "oneOnOne" {
        return format!("DM: {}", chat.member_names.join(", "));
    }
    format!("group: {}", chat.member_names.join(", "))
}

fn parse_chat_send(input: &str) -> Option<(char, &str)> {
    let bytes = input.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return None;
    }
    let tag = bytes[1] as char;
    if !tag.is_ascii_uppercase() {
        return None;
    }
    if bytes.len() == 2 {
        return Some((tag, ""));
    }
    if bytes[2] != b' ' {
        return None;
    }
    Some((tag, input[3..].trim()))
}

fn handle_chats_list(stream: &Stream, follow: &Arc<StdMutex<FollowState>>) {
    let lines: Vec<String> = {
        let mut state = follow.lock().unwrap();
        state.assign_chat_tags();
        let mut pairs: Vec<(char, String)> = state
            .chat_tags
            .iter()
            .filter_map(|(t, id)| state.chats.get(id).map(|c| (*t, chat_label(c))))
            .collect();
        pairs.sort_by_key(|(t, _)| *t);
        pairs
            .into_iter()
            .map(|(t, l)| format!("[{t}] {l}"))
            .collect()
    };
    if lines.is_empty() {
        stream.print_line("teams-tui: no chats yet");
        return;
    }
    for line in lines {
        stream.print_line(&line);
    }
}

async fn handle_chat_send(
    tag: char,
    body: &str,
    stream: &Stream,
    graph: &GraphClient,
    follow: &Arc<StdMutex<FollowState>>,
) {
    let (chat_id, conv) = {
        let state = follow.lock().unwrap();
        match state.chat_for_tag(tag) {
            Some(chat) => (chat.id.clone(), chat_to_conversation(chat)),
            None => {
                drop(state);
                eprintln!("teams-tui: no chat with tag {tag} (try /chats)");
                return;
            }
        }
    };
    if body.is_empty() {
        eprintln!("teams-tui: empty message");
        return;
    }
    let key = ConversationKey::Chat {
        id: chat_id.clone(),
    };
    stream.send_local_echo(&conv, &key, body);
    if let Err(e) = graph.send_chat_message(&chat_id, body).await {
        eprintln!("teams-tui: send failed: {e}");
    }
}
