# teams-tui

A minimal terminal Microsoft Teams client. Single-user, single-tenant, read-mostly. It streams DMs and a few configured channels into a scrolling pane, lets you reply with one-letter tags, and stays out of your way the rest of the time. It is **not** a replacement for the real Teams client — for anything non-trivial, switch to that.

## Features

- Streams 1:1 chats, group chats, and configured team channels into a single scrolling pane.
- Reply to any message by typing `<letter> <text>` against its `[a]`–`[z]` tag, or bare `<text>` to reply to the most recent.
- `/chats` lists followed chats with `A`–`Z` tags; `/A hello team` cold-starts a conversation when no message is yet on screen to reply against.
- Desktop notifications via `notify-rust` on @mentions and 1:1 DMs.
- Status counter file at `~/.cache/teams-tui/status` for i3blocks / polybar integration.
- Device-code auth flow (no client secret), refresh token persisted at `~/.config/teams-tui/token.json` mode `0600`.

## Setup

### 1. Register an Azure AD app

See [plan-docs/AZURE-SETUP.md](plan-docs/AZURE-SETUP.md). Five-minute job if you're tenant admin. You'll come away with a `tenant_id` and `client_id`.

### 2. Write the config

At `~/.config/teams-tui/config.toml`:

```toml
[auth]
tenant_id = "your-tenant-guid"
client_id = "your-client-guid"

[notifications]
mention_desktop = true
status_file = "~/.cache/teams-tui/status"

# Optional: follow specific team channels in addition to all your chats.
# [[follow.channels]]
# team = "Engineering"
# channel = "general"

# Optional: exec bots — when an incoming message matches `trigger`, run a
# program and post its stdout back into that conversation (as you).
# [[bot]]
# kind = "exec"
# trigger = "^!deploy (\\w+)"      # regex; capture groups become extra argv
# command = ["/usr/local/bin/deploy"]
# timeout_secs = 30                # kill the program after this long (default 10)
# cooldown_secs = 5                # min seconds between firings per conversation
# only = { channel = "ops" }       # optional: restrict to one channel
```

### Exec bots

Each `[[bot]]` watches every message you'd see in the stream. On a regex match,
`command` runs with the regex capture groups appended as arguments and the whole
message piped to **stdin**; its stdout is posted back into the same conversation.

The message is passed as *data* — the command is an argv list, never run through
a shell — so a crafted message can't inject a command. Output is capped at 8 KiB.

**You can trigger your own bots from another Teams client.** Send `!bot fortune`
from your phone or the desktop app and teams-tui will see it as a new message and
fire the bot. A bot's *reply*, by contrast, is echoed locally, so when it loops
back through the poll it's recognised as your own and dropped — that's what stops
a program that echoes its trigger from looping. (For the same reason, typing the
trigger directly in the teams-tui prompt won't fire it — send it from another
client.) A per-conversation `cooldown_secs` bounds the rate if you want a backstop.

Because bots post **as you**, treat the programs you wire up as things you're
happy to run against attacker-controlled input from anyone who can message you.

### 3. Install

```bash
cargo install --path .
```

Drops the binary at `~/.cargo/bin/teams-tui`.

### 4. Run

```bash
teams-tui
```

First launch prints a device code; open the URL it shows and authenticate. Subsequent launches use the stored refresh token.

## Keys & commands

| Input | Effect |
|---|---|
| `a hi there` | Reply to message tagged `a` with body "hi there" |
| `hi there` | Reply to the most recent tagged message |
| `/chats` | List followed chats with `A`–`Z` tags |
| `/A hello team` | Send "hello team" to chat tagged `A` |
| `/reauth` | Redo device-code login (use on 401s) |
| `/quit`, `Ctrl-C`, `Ctrl-D` | Shut down |

The leading reply letter is recognised only when followed by a space: `alice is here` replies to tag `a` with body "lice is here" (per the SPEC; the user is expected to know the convention).

## Launching from dmenu

Bare `teams-tui` needs a real TTY. A wrapper that opens it in a terminal:

```bash
#!/usr/bin/env bash
exec alacritty --title teams-tui -e teams-tui "$@"
```

Save as `~/.cargo/bin/teams-tui-term`, `chmod +x`, and invoke that from dmenu.

## Limitations

- **Reply-anchored input.** Every send must point at a known conversation, either by tagged message or by `/chats` + `/<TAG>`.
- **No `/help`, `/ignore`, `/join`, `/watch`.** Configuration changes require editing the TOML and restarting.
- **Max 26 followed chats taggable.** Extras get no `/<TAG>` slot until a tag frees up.

## Design docs

The full design lives in [plan-docs/](plan-docs/):

- `SPEC.md` — observable behaviour, the source of truth
- `ARCHITECTURE.md` — module layout and data flow
- `AZURE-SETUP.md` — app registration walkthrough
- `TASKS.md` — implementation breakdown
- `CONTENTS.md` — index

## Build

```bash
cargo build --release
cargo test --release
```
