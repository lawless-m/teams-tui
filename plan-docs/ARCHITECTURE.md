# teams-tui — architecture notes

This document describes a suggested module layout and flags the non-obvious implementation problems. It is not a rigid specification — if a better structure emerges during implementation, use it. But the problems called out here are real and deserve up-front thought.

## Suggested module layout

```
src/
  main.rs           # Argument parsing, top-level orchestration, signal handling
  config.rs         # TOML loading, ~ expansion, validation
  auth.rs           # Device code flow, token refresh, token persistence
  graph.rs          # Typed wrappers around the Graph endpoints used
  poll.rs           # The two polling loops (messages, chat discovery)
  render.rs         # HTML → plain text, message formatting, hard-wrap
  stream.rs         # The output stream: local echo, dedup ring, ExternalPrinter
  tags.rs           # Reply-tag cycling and lookup
  notify.rs         # Desktop notifications and status file writer
  cursors.rs        # Delta cursor persistence
```

`main.rs` wires these together. Most modules should be independently testable.

## Concurrency model

Three logical tasks run concurrently:

1. **Prompt task** (on the main thread, driven by `rustyline`). Reads user input, dispatches commands and replies. Reply-tag resolution and send-API calls happen here.
2. **Message-poll task** (tokio task). Every 5 seconds, iterates the set of followed chats and channels, calls their delta endpoints, feeds new/edited/deleted messages into the stream module.
3. **Discovery-poll task** (tokio task). Every 30 seconds, calls `/me/chats`, diffs against the known chat set, adds any new chats to the followed set.

Shared state between tasks:

- The followed set (chats + channels + cursors). `Arc<Mutex<FollowState>>` is fine; contention is negligible.
- The tag table (letter → most recent message reference). Owned by the stream module; the prompt task queries it to resolve replies.
- The dedup ring (recent local-echo hashes). Owned by the stream module; written by send path, read by poll path.

`ExternalPrinter` from rustyline is the mechanism by which the poll tasks print above the prompt without disturbing the user's in-flight input. Clone it into each task that needs to emit output.

## The non-obvious problems

### 1. Delta cursors

Graph's delta endpoints are `/chats/{id}/messages/delta` and `/teams/{tid}/channels/{cid}/messages/delta`. First call returns a page of recent messages plus `@odata.nextLink` or `@odata.deltaLink`. Follow `nextLink` until you get a `deltaLink`; that URL (with its embedded `$deltatoken`) is what you call next time to get only changes since.

Gotchas:

- The bootstrap call (no prior token) returns recent history. For a fresh follow we deliberately discard this — the spec says no backfill.
- Cursors expire. Graph returns 410 Gone (sometimes 400 with a specific error code) when that happens. Reinit from scratch, discard bootstrap, continue.
- Cursors are per-conversation. Persist them as a map keyed by `chat_id` or `(team_id, channel_id)`.
- Deltas include edits and deletions. An edit arrives as a message with the same `id` as something previously seen but updated `lastModifiedDateTime` and new `body`. A deletion arrives as a message with `deletedDateTime` set. The stream module needs to distinguish: did we see this id before? If yes and body changed → edit line. If yes and deletedDateTime → deletion line. If no → new message.
- For the "did we see this id before" check: a bounded in-memory set of recent ids per conversation is enough (say, last 500 per conversation). Messages older than that won't get edit-detection, but that's acceptable — edit notifications for week-old messages aren't useful anyway.

### 2. rustyline + async output

`rustyline::ExternalPrinter` is the key. Without it, the poll loop's `println!` will clobber the input line mid-type.

The pattern: create the editor, call `editor.create_external_printer()` to get an `ExternalPrinter`, clone it into poll tasks, have them call `printer.print(line)?` instead of `println!`. Rustyline internally parks the prompt, prints, and restores the prompt with any in-flight input preserved.

One subtlety: `ExternalPrinter` in rustyline is synchronous but designed to be called from other threads. It uses an internal channel and a background thread. You can call it from inside a tokio task via `spawn_blocking` if necessary, but in practice calling it directly from async code works because the send is effectively non-blocking.

### 3. HTML → plain text

Teams messages come as HTML. Examples to handle:

- `<p>Hello <at id="0">Alice</at>, see <a href="https://...">this</a></p>`
- `<div><blockquote>...previous message...</blockquote>Reply text</div>`
- `<emoji alt="smile">` — render as the alt text or a unicode equivalent
- Inline `<img src="...">` with auth-protected src URLs
- Code blocks `<pre><code>...`

Suggested approach with `scraper`: walk the DOM, emit text with a small transformer per tag type. Not a generic HTML-to-text library — just handle the tags Teams actually emits. Expect to iterate on this as weird edge cases turn up in real messages.

Known Teams-specific tags and their handling:

| Tag                     | Handling                                              |
|-------------------------|-------------------------------------------------------|
| `<at id="N">Name</at>`  | Emit `@Name`. If id matches self, set mention flag.   |
| `<emoji alt="...">`     | Emit the alt text as-is.                              |
| `<a href="URL">text</a>`| Emit `text (URL)`.                                    |
| `<img src="...">`       | Emit `[image]`.                                       |
| `<blockquote>...`       | Emit each line prefixed with `> ` and indented.       |
| `<pre><code>...`        | Emit with surrounding blank lines; keep content as-is.|
| `<br>`, `<p>`           | Line breaks.                                          |
| everything else         | Recurse into children, ignore the tag itself.         |

### 4. Local-echo deduplication

When the user sends a reply:

1. Stream module prints `> prefix:you: text` immediately.
2. Stream module records `(conversation_id, text_hash, timestamp)` in a bounded ring buffer.
3. POST to Graph's send endpoint.
4. Within ~5 seconds, the delta poll returns this same message (because Graph echoes the user's own sends through the delta endpoint).
5. Poll code checks: is this message from self? If yes, is `(conversation_id, hash(body_text), ~now)` in the ring? If yes, suppress. If no, print (which means the user sent something from real Teams on another device — legitimately show it).

Use plain text (post-HTML-rendering) for the hash, not the raw HTML Graph returns, because Graph may wrap the plaintext the TUI sent in `<p>...</p>` or similar before echoing it back.

Ring buffer size: 32 entries is plenty. TTL: 30 seconds.

### 5. The two polling loops and rate limits

Graph rate limits for Teams messaging are the tightest constraint. The per-app-per-tenant budget is generous, but per-user and per-conversation throttling exists too. For one user with (say) 20 followed chats and 5 channels:

- Message poll every 5s: 25 delta calls per cycle = 5 calls/second steady state.
- Discovery poll every 30s: 1 call per cycle.

Well within limits. But: if `@odata.nextLink` is returned (a delta returns multiple pages), follow it in-cycle — don't wait for the next 5-second tick for each page. Bursts are fine; sustained rate is the thing that matters.

On HTTP 429 (throttled): read the `Retry-After` header, sleep that long, resume. Do not retry faster than the header says.

### 6. Mention detection

Graph provides a `mentions` array on message objects giving the mentioned user IDs. The cleaner way to detect "was I mentioned" is to check this array against `me.id`, not to parse `<at>` tags out of the HTML. Use it.

For sending mentions: the user will often want to reply without @mentioning anyone, which is the simple case (plain text body). Actually constructing outbound mentions is out of scope for v1 (no `/mention` command). If the user needs to @mention someone, they'll use real Teams.

### 7. Name resolution

The config lists channels by human names (`team = "IT Department"`, `channel = "IT-Support"`). These must be resolved to IDs via:

- `GET /me/joinedTeams` → find team by `displayName`
- `GET /teams/{id}/channels` → find channel by `displayName`

Do this at startup. If a name doesn't resolve, print an error mentioning the offending config entry and exit. Don't silently skip — the user wants to know.

Caching the resolution on disk is tempting but a bad idea: team/channel renames happen, and startup cost is negligible.

### 8. Tag cycling across chats

The tag space is global across all conversations, not per-conversation. So tag `[a]` might be on a message in `#IT-Support` right now, and the next incoming message in a DM will be `[b]`, and so on. This is intentional — the user is reading one stream, not per-conversation streams, so tags being unique across the whole view is what's useful.

Implementation: a single `AtomicUsize` counter, modulo 26, giving `('a' as u8 + n) as char`.

When a tag is re-used (26 messages later), the earlier message loses its tag. Maintain a `[Option<MessageRef>; 26]` array indexed by letter; each new assignment overwrites the previous holder.

### 9. Keypress counter reset

Rustyline doesn't expose "any keypress" as an event directly — it emits completed lines (on enter) and editor events. For the counter-reset behaviour, the pragmatic approach: reset the counter inside the prompt's main loop every time control returns from a user action, which is close enough. If that proves insufficient, use rustyline's `EventHandler` API to hook individual keys.

If going the EventHandler route, be careful not to add latency to typing.

### 10. SIGINT handling

Install a `tokio::signal::ctrl_c` handler that sets a shutdown flag and drops the rustyline editor. The main loop should check the flag between prompts. The poll tasks check the flag between cycles.

Avoid the temptation to make Ctrl-C "cancel current input" — rustyline already does that for the prompt. Top-level Ctrl-C means quit.

## Testing

- Pure functions (HTML rendering, hard-wrap, tag cycling, config parsing) should have unit tests.
- The polling loop and Graph client are harder. A mock HTTP server (wiremock or similar) for the Graph client is worth the setup cost. Don't hit live Graph in tests.
- No need for integration tests of the full TUI; the pieces are small enough that integration can be verified by running it.

## Error handling philosophy

- Network errors in poll loops: log to stderr, back off, retry. Never panic, never surface to the user's message pane.
- Parsing errors on individual messages: log to stderr, skip the message, continue. One malformed message must not stop the stream.
- Fatal errors (config missing, auth failed, filesystem unwritable): print to stderr, exit non-zero. These are launch-time problems the user needs to see and fix.

## Build notes

- Target: `x86_64-unknown-linux-gnu`. No cross-compilation needed.
- `cargo build --release` for actual use; startup latency matters less than memory (though neither will be a problem here).
- Binary should be a single file installable to `~/.local/bin/`.
