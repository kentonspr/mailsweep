# Mailsweep

A local tool for cleaning out an email account. Groups inbox messages by sender
domain and lets you bulk-trash, mark-spam, or unsubscribe. Gmail today; the
`MailProvider` trait leaves room for Outlook (Microsoft Graph) and IMAP
providers (Yahoo, etc.) later.

Each user runs it against their own account, so it stays within Gmail's OAuth
"testing" mode — no app verification or security assessment required.

## Layout

```
crates/
  core/   shared library: OAuth, Gmail client, grouping, unsubscribe
  tui/    ratatui terminal frontend  (binary: mailsweep)
  gui/    iced desktop frontend       (binary: mailsweep-gui)
```

## One-time Google setup

1. In the [Google Cloud Console](https://console.cloud.google.com/), create a
   project and enable the **Gmail API**.
2. Configure the OAuth consent screen as **External**, in **Testing** mode, and
   add your Google account under **Test users**.
3. Create an **OAuth client ID** of type **Desktop app**. Download the JSON.
4. Save it where the app expects it, or point at it via env var:

   ```sh
   # Default location (Linux): ~/.config/mailsweep/client_secret.json
   export MAILSWEEP_CLIENT_SECRET=/path/to/client_secret.json
   ```

The requested scope is `gmail.modify` — it can read, trash, and relabel mail,
but **cannot permanently delete** (deletions go to Trash and are reversible).

## Run

```sh
# Terminal UI
cargo run -p mailsweep-tui

# Desktop GUI
cargo run -p mailsweep-gui
```

First launch opens a browser for consent; the token is cached to
`~/.config/mailsweep/token_cache.json` for subsequent runs.

### Keys (TUI)

- `1`/`2`/`3` — focus the Accounts / Domains / Details panel
- `Tab` / `Shift-Tab` — switch domain view (All / Subscriptions / Attachments)
- `j`/`k` (or `↑`/`↓`) — move within the focused panel (or scroll Details)
- `h`/`l` (or `←`/`→`) — collapse / expand the tree (domain → sender → message)
- `Enter` — load the selected message's attachment list into Details
- `a` — archive attachments of the selected domain / sender / message
- `d` trash · `s` mark spam · `u` unsubscribe — acts on the selected node
- `q` — quit

The inbox syncs in the background: the domain → sender → message tree fills in as
messages arrive, with a progress bar until the scan completes.

### Attachment archives

Pressing `a` downloads the attachments under the current selection and writes a
zip to `~/.config/mailsweep/archives/<account>-<timestamp>.zip`, organized as
`<domain>/<sender>/<message-id>__<filename>`, alongside a `manifest.json`
describing every archived message and attachment.

## Performance

- **Metadata cache** — fetched headers are cached in SQLite at
  `~/.config/mailsweep/metadata.sqlite3` (`core/src/cache.rs`). Rescans only
  fetch IDs not already cached; trashing/spamming evicts the affected rows.
- **Batch fetching** — `fetch_metadata` uses Gmail's `multipart/mixed` batch
  endpoint, bundling up to 100 `messages.get` calls per HTTP request, with
  several batches in flight at once (`core/src/gmail/client.rs`).

## Notes / next steps

- The initial scan is still capped at `SCAN_LIMIT` (1000) messages. To clean a
  large mailbox end-to-end, raise the cap (the cache makes repeated scans cheap)
  and consider incremental/paged loading so the UI is usable before the full
  scan completes.
- Multi-account and multi-provider support (Outlook via Microsoft Graph, Yahoo
  via IMAP) hang off the `MailProvider` trait in `core/src/provider.rs`.
