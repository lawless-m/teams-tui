# teams-tui — project documents

A minimal TUI Teams client for passive monitoring and quick replies. Single-user, single-tenant, Debian Linux target, Rust.

## Where to start

1. **SPEC.md** — the full behavioural specification. Read this first. Everything the app should and shouldn't do is here.
2. **ARCHITECTURE.md** — suggested module layout and notes on the non-obvious implementation problems (delta cursors, async prompt, HTML rendering, local echo deduplication, polling loops). Read before writing code.
3. **AZURE-SETUP.md** — steps to register the Azure AD app and grant scopes. The user is tenant admin and will do this themselves before the code is run. The resulting `tenant_id` and `client_id` go into the config file.

## Scope reminder

This is a personal tool for one user on one machine. It is deliberately small. Features not listed in SPEC.md are out of scope — do not add them speculatively. If something in the spec is ambiguous, prefer the simpler interpretation.

## Target environment

- Debian Linux, i3 window manager, i3blocks for status
- Rust stable, no nightly features
- Terminal is a standard xterm-compatible; no kitty/sixel image support needed
- User has 45 years of programming experience but does not write Rust, so code should be idiomatic enough to read rather than clever
