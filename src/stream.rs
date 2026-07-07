//! Output stream: local echo, dedupe ring, ExternalPrinter.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::hash::Hasher;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cursors::ConversationKey;
use crate::render::{Conversation, format_message, html_to_text, prefix_for};
use crate::tags::{MessageRef, TagTable};

pub const SEEN_PER_CONV_LIMIT: usize = 500;
pub const DEDUPE_RING_LIMIT: usize = 32;
pub const DEDUPE_TTL: Duration = Duration::from_secs(30);

pub trait LinePrinter: Send + Sync {
    fn print_line(&self, line: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    None,
    NewOther,
    Mention,
}

/// What `on_incoming` did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub notification: NotificationKind,
    /// True when the message was displayed as a *new* line (tag assigned) — i.e.
    /// a fresh message from someone else, or from you on another Teams client.
    /// False for edits, deletes, and our own echoes looping back through the
    /// poll. This is the signal bots fire on, so a bot never sees its own reply.
    pub fresh: bool,
}

impl Outcome {
    fn quiet() -> Self {
        Self {
            notification: NotificationKind::None,
            fresh: false,
        }
    }
}

pub struct IncomingEvent {
    pub conversation_key: ConversationKey,
    pub conversation: Conversation,
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub time_hhmm: String,
    pub body_html: String,
    pub deleted: bool,
    pub graph_says_mentioned: bool,
}

struct DedupeEntry {
    conversation_key: ConversationKey,
    body_hash: u64,
    at: Instant,
}

struct StreamInner {
    tags: TagTable,
    seen: HashMap<ConversationKey, VecDeque<String>>,
    ring: VecDeque<DedupeEntry>,
}

pub struct Stream {
    printer: Arc<dyn LinePrinter>,
    self_id: String,
    width_fn: Arc<dyn Fn() -> usize + Send + Sync>,
    inner: Mutex<StreamInner>,
}

impl Stream {
    pub fn new(
        printer: Arc<dyn LinePrinter>,
        self_id: String,
        width_fn: Arc<dyn Fn() -> usize + Send + Sync>,
    ) -> Self {
        Self {
            printer,
            self_id,
            width_fn,
            inner: Mutex::new(StreamInner {
                tags: TagTable::new(),
                seen: HashMap::new(),
                ring: VecDeque::new(),
            }),
        }
    }

    pub fn print_line(&self, line: &str) {
        self.printer.print_line(line);
    }

    pub fn send_local_echo(
        &self,
        conversation: &Conversation,
        conversation_key: &ConversationKey,
        text: &str,
    ) {
        let prefix = prefix_for(conversation);
        self.printer.print_line(&format!("> {prefix}:you: {text}"));

        let mut inner = self.inner.lock().unwrap();
        prune_ring(&mut inner.ring);
        if inner.ring.len() >= DEDUPE_RING_LIMIT {
            inner.ring.pop_front();
        }
        inner.ring.push_back(DedupeEntry {
            conversation_key: conversation_key.clone(),
            body_hash: hash_body(text),
            at: Instant::now(),
        });
    }

    pub fn on_incoming(&self, event: IncomingEvent) -> Outcome {
        let mut inner = self.inner.lock().unwrap();
        prune_ring(&mut inner.ring);

        let is_self = event.sender_id == self.self_id;
        let prefix = prefix_for(&event.conversation);

        let seen_list = inner
            .seen
            .entry(event.conversation_key.clone())
            .or_default();
        let is_resend = seen_list.iter().any(|id| id == &event.message_id);
        if !is_resend {
            seen_list.push_back(event.message_id.clone());
            while seen_list.len() > SEEN_PER_CONV_LIMIT {
                seen_list.pop_front();
            }
        }

        if event.deleted {
            if is_self {
                return Outcome::quiet();
            }
            self.printer.print_line(&format!(
                "[{} deleted a message in {prefix}]",
                event.sender_name
            ));
            return Outcome::quiet();
        }

        let (plain, mention_via_html) = html_to_text(&event.body_html, &self.self_id);

        if is_resend {
            if is_self {
                return Outcome::quiet();
            }
            self.printer.print_line(&format!(
                "[edit from {} in {prefix}]: {plain}",
                event.sender_name
            ));
            return Outcome::quiet();
        }

        // Our own echo looping back through the poll: drop it. This is what keeps
        // a bot from seeing its own reply (and what keeps messages you type in
        // teams-tui from firing bots — send from another client for that).
        if is_self {
            let hash = hash_body(&plain);
            if let Some(pos) = inner
                .ring
                .iter()
                .position(|e| e.conversation_key == event.conversation_key && e.body_hash == hash)
            {
                inner.ring.remove(pos);
                return Outcome::quiet();
            }
        }

        let msg_ref = MessageRef {
            conversation_key: event.conversation_key.clone(),
            message_id: event.message_id.clone(),
            plaintext_body: plain.clone(),
        };
        let tag = inner.tags.assign(msg_ref);
        let width = (self.width_fn)();
        let line = format_message(
            &event.time_hhmm,
            &prefix,
            &event.sender_name,
            tag,
            &plain,
            width,
        );
        self.printer.print_line(&line);

        // A freshly-displayed message. Bots fire on these (see `Outcome::fresh`),
        // including ones you send from another Teams client, which are `is_self`
        // but weren't echoed locally so they reach here rather than being deduped.
        let notification = if is_self {
            NotificationKind::None
        } else if matches!(event.conversation, Conversation::Dm)
            || event.graph_says_mentioned
            || mention_via_html
        {
            NotificationKind::Mention
        } else {
            NotificationKind::NewOther
        };
        Outcome {
            notification,
            fresh: true,
        }
    }

    pub fn resolve_reply(&self, tag: Option<char>) -> Option<MessageRef> {
        let inner = self.inner.lock().unwrap();
        match tag {
            Some(c) => inner.tags.lookup(c).cloned(),
            None => inner.tags.lookup_latest().cloned(),
        }
    }
}

fn prune_ring(ring: &mut VecDeque<DedupeEntry>) {
    let now = Instant::now();
    while let Some(front) = ring.front() {
        if now.duration_since(front.at) > DEDUPE_TTL {
            ring.pop_front();
        } else {
            break;
        }
    }
}

fn hash_body(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(s.as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    struct CapturePrinter(Arc<StdMutex<Vec<String>>>);
    impl LinePrinter for CapturePrinter {
        fn print_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_string());
        }
    }

    fn mk(self_id: &str) -> (Stream, Arc<StdMutex<Vec<String>>>) {
        let sink = Arc::new(StdMutex::new(Vec::new()));
        let printer: Arc<dyn LinePrinter> = Arc::new(CapturePrinter(sink.clone()));
        let stream = Stream::new(printer, self_id.into(), Arc::new(|| 200));
        (stream, sink)
    }

    fn dm_key() -> ConversationKey {
        ConversationKey::Chat {
            id: "chat-dm".into(),
        }
    }

    fn chan_key() -> ConversationKey {
        ConversationKey::Channel {
            team_id: "t".into(),
            channel_id: "ch".into(),
        }
    }

    fn chan_conv() -> Conversation {
        Conversation::Channel {
            name: "chan".into(),
            team: None,
        }
    }

    #[test]
    fn own_message_within_window_is_deduped() {
        let (s, sink) = mk("me");
        s.send_local_echo(&Conversation::Dm, &dm_key(), "hello");
        s.on_incoming(IncomingEvent {
            conversation_key: dm_key(),
            conversation: Conversation::Dm,
            message_id: "m1".into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>hello</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        let lines = sink.lock().unwrap();
        assert_eq!(
            lines.len(),
            1,
            "expected only local echo line, got {lines:?}"
        );
        assert!(lines[0].starts_with("> DM:you: hello"));
    }

    #[test]
    fn own_message_from_other_device_is_not_deduped() {
        let (s, sink) = mk("me");
        s.on_incoming(IncomingEvent {
            conversation_key: dm_key(),
            conversation: Conversation::Dm,
            message_id: "m1".into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>from phone</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        let lines = sink.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("from phone"));
    }

    #[test]
    fn new_edit_delete_sequence_produces_three_lines() {
        let (s, sink) = mk("me");

        s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>hi</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });

        s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:01".into(),
            body_html: "<p>hi there</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });

        s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:02".into(),
            body_html: String::new(),
            deleted: true,
            graph_says_mentioned: false,
        });

        let lines = sink.lock().unwrap();
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[0].contains("Alice"));
        assert!(lines[0].contains("[a]"));
        assert!(lines[0].contains("hi"));
        assert_eq!(lines[1], "[edit from Alice in #chan]: hi there");
        assert_eq!(lines[2], "[Alice deleted a message in #chan]");
    }

    #[test]
    fn dm_from_other_person_returns_mention_notification() {
        let (s, _sink) = mk("me");
        let outcome = s.on_incoming(IncomingEvent {
            conversation_key: dm_key(),
            conversation: Conversation::Dm,
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>hi</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        assert_eq!(outcome.notification, NotificationKind::Mention);
        assert!(outcome.fresh);
    }

    #[test]
    fn channel_without_mention_returns_new_other() {
        let (s, _sink) = mk("me");
        let outcome = s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>hi all</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        assert_eq!(outcome.notification, NotificationKind::NewOther);
        assert!(outcome.fresh);
    }

    #[test]
    fn channel_with_at_mention_of_self_returns_mention() {
        let (s, _sink) = mk("me");
        let outcome = s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            time_hhmm: "10:00".into(),
            body_html: r#"<p>hi <at id="me">Me</at></p>"#.into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        assert_eq!(outcome.notification, NotificationKind::Mention);
        assert!(outcome.fresh);
    }

    fn self_msg(id: &str, body: &str) -> IncomingEvent {
        IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: id.into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            time_hhmm: "10:00".into(),
            body_html: format!("<p>{body}</p>"),
            deleted: false,
            graph_says_mentioned: false,
        }
    }

    #[test]
    fn self_message_from_another_client_is_fresh() {
        // Sent from the phone: never locally echoed, so it reaches the display
        // path and is `fresh` — this is what lets it trigger a bot.
        let (s, _sink) = mk("me");
        let outcome = s.on_incoming(self_msg("m1", "!bot fortune"));
        assert!(outcome.fresh, "a self message we didn't echo must be fresh");
        assert_eq!(outcome.notification, NotificationKind::None);
    }

    #[test]
    fn own_echo_looping_back_is_not_fresh() {
        // A reply/bot-output we sent: echoed locally, so the poll's copy is
        // deduped and NOT fresh — a bot must never re-trigger on its own output.
        let (s, _sink) = mk("me");
        s.send_local_echo(&chan_conv(), &chan_key(), "fortune output");
        let outcome = s.on_incoming(self_msg("m1", "fortune output"));
        assert!(!outcome.fresh, "our own echo must not be fresh");
    }

    #[test]
    fn edits_and_deletes_are_not_fresh() {
        let (s, _sink) = mk("me");
        let first = s.on_incoming(IncomingEvent {
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            ..self_msg("m1", "hi")
        });
        assert!(first.fresh);
        let edit = s.on_incoming(IncomingEvent {
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            ..self_msg("m1", "hi there")
        });
        assert!(!edit.fresh, "an edit is not a fresh message");
        let del = s.on_incoming(IncomingEvent {
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            deleted: true,
            ..self_msg("m1", "")
        });
        assert!(!del.fresh, "a delete is not a fresh message");
    }

    #[test]
    fn own_edit_is_suppressed() {
        let (s, sink) = mk("me");
        s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            time_hhmm: "10:00".into(),
            body_html: "<p>v1</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        s.on_incoming(IncomingEvent {
            conversation_key: chan_key(),
            conversation: chan_conv(),
            message_id: "m1".into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            time_hhmm: "10:01".into(),
            body_html: "<p>v2</p>".into(),
            deleted: false,
            graph_says_mentioned: false,
        });
        let lines = sink.lock().unwrap();
        assert_eq!(
            lines.len(),
            1,
            "expected only the initial message, got {lines:?}"
        );
    }
}
