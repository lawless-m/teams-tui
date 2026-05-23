# teams-tui — specification

A minimal terminal-based Microsoft Teams client for passive monitoring and quick replies. Single-user, single-tenant, read-mostly.

## Purpose

The user is a Teams admin who uses Teams at work and currently keeps the full Teams desktop client open solely to notice when someone asks a question in a handful of channels or DMs. This tool replaces that need with a small terminal window streaming those messages and allowing short replies. For any non-trivial interaction the user switches to the real Teams client (available via Remmina). This tool is explicitly not a replacement for Teams.

## Behaviour

### Display

- Single scrolling pane of messages. Prompt at the bottom via `rustyline` with `ExternalPrinter` so incoming messages printed asynchronously do not stomp the input line.
- Message line format:

  ```
  HH:MM prefix:sender: [tag] content
  ```

- Hard-wrap long lines, with continuation lines indented past the prefix so the message column is visually aligned. Use the current terminal width (re-query on each print; do not cache).
- Prefixes:
  - `#channel-name` for team channels (drop the team name; if two followed channels share a name, disambiguate as `team/channel`)
  - `DM:sender` for 1:1 chats
  - `#topic:sender` for group chats with a topic set
  - `group[Alice,Bob,Carol]:sender` for group chats without a topic; truncate the participant list to first 3 names followed by `…` if longer
- Timestamps in local time, 24-hour, `HH:MM`. No date — if the user cares about what day something happened they will look in Teams.

### Reply tags

- Every incoming message receives the next tag in a cycling alphabet `[a]`–`[z]`, wrapping back to `[a]` after `[z]`. Own messages (local echo) do not consume tags.
- To reply: type `<letter> <text>` and press enter.
- To reply to the most recently tagged message: type `<text>` with no leading letter and press enter.
- The leading letter is identified as a reply-tag only when it is a single ASCII letter followed by a space. `hi there` replies to most-recent; `a hi there` replies to tag `a`; `alice is here` also replies to tag `a` (trailing `lice is here` is the message body — this is acceptable; the user knows the convention).
- If a letter refers to a tag that has been overwritten by wrap-around, use the most recent message that currently holds that letter.

### Sending

- Local echo on send: print `> prefix:you: text` immediately so the user sees their own message in the stream without waiting for the next poll cycle.
- The same message will come back via delta poll within 5 seconds. Deduplicate: suppress any polled message whose `from.user.id` matches the authenticated user's id AND whose content matches a locally-echoed message sent in the last 30 seconds. Keep a small ring buffer of recent local-echo hashes for this purpose.

### Edits and deletions

- Edit of a previously-seen message: append a new line `[edit from sender in prefix]: new text` at the current position in the stream. Do not rewrite history.
- Deletion: append `[sender deleted a message in prefix]`.
- Edits/deletions of the user's own messages are suppressed (no notification to self).

### Filtering

- System messages silently dropped. This includes: user added to chat, user removed, chat renamed, meeting started/ended, call started/ended, and any message with `messageType` other than `message`.
- Adaptive Cards: render as `[card: title]` where title is extracted from the card JSON if present, else just `[card]`.
- Attachments: render as `[attachment: filename.ext]` for each attachment. No download, no open. If the user needs the file they open real Teams.
- Inline images: render as `[image]`. Do not attempt to display.
- Hyperlinks in message content: render as the visible text followed by ` (<url>)`. Terminal hyperlink escapes (OSC 8) may be used as a nice-to-have but are not required.
- Mentions in received messages: render `<at>Alice</at>` as `@Alice`. Mentions of the authenticated user trigger the mention notification path (see Notifications).
- Quoted replies / reply-to-another-message: render the quoted portion on its own indented line prefixed with `> ` before the reply content.

### Follow model

- All 1:1 chats auto-followed.
- All group chats auto-followed. A background task re-lists `/me/chats` every 30 seconds to detect newly-created group chats; any new chat ID starts being polled on the next message-poll cycle.
- Team channels: fixed list in config file. No auto-follow of all channels in all teams — that would be a firehose.
- No ignore list in v1. If it becomes noisy, revisit.

### Polling

- 5-second interval for delta queries on all known chats and channels.
- 30-second interval for re-listing `/me/chats` to detect new group chats.
- Graph delta cursors persisted to `~/.cache/teams-tui/cursors.json` after each successful poll so restarts continue where the previous session stopped.
- On cursor expiry (HTTP 410 Gone or similar): silently reinitialise that cursor from "now" (i.e., start a fresh delta query and discard the bootstrap page without printing it). Accept that messages during the gap are lost; the user can see them in real Teams if needed.
- On transient network errors: log to stderr, back off (5s, 10s, 30s, cap at 60s), resume. Do not crash. Do not spam the user's message pane with error chatter.

### Notifications

- Desktop notifications via `notify-rust` when:
  - The authenticated user is @mentioned in any watched conversation, OR
  - A 1:1 DM arrives (these are implicitly directed at the user).
  - Never for own messages, edits, deletions, system messages, or non-mention channel chatter.
- Notification body: `prefix:sender: first 100 chars of message`. Normal urgency.
- Status counter file at `~/.cache/teams-tui/status`, rewritten atomically (write to `.status.tmp`, rename) on every state change. Format is a single line:

  ```
  unread=N mentions=M
  ```

  where `N` counts all new messages since last counter reset and `M` counts @mentions / DMs since last reset.
- Counter resets to `unread=0 mentions=0` on any keypress in the TUI (including arrow keys, not only enter — user glancing at the terminal and pressing anything should count as acknowledgement).
- An i3blocks script will read this file every few seconds; the TUI does not interact with i3blocks directly.

### Commands

Typed at the prompt, with a leading `/`:

- `/quit` — clean shutdown; persist cursors, exit 0.
- `/reauth` — force re-run of device code flow. Used when the stored token is rejected (Graph returns 401 and refresh fails).

No other commands in v1. No `/ignore`, no `/join`, no `/help`, no `/watch`. If the user needs more control they edit the config file and restart.

### Config

TOML at `~/.config/teams-tui/config.toml`. Created by the user manually before first run; the app does not generate it. Example:

```toml
[auth]
tenant_id = "00000000-0000-0000-0000-000000000000"
client_id = "00000000-0000-0000-0000-000000000000"

[[follow.channels]]
team = "IT Department"
channel = "IT-Support"

[[follow.channels]]
team = "Engineering"
channel = "general"

[notifications]
mention_desktop = true
status_file = "~/.cache/teams-tui/status"
```

Paths containing `~` are expanded at load time.

### Authentication

- OAuth2 device code flow on first run. The device code URL and user code are printed to the terminal; the user opens the URL in a browser, enters the code, consents.
- Resulting refresh token and access token stored in plain JSON at `~/.config/teams-tui/token.json`, mode `0600`. The user explicitly prefers this over keyring.
- Access token refreshed automatically when <5 minutes from expiry or on any 401.
- Scopes requested:
  - `User.Read`
  - `Chat.ReadWrite`
  - `ChannelMessage.Send`
  - `ChannelMessage.Read.All`
  - `Team.ReadBasic.All`
  - `Channel.ReadBasic.All`
  - `offline_access`

### Startup

- Load config. If missing or malformed, print a clear error pointing at the file path and exit non-zero.
- Load token. If missing, run device code flow. If present but refresh fails, run device code flow.
- Resolve channel names in config to `team_id`/`channel_id` via Graph. Cache the resolution in memory only — re-resolve on each launch.
- Load cursors from cache file if present. For any followed conversation without a cursor, initialise a fresh delta query and discard the bootstrap page (no backfill).
- Print a single line: `teams-tui ready — following N chats and M channels` then show the prompt.
- Begin polling.

### Shutdown

- On `/quit` or SIGINT/SIGTERM: persist cursors, close rustyline cleanly, exit 0.
- Do not attempt to "flush" any pending sends — if the user has typed a reply and not pressed enter, it is discarded without warning. This is a personal tool; they'll remember.

## Stack

- Rust, stable toolchain.
- `tokio` async runtime.
- `reqwest` with `rustls-tls` for Graph HTTP (no OpenSSL dependency).
- `rustyline` with `ExternalPrinter` for the prompt and async output.
- `notify-rust` for desktop notifications.
- `serde`, `serde_json`, `toml` for config and API payloads.
- `scraper` (preferred) or `html5ever` for Teams HTML message content → plain text conversion.
- `chrono` for timestamps.
- `directories` for config/cache path resolution.
- `anyhow` for error handling in application code; `thiserror` only if a library-style error type is needed somewhere (probably not).

No SQLite, no ratatui, no keyring, no async-std, no hyper-direct. Keep the dependency tree small.

## Out of scope for v1

Listed so Claude Code does not accidentally implement them:

- Scrollback beyond native terminal buffer
- Message search
- Presence indicators
- File uploads or downloads
- Reactions, emoji picker
- Threading drill-in for channel replies (channel replies appear inline in the stream, tagged with their channel prefix; the user can decide per-case whether they care about thread context)
- Meeting controls, call controls
- Multiple accounts or tenants
- Per-chat ignore list
- Backfill of history on startup
- Click-to-focus from i3blocks (wrapper script territory, not TUI territory)
- Configuration reload without restart
- Automatic config file generation
- Colour themes or per-user colours (simple ANSI colours for channel vs DM vs mention is fine; no theming)
