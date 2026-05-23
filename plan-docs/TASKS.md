# teams-tui — build schedule

Ordered list of build slices. Each slice is sized for one `/loop` iteration:
a focused goal, a concrete deliverable, and a check that says whether it's
done. Do them in order — later slices assume earlier ones are complete.

## How to drive with /loop

Invoke `/loop` with no interval to let the model self-pace. On each wake-up,
pick the first slice marked `pending`, work it to completion, mark it
`done`, and schedule the next wake-up. If a slice turns out larger than
expected, split it rather than cramming.

Rules for the loop:

- Do **only** the current slice. Do not pull forward work from later slices
  "while you're here".
- Run the verification step and paste the result before marking done.
- If a slice blocks (missing dep, spec ambiguity, failing verification you
  can't resolve in-slice), mark it `blocked` with a one-line reason and
  stop the loop — surface the blocker to the user.
- No speculative features. The spec in `SPEC.md` is the contract; if it's
  not there, it's not in scope.

## Status legend

`pending` — not started. `in-progress` — started, not finished.
`done` — finished and verified. `blocked` — needs user input.

---

## Slices

### 1. Scaffold the project — `done`

**Goal:** empty, compilable Rust project with all dependencies declared and
the module files stubbed out.

**Do:**
- `cargo init --name teams-tui` at repo root.
- Populate `Cargo.toml` with the stack from SPEC.md §Stack: `tokio`,
  `reqwest` (features: `rustls-tls`, `json`; `default-features = false`),
  `rustyline`, `notify-rust`, `serde`, `serde_json`, `toml`, `scraper`,
  `chrono`, `directories`, `anyhow`. Pin to current stable releases.
- Create empty module files under `src/` per ARCHITECTURE.md §Suggested
  module layout: `config.rs`, `auth.rs`, `graph.rs`, `poll.rs`,
  `render.rs`, `stream.rs`, `tags.rs`, `notify.rs`, `cursors.rs`. Each
  with a one-line `//!` module doc and `mod` declarations wired into
  `main.rs`.
- `main.rs` should have a `#[tokio::main] async fn main()` that prints
  nothing and exits 0.

**Verify:** `cargo build` succeeds with zero warnings. `cargo run` exits 0.

**Depends on:** nothing.

---

### 2. Config loading — `done`

**Goal:** `config.rs` parses the TOML shown in SPEC.md and returns a typed
`Config` struct, with `~` expanded in paths.

**Do:**
- Define `Config`, `AuthConfig`, `FollowConfig`, `ChannelFollow`,
  `NotificationsConfig` structs with `serde::Deserialize`.
- `Config::load(path)` reads the file, parses TOML, expands `~` on any
  path-shaped fields (notifications.status_file for now; token and cursor
  paths are fixed so can stay hardcoded in their respective modules).
- On missing or malformed config, return an error with a clear message
  pointing at the file path.

**Verify:** unit tests covering: valid config parses; missing file gives
expected error; malformed TOML gives expected error; `~/foo` expands to
`$HOME/foo`. `cargo test config` passes.

**Depends on:** 1.

---

### 3. Cursor persistence — `done`

**Goal:** `cursors.rs` loads and saves a `HashMap<ConversationKey, String>`
(the delta tokens) to `~/.cache/teams-tui/cursors.json`.

**Do:**
- Define `ConversationKey` as an enum: `Chat(String)` or
  `Channel { team_id: String, channel_id: String }`. Derive
  `Serialize`/`Deserialize` with a tagged representation.
- `Cursors::load()` — read file if present, else empty map. Missing file
  is not an error.
- `Cursors::save(&self)` — atomic write (tmp + rename), create parent dir
  if needed.

**Verify:** unit test round-trips a map with one chat entry and one
channel entry through save/load and asserts equality.

**Depends on:** 1.

---

### 4. Tag cycling — `done`

**Goal:** `tags.rs` provides the global `[a]`–`[z]` cycling tag assignment
and lookup.

**Do:**
- `TagTable` with `[Option<MessageRef>; 26]` and a counter.
- `MessageRef` is a small struct: `conversation_key`, `message_id`,
  `plaintext_body` (enough to reconstruct a reply target).
- `assign(msg_ref) -> char` — increments counter, writes to slot, returns
  the letter.
- `lookup(letter) -> Option<&MessageRef>` — reads slot.
- `lookup_latest() -> Option<&MessageRef>` — the most recently assigned.
- Parse helper: `parse_reply(input) -> (Option<char>, &str)` implementing
  the rule from SPEC.md §Reply tags (single ASCII letter + space = tag;
  otherwise reply-to-latest).

**Verify:** unit tests for: 27 assignments wrap correctly; parse_reply on
`"a hi"`, `"hi"`, `"alice is here"`, `""`, `"A hi"` (uppercase — spec
says ASCII letter, but it specifies `[a]`–`[z]` so treat uppercase as
body text).

**Depends on:** 1.

---

### 5. HTML → plain text rendering — `done`

**Goal:** `render.rs` has a `html_to_text(html, self_id) -> (String, bool)`
function returning the rendered plaintext and whether self was mentioned.

**Do:**
- Use `scraper` to walk the DOM.
- Handle the tag table from ARCHITECTURE.md §3: `<at>`, `<emoji>`, `<a>`,
  `<img>`, `<blockquote>`, `<pre><code>`, `<br>`, `<p>`, everything-else.
- `<at id="N">` sets the mention flag when `N == self_id`.

**Verify:** unit tests covering each tag type plus a nested realistic
sample (blockquote + reply text + mention + link).

**Depends on:** 1.

---

### 6. Hard-wrap and message formatting — `done`

**Goal:** `render.rs` gains `format_message(msg, tag, width) -> String`
producing the final multi-line display form from SPEC.md §Display.

**Do:**
- `hard_wrap(text, first_indent, continuation_indent, width) -> String`.
- Prefix construction for the four conversation types (channel/DM/topic
  group/untitled group).
- Final line format: `HH:MM prefix:sender: [tag] content`, with
  continuation lines aligned past the prefix.
- Re-query terminal width on each call (use `rustyline`'s term size or
  a tiny helper; do not cache).

**Verify:** unit tests for each prefix variant, a wrap test with a long
line, and the truncation rule for group participants (`first 3 + …`).

**Depends on:** 5.

---

### 7. Auth / device code flow — `done`

**Goal:** `auth.rs` handles device code acquisition, token refresh, and
persistence to `~/.config/teams-tui/token.json` mode 0600.

**Do:**
- `Tokens` struct: access_token, refresh_token, expires_at.
- `device_code_flow(tenant_id, client_id, scopes) -> Tokens` — prints
  URL and code to terminal, polls `/oauth2/v2.0/token` with
  `grant_type=device_code` until it resolves.
- `refresh(tenant_id, client_id, refresh_token) -> Tokens`.
- `load() / save()` — JSON, 0600 on save.
- `ensure_fresh(&mut self, ...)` — refreshes if <5 min from expiry.

**Verify:** `cargo build` clean; a manual smoke test is deferred to slice
13. For now, a unit test that `save()` sets mode 0600 on the file.

**Depends on:** 2.

---

### 8. Graph HTTP client — `done`

**Goal:** `graph.rs` has typed async wrappers for every Graph endpoint the
app needs.

**Do:**
- `GraphClient` wrapping a `reqwest::Client` and a token provider.
- Methods (name them what fits, but cover these calls):
  - `me()` → user id and display name
  - `joined_teams()` → list teams
  - `team_channels(team_id)` → list channels
  - `list_chats()` → `/me/chats` with `topic`, `chatType`, `members`
  - `chat_messages_delta(chat_id, cursor?)` → messages + nextLink/deltaLink
  - `channel_messages_delta(team_id, channel_id, cursor?)` → same shape
  - `send_chat_message(chat_id, text)` → post plaintext as
    `{"body":{"contentType":"text","content":text}}`
  - `send_channel_message(team_id, channel_id, text)` → same
- On 401, call into `auth` to refresh and retry once.
- On 429, honour `Retry-After` and retry.
- On 410 (cursor expired), bubble up a typed error the caller can catch.
- Define the response types as small `serde` structs — only the fields
  actually used. Do not mirror the full Graph schema.

**Verify:** unit tests using `wiremock` for: happy-path delta response;
401 triggers refresh; 429 sleeps and retries; 410 returns typed error.

**Depends on:** 7.

---

### 9. Stream module — `done`

**Goal:** `stream.rs` owns the output stream: accepts incoming
(new/edit/delete) events, applies local-echo dedupe, assigns tags, prints
via `ExternalPrinter`, maintains the seen-id sets for edit detection.

**Do:**
- `Stream` owns an `ExternalPrinter` (clonable), a `TagTable`, a
  per-conversation bounded seen-id map (last 500 ids each), a dedupe ring
  (32 entries, 30s TTL).
- `send_local_echo(conversation, text)` — prints `> prefix:you: text`,
  records `(conv_id, hash(text), now)` in the ring.
- `on_incoming(msg)` — applies the classification (new/edit/delete from
  ARCHITECTURE.md §1), dedupes own sends via ring, renders, wraps,
  prints, updates tag table, returns a `Notification` enum describing
  what desktop notification (if any) the caller should fire.

**Verify:** unit tests for dedupe (own message within window suppressed;
own message on other device not suppressed because no ring entry);
new→edit→delete sequence produces the right three output lines.

**Depends on:** 4, 6.

---

### 10. Polling loops — `done`

**Goal:** `poll.rs` runs the two concurrent loops: 5s message delta poll
and 30s chat-discovery poll.

**Do:**
- `run_message_poll(graph, cursors, stream, follow_state, shutdown)` —
  every 5s iterate followed conversations, call delta, follow `nextLink`
  in-cycle, feed messages to `stream`, persist cursors on success. On
  410, reinit that conversation's cursor and discard bootstrap.
- `run_discovery_poll(graph, follow_state, shutdown)` — every 30s,
  `/me/chats`, diff, add new chats.
- Backoff on transient network errors per SPEC.md §Polling (5→10→30→60s).
- Both loops honour a shutdown flag and exit cleanly.

**Verify:** unit test with a wiremock server that exposes a delta
endpoint; drive one tick and assert the stream received the expected
messages and the cursor was saved.

**Depends on:** 3, 8, 9.

---

### 11. Notifications and status file — `done`

**Goal:** `notify.rs` fires desktop notifications and writes the status
counter file atomically.

**Do:**
- `Notifier` with `notify(prefix, sender, body_preview)` using
  `notify-rust` at normal urgency; body = `prefix:sender: first 100 chars`.
- `StatusFile::update(unread, mentions)` — atomic write to
  `~/.cache/teams-tui/status` as `unread=N mentions=M\n`.
- Counters live here; reset on request from the prompt task.

**Verify:** unit test for atomic-write (write, rename visible, no
partial file); unit test for format.

**Depends on:** 2.

---

### 12. Wire it all up in `main.rs` — `done`

**Goal:** startup sequence, prompt loop, command dispatch, SIGINT handler.

**Do:**
- Startup per SPEC.md §Startup: load config, load/acquire tokens,
  resolve channel names → ids, load cursors, print
  `teams-tui ready — following N chats and M channels`.
- Spawn the two poll tasks with a shared `ExternalPrinter`, `FollowState`,
  `Stream`.
- Prompt loop on `rustyline::Editor`. On each completed line: parse as
  `/quit`, `/reauth`, or reply. Resolve reply tag via `TagTable`. Call
  `stream.send_local_echo(...)` then `graph.send_*(...)`. Any error
  prints a short red line above the prompt; does not crash.
- Counter reset: call into `Notifier::reset_counters()` once per return
  from prompt (per ARCHITECTURE.md §9's pragmatic approach).
- `tokio::signal::ctrl_c` handler sets the shutdown flag; main loop
  drops the editor, persists cursors, exits 0.

**Verify:** `cargo build --release` clean. `cargo clippy` clean on
default lints.

**Depends on:** 10, 11.

---

### 13. Manual smoke test against real Graph — `blocked`

**Blocker:** requires user-side Azure AD app registration per `AZURE-SETUP.md`,
a `~/.config/teams-tui/config.toml` populated with the resulting tenant_id /
client_id / channel list, and hands-on interaction at the terminal (device
code flow, sending and editing messages, triggering @mentions). The loop
cannot drive this; the user must run it.

**Goal:** confirm the whole contraption talks to real Teams.

**Do:**
- Assumes the user has completed `AZURE-SETUP.md` and created
  `~/.config/teams-tui/config.toml`. If not, stop and tell them.
- Run the binary. Walk through device code flow. Verify ready line.
- Have a colleague (or self from phone) send a message in a followed
  channel/chat. Verify it appears with correct prefix, tag, and
  formatting. Reply with `a test reply`. Verify local echo, no
  duplicate, and the reply arrives in Teams.
- Trigger a @mention in a followed channel — verify desktop
  notification.
- Trigger an edit and a delete from the other side — verify the two
  append lines appear.
- Cat `~/.cache/teams-tui/status` — verify `unread=...` format.
- `/quit` — verify clean exit and cursors file on disk.

**Verify:** each bullet above confirmed. Any issue found is a new slice
(14, 15, ...) appended to this file; this slice stays `done` once the
happy path works end-to-end.

**Depends on:** 12, plus user-side Azure setup from `AZURE-SETUP.md`.
