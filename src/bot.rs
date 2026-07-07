//! Exec bots: regex-match incoming messages, run a program, post its stdout.
//!
//! Safety model — these bots post as *you*, and that reply loops back through
//! the poll as an `is_self` message:
//!   * The message is passed to the program as data (argv + stdin), never
//!     interpolated into a shell string, so a crafted message can't inject a
//!     command.
//!   * Bots only run against messages the stream displayed as *fresh* (see
//!     `stream::Outcome::fresh`). A bot's own reply is locally echoed, so its
//!     looped-back copy is deduped and never fresh — that's what stops a program
//!     that echoes its trigger from looping. A message you send from another
//!     Teams client isn't echoed here, so it *is* fresh and does fire bots.
//!   * A per-conversation cooldown and an output cap bound a chatty channel.

#![allow(dead_code)]

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::config::BotConfig;
use crate::cursors::ConversationKey;
use crate::graph::GraphClient;
use crate::render::Conversation;
use crate::stream::Stream;

/// Most stdout we'll post back into a conversation.
const MAX_OUTPUT_BYTES: usize = 8 * 1024;

/// Read-only view of an incoming message handed to the engine. The caller only
/// passes messages that were displayed as fresh (see `stream::Outcome::fresh`),
/// which is what keeps a bot's own reply from reaching it — so there's no
/// `is_self` guard here.
pub struct BotContext<'a> {
    pub plaintext: &'a str,
    pub conversation: &'a Conversation,
    pub conversation_key: &'a ConversationKey,
}

/// A program to run and where to post its output, produced by a matched bot.
pub struct BotJob {
    pub key: ConversationKey,
    pub conv: Conversation,
    pub argv: Vec<String>,
    pub stdin: String,
    pub timeout: Duration,
    pub name: String,
}

struct Scope {
    channel: String,
    team: Option<String>,
}

struct ExecBot {
    name: String,
    trigger: Regex,
    command: Vec<String>,
    scope: Option<Scope>,
    timeout: Duration,
    cooldown: Duration,
}

impl ExecBot {
    /// True when this bot is allowed to fire in the given conversation. A scope
    /// only matches channels; `team` is best-effort (it only appears on the
    /// conversation when the channel name is ambiguous across teams).
    fn scope_matches(&self, conv: &Conversation) -> bool {
        let Some(scope) = &self.scope else {
            return true;
        };
        match conv {
            Conversation::Channel { name, team } => {
                name == &scope.channel
                    && match &scope.team {
                        Some(want) => team.as_deref() == Some(want.as_str()),
                        None => true,
                    }
            }
            _ => false,
        }
    }
}

/// Compiled set of bots plus the queue they push jobs onto. Matching is cheap
/// and synchronous; the actual program run happens in [`run_bot_jobs`].
pub struct BotEngine {
    bots: Vec<ExecBot>,
    last_fired: Mutex<HashMap<(usize, ConversationKey), Instant>>,
    tx: mpsc::UnboundedSender<BotJob>,
}

impl BotEngine {
    /// Compile `[[bot]]` config into an engine, or `None` if none are configured.
    /// Fails fast on a bad regex, empty command, or unknown kind.
    pub fn from_config(
        configs: &[BotConfig],
        tx: mpsc::UnboundedSender<BotJob>,
    ) -> Result<Option<Self>> {
        if configs.is_empty() {
            return Ok(None);
        }
        let mut bots = Vec::with_capacity(configs.len());
        for (i, c) in configs.iter().enumerate() {
            if c.kind != "exec" {
                bail!(
                    "bot #{i}: unknown kind '{}' (only 'exec' is supported)",
                    c.kind
                );
            }
            let Some(program) = c.command.first() else {
                bail!("bot #{i}: 'command' must list at least the program to run");
            };
            let trigger = Regex::new(&c.trigger)
                .with_context(|| format!("bot #{i}: invalid trigger regex '{}'", c.trigger))?;
            bots.push(ExecBot {
                name: format!("exec[{program}]"),
                trigger,
                command: c.command.clone(),
                scope: c.only.as_ref().map(|s| Scope {
                    channel: s.channel.clone(),
                    team: s.team.clone(),
                }),
                timeout: Duration::from_secs(c.timeout_secs),
                cooldown: Duration::from_secs(c.cooldown_secs),
            });
        }
        Ok(Some(Self {
            bots,
            last_fired: Mutex::new(HashMap::new()),
            tx,
        }))
    }

    /// Match `ctx` against every bot and enqueue a job for each that fires. Loop
    /// safety lives at the call site (only fresh, non-echoed messages get here),
    /// so a bot's own reply never reaches this method.
    pub fn on_message(&self, ctx: &BotContext) {
        let now = Instant::now();
        for (i, bot) in self.bots.iter().enumerate() {
            if !bot.scope_matches(ctx.conversation) {
                continue;
            }
            let Some(caps) = bot.trigger.captures(ctx.plaintext) else {
                continue;
            };
            if !self.cooldown_ok(i, bot, ctx.conversation_key, now) {
                continue;
            }
            // argv = configured command + each regex capture group (group 0, the
            // whole match, is skipped). Groups are pushed as separate args, so a
            // captured value is data — it can't spill into further arguments.
            let mut argv = bot.command.clone();
            for g in caps.iter().skip(1).flatten() {
                argv.push(g.as_str().to_string());
            }
            let job = BotJob {
                key: ctx.conversation_key.clone(),
                conv: ctx.conversation.clone(),
                argv,
                stdin: ctx.plaintext.to_string(),
                timeout: bot.timeout,
                name: bot.name.clone(),
            };
            // Receiver only drops at shutdown; a lost job then is fine.
            let _ = self.tx.send(job);
        }
    }

    /// True if this bot may fire in this conversation now, recording the firing.
    fn cooldown_ok(&self, idx: usize, bot: &ExecBot, key: &ConversationKey, now: Instant) -> bool {
        if bot.cooldown.is_zero() {
            return true;
        }
        let mut fired = self.last_fired.lock().unwrap();
        if let Some(&last) = fired.get(&(idx, key.clone()))
            && now.duration_since(last) < bot.cooldown
        {
            return false;
        }
        fired.insert((idx, key.clone()), now);
        true
    }
}

/// Drain the job queue: run each program and post its stdout back. Runs as its
/// own task so a slow program never stalls the poll loop.
pub async fn run_bot_jobs(
    mut rx: mpsc::UnboundedReceiver<BotJob>,
    graph: Arc<GraphClient>,
    stream: Arc<Stream>,
) {
    while let Some(job) = rx.recv().await {
        match run_program(&job).await {
            Ok(out) => {
                let text = clip_output(&out);
                if text.is_empty() {
                    continue;
                }
                // Echo like a manual reply so it dedupes when it loops back.
                stream.send_local_echo(&job.conv, &job.key, &text);
                let sent = match &job.key {
                    ConversationKey::Chat { id } => {
                        graph.send_chat_message(id, &text).await.map(|_| ())
                    }
                    ConversationKey::Channel {
                        team_id,
                        channel_id,
                    } => graph
                        .send_channel_message(team_id, channel_id, &text)
                        .await
                        .map(|_| ()),
                };
                if let Err(e) = sent {
                    eprintln!("teams-tui: bot {} send failed: {e}", job.name);
                }
            }
            Err(e) => eprintln!("teams-tui: bot {} failed: {e}", job.name),
        }
    }
}

/// Spawn `job.argv`, feed the message to stdin, and return stdout. The program
/// is killed if it outlives `job.timeout`. No shell is involved.
async fn run_program(job: &BotJob) -> Result<String> {
    let mut cmd = tokio::process::Command::new(&job.argv[0]);
    cmd.args(&job.argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {}", job.argv[0]))?;

    // Write stdin from a separate task so a program that streams stdout while
    // we're still writing can't deadlock on a full pipe.
    if let Some(mut si) = child.stdin.take() {
        let data = job.stdin.clone();
        tokio::spawn(async move {
            let _ = si.write_all(data.as_bytes()).await;
            // dropping `si` closes stdin, signalling EOF to the program.
        });
    }

    let output = tokio::time::timeout(job.timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("timed out after {}s", job.timeout.as_secs()))?
        .context("failed to collect program output")?;

    if !output.status.success() {
        bail!("{}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Trim the trailing newline programs add and cap the length we post.
fn clip_output(s: &str) -> String {
    let trimmed = s.trim_end();
    if trimmed.len() <= MAX_OUTPUT_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chan(name: &str, team: Option<&str>) -> Conversation {
        Conversation::Channel {
            name: name.into(),
            team: team.map(String::from),
        }
    }

    fn chan_key() -> ConversationKey {
        ConversationKey::Channel {
            team_id: "t".into(),
            channel_id: "ch".into(),
        }
    }

    /// Build an engine from one exec bot spec and hand back the receiver.
    fn engine(cfg: BotConfig) -> (BotEngine, mpsc::UnboundedReceiver<BotJob>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let eng = BotEngine::from_config(&[cfg], tx).unwrap().unwrap();
        (eng, rx)
    }

    fn spec(trigger: &str, command: &[&str]) -> BotConfig {
        BotConfig {
            kind: "exec".into(),
            trigger: trigger.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 10,
            cooldown_secs: 0,
            only: None,
        }
    }

    #[test]
    fn capture_groups_become_argv_and_message_is_stdin() {
        let (eng, mut rx) = engine(spec(r"^!deploy (\w+) (\w+)", &["/opt/deploy", "--ci"]));
        eng.on_message(&BotContext {
            plaintext: "!deploy staging eu",
            conversation: &chan("general", None),
            conversation_key: &chan_key(),
        });
        let job = rx.try_recv().expect("bot should have fired");
        assert_eq!(job.argv, vec!["/opt/deploy", "--ci", "staging", "eu"]);
        assert_eq!(job.stdin, "!deploy staging eu");
    }

    #[test]
    fn no_capture_groups_passes_command_verbatim() {
        let (eng, mut rx) = engine(spec("ping", &["/bin/echo", "pong"]));
        eng.on_message(&BotContext {
            plaintext: "please ping me",
            conversation: &chan("general", None),
            conversation_key: &chan_key(),
        });
        let job = rx.try_recv().unwrap();
        assert_eq!(job.argv, vec!["/bin/echo", "pong"]);
    }

    #[test]
    fn non_matching_message_does_nothing() {
        let (eng, mut rx) = engine(spec("^!build", &["/bin/echo"]));
        eng.on_message(&BotContext {
            plaintext: "just chatting",
            conversation: &chan("general", None),
            conversation_key: &chan_key(),
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn scope_limits_to_named_channel() {
        let mut cfg = spec("ping", &["/bin/echo"]);
        cfg.only = Some(crate::config::BotScope {
            channel: "ops".into(),
            team: None,
        });
        let (eng, mut rx) = engine(cfg);

        eng.on_message(&BotContext {
            plaintext: "ping",
            conversation: &chan("general", None),
            conversation_key: &chan_key(),
        });
        assert!(rx.try_recv().is_err(), "wrong channel must not fire");

        eng.on_message(&BotContext {
            plaintext: "ping",
            conversation: &chan("ops", None),
            conversation_key: &chan_key(),
        });
        assert!(rx.try_recv().is_ok(), "named channel should fire");
    }

    #[test]
    fn scoped_bot_ignores_non_channel_conversations() {
        let mut cfg = spec("ping", &["/bin/echo"]);
        cfg.only = Some(crate::config::BotScope {
            channel: "ops".into(),
            team: None,
        });
        let (eng, mut rx) = engine(cfg);
        eng.on_message(&BotContext {
            plaintext: "ping",
            conversation: &Conversation::Dm,
            conversation_key: &ConversationKey::Chat { id: "c".into() },
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cooldown_suppresses_rapid_refire() {
        let mut cfg = spec("ping", &["/bin/echo"]);
        cfg.cooldown_secs = 3600;
        let (eng, mut rx) = engine(cfg);
        let conv = chan("general", None);
        let key = chan_key();
        for _ in 0..2 {
            eng.on_message(&BotContext {
                plaintext: "ping",
                conversation: &conv,
                conversation_key: &key,
            });
        }
        assert!(rx.try_recv().is_ok(), "first fires");
        assert!(rx.try_recv().is_err(), "second is within cooldown");
    }

    #[test]
    fn bad_regex_is_rejected_at_construction() {
        let (tx, _rx) = mpsc::unbounded_channel();
        // `BotEngine` isn't `Debug`, so match rather than `unwrap_err`.
        match BotEngine::from_config(&[spec("(unclosed", &["/bin/echo"])], tx) {
            Ok(_) => panic!("expected a compile error for a bad regex"),
            Err(e) => assert!(format!("{e:#}").contains("invalid trigger regex")),
        }
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut cfg = spec("x", &["/bin/echo"]);
        cfg.kind = "llm".into();
        match BotEngine::from_config(&[cfg], tx) {
            Ok(_) => panic!("expected an error for an unknown kind"),
            Err(e) => assert!(format!("{e:#}").contains("unknown kind")),
        }
    }

    fn job(argv: &[&str], stdin: &str, timeout_secs: u64) -> BotJob {
        BotJob {
            key: chan_key(),
            conv: chan("general", None),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            stdin: stdin.into(),
            timeout: Duration::from_secs(timeout_secs),
            name: "test".into(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_program_pipes_stdin_through_cat() {
        let out = run_program(&job(&["/bin/cat"], "hello from stdin", 10))
            .await
            .unwrap();
        assert_eq!(out, "hello from stdin");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_program_passes_captured_args() {
        let out = run_program(&job(&["/bin/echo", "staging"], "", 10))
            .await
            .unwrap();
        assert_eq!(out.trim_end(), "staging");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_program_reports_nonzero_exit() {
        let err = run_program(&job(&["/bin/false"], "", 10))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("exit"), "got: {err:#}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_program_times_out_slow_child() {
        let err = run_program(&job(&["/bin/sleep", "10"], "", 1))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "got: {err:#}");
    }

    #[test]
    fn clip_output_trims_and_caps() {
        assert_eq!(clip_output("done\n"), "done");
        let big = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let clipped = clip_output(&big);
        assert!(clipped.ends_with("…[truncated]"));
        assert!(clipped.len() < big.len());
    }
}
