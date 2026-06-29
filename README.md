# Mailsweep

A fast terminal app for **cleaning out an overgrown mailbox**. It groups your
mail into a domain → sender → message tree, surfaces unsubscribe links and big
attachments, and lets you bulk-trash, spam, mark-read, unsubscribe, and archive —
across thousands of messages at a time.

Runs entirely on your machine against your **own** account credentials. No
servers, no telemetry.

> ⚠️ **Alpha — use at your own risk.** Mailsweep modifies real email. It has had
> limited testing against live accounts (Gmail more than Outlook), so treat it as
> experimental. Trash/spam are reversible and there's an undo (`z`) and a
> confirmation gate for large batches — but review what an action will do before
> confirming. See [SECURITY.md](SECURITY.md).

## Features

- **Domain → sender → message tree** with live counts and sizes.
- **Views**: All · Subscriptions (senders with an unsubscribe header) ·
  Attachments. Sort by message count, total size, or recency.
- **Bulk actions** on a marked set or the selected node: trash, spam, mark read,
  unsubscribe, **unsubscribe + delete**, archive, **archive + delete**.
- **Fuzzy search** (`/`) over the loaded list, and **server-side scan scope**
  (`f`) — `older_than:1y`, `larger:5M`, `is:unread`, `category:promotions`, …
- **Archive** selected messages to a zip of `.eml` files + extracted attachments
  + a `manifest.json`.
- **Message viewer** (`Enter`) — read the body without leaving the terminal.
- **Undo** (`z`), a **confirmation gate** for big deletes, and a single-instance
  lock so two copies don't fight over the cache.
- **Multiple accounts**, **incremental sync**, on-disk caching, vim-style keys.

## Provider support

| Provider | Status |
| --- | --- |
| **Gmail / Google Workspace** | Primary target; the most-exercised path |
| **Outlook / Hotmail** (consumer, via Microsoft Graph) | Implemented but **experimental / untested** |
| **Generic IMAP** (Yahoo, iCloud, Fastmail, …) | Implemented but **experimental / untested** — no live account to verify against |

## Install

Build from source (needs a recent stable Rust toolchain):

```sh
git clone <repo-url> mailsweep && cd mailsweep
cargo run -p mailsweep-tui
```

Linux, macOS, and Windows are all supported — the TUI runs in any modern
terminal (Windows Terminal or a recent PowerShell on Windows; Terminal.app or
iTerm2 on macOS). Building needs a C toolchain and a system TLS library for the
bundled SQLite and the IMAP/`native-tls` dependency (on Windows the MSVC
toolchain covers this; on macOS the Xcode command-line tools do). So far it has
only been exercised on Linux.

## Setup

Mailsweep uses **your own** OAuth client (Gmail's modify scope is "restricted," so
a shared public client would need Google's paid verification). Creating one is a
free, one-time setup:

- **Gmail** → [`docs/gmail-setup.md`](docs/gmail-setup.md)
- **Outlook** → [`docs/outlook-setup.md`](docs/outlook-setup.md)

Then it's all in-app: launch, focus the **Config** panel (`2`), and pick
**Set Gmail/Outlook credential** (paste the value/JSON or a path), then
**+ Add Gmail/Outlook account** to sign in (browser for Gmail, device code for
Outlook). Switch between accounts in the **Accounts** panel (`1`).

**Generic IMAP** needs no pre-configured credential: pick **+ Add IMAP account**
in the Config panel and fill in host, port, username, and password (use an
app-specific password where the provider requires one). The connection is always
encrypted: port `993` (the default) uses implicit TLS, port `143` uses STARTTLS.
Mailsweep verifies the login before saving. IMAP is **experimental** — restore
isn't available and attachment listing is skipped.

## Keys

| Key | Action |
| --- | --- |
| `1` `2` `3` `4` | Focus Accounts / Config / Domains / Details |
| `Tab` / `Shift-Tab` | Switch view (All / Subscriptions / Attachments) |
| `o` | Cycle sort (Messages / Size / Recent) |
| `/` | Fuzzy-search the loaded list |
| `f` | Server-side scan scope / query (`Tab` for examples) |
| `j` `k` / arrows | Move (or scroll the focused panel) |
| `h` `l` / arrows | Collapse / expand the tree |
| `gg` / `G` | Jump to top / bottom |
| `Space` / `c` | Mark / unmark · clear all marks |
| `Enter` | Open the selected message |
| `a` / `A` | Archive · archive **and** delete |
| `d` / `s` / `r` | Trash · spam · mark read |
| `u` / `U` | Unsubscribe · unsubscribe **and** delete |
| `z` | Undo the last delete (restore to inbox) |
| `?` | Full key help · `q` quit |

Actions apply to the marked set (`●`/`◐`) if any, otherwise the selected
domain/sender/message. Destructive actions over 100 messages ask `y`/`n`.

## How it works

- **Background sync** — the inbox is fetched on a background task; the tree fills
  in live with a progress bar. The scan covers the whole inbox by default; set
  `MAILSWEEP_SCAN_LIMIT=N` to cap it.
- **Incremental** — after a full sync, a `historyId`/delta checkpoint is stored
  and reruns fetch only what changed (no full re-listing). Falls back to a full
  sync if the checkpoint expires.
- **Caching** — message metadata and attachment details are cached in SQLite per
  account, so reruns are fast and quota-cheap. Gmail metadata is fetched via the
  `multipart/mixed` batch endpoint, paced under the per-user quota.

## Data & privacy

Config (your OAuth client credentials) lives in `~/.config/mailsweep/`; per-account
tokens, caches, and archives in `~/.local/share/mailsweep/`. The token files grant
mailbox access — guard them. Full details, scopes, and how to revoke access:
[SECURITY.md](SECURITY.md).

## Layout

```
crates/
  core/   library: providers (Gmail/Outlook), sync, cache, archive, unsubscribe
  tui/    ratatui terminal frontend  (binary: mailsweep)
```

## Contributing

Adding a mail provider is the most self-contained way to help: implement the
`MailProvider` trait in `crates/core/src/provider.rs` (see `gmail/` and
`outlook.rs` for examples) and wire it into `accounts.rs`. Before sending a
change, please run:

```sh
cargo fmt
cargo clippy --workspace
cargo test --workspace
```

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
