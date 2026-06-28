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

## Multiple accounts

Each account gets its own token cache, metadata cache, and `historyId`, all
under `~/.config/mailsweep/accounts/<email>/`, and all share the one OAuth client
you set up below. Focus the Accounts panel (`1`), move with `j`/`k`, and press
`Enter` on `[+ Add account]` to authorize another Gmail account in your browser;
`Enter` on an account switches to it. An existing single-account setup is
migrated automatically on first launch.

Note: while your OAuth client is in "testing" mode, every Gmail account you add
must be listed as a **test user** in your Cloud project (see below).

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

- `1`/`2`/`3` — focus the Accounts / Domains / Details panel; in the Accounts
  panel, `j`/`k` move and `Enter` switches account (or `[+ Add account]`)
- `Tab` / `Shift-Tab` — switch domain view (All / Subscriptions / Attachments)
- `o` — cycle sort (Messages / Size / Recent), applied to the current view
- `j`/`k` (or `↑`/`↓`) — move within the focused panel (or scroll Details)
- `h`/`l` (or `←`/`→`) — collapse / expand the tree (domain → sender → message)
- `Space` — mark/unmark the selected node; `c` — clear all marks
- `Enter` — load the selected message's attachment list into Details
- `a` — archive attachments of the marked set (or the selected node)
- `A` — archive **and** trash those messages
- `d` trash · `s` mark spam · `u` unsubscribe — acts on the marked set, or the selected node
- `q` — quit

Views: **All** (everything), **Subscriptions** (senders with an unsubscribe
header), **Attachments** (`has:attachment`). Sort each by message count, total
size, or recency with `o`; under Size sort, aggregate sizes show per
domain/sender. Marks (`●` full, `◐` partial) let you batch a trash/spam/archive
across many domains/senders/messages at once.

In the Attachments view, the app fetches each message's **actual** attachment
sizes/filenames in the background after the sync, so sizes fill in (and `Enter`
becomes instant) without per-message requests.

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

- The scan covers the **whole inbox** by default. Set `MAILSWEEP_SCAN_LIMIT=N`
  to cap it at N messages (e.g. for a quick look at a huge mailbox).
- After the first full sync, a `historyId` checkpoint is stored in the cache and
  subsequent runs do an **incremental sync** (`users.history.list`) — only the
  adds/removes since last time, rebuilt from the cache, with no full re-listing.
  If the history has expired (Gmail keeps ~1 week) it transparently falls back
  to a full sync.
- Multi-account and multi-provider support (Outlook via Microsoft Graph, Yahoo
  via IMAP) hang off the `MailProvider` trait in `core/src/provider.rs`.
