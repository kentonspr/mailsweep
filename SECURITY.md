# Security & privacy

Mailsweep reads and modifies your email, so it's worth being clear about what it
touches and where things live.

## No servers, no telemetry

Mailsweep is a local program. It talks **directly** to your mail provider
(Google / Microsoft APIs, or an IMAP server over TLS) from your machine. There
is no Mailsweep backend, no analytics, and no network traffic to anyone other
than your provider.

## Bring-your-own credentials

You register your **own** OAuth client (Google Cloud / Azure) and authorize your
**own** accounts. Mailsweep ships no shared client secret. See
[`docs/gmail-setup.md`](docs/gmail-setup.md) and
[`docs/outlook-setup.md`](docs/outlook-setup.md).

## Scopes & access

Mailsweep **never permanently deletes** mail. Every "delete" moves messages to
Trash (or Junk for spam), which your provider keeps recoverable for a while.
This is intentional and consistent across providers, even where the API or
protocol would allow a hard delete.

- **Gmail:** `https://www.googleapis.com/auth/gmail.modify` — read, label, trash,
  and mark messages. It can't expunge at all; Mailsweep deliberately does not
  request the broader `https://mail.google.com/` scope.
- **Outlook (Microsoft Graph):** `Mail.ReadWrite`, `User.Read`, `offline_access`.
  Mailsweep only ever moves messages to Deleted Items.
- **Generic IMAP:** Mailsweep authenticates with the username/password you
  supply and only moves messages to the Trash folder (it never sets `\Deleted`
  / expunges). Prefer an app-specific password where the provider offers one.

## Where data is stored

- **Config** — `~/.config/mailsweep/`: your OAuth client credentials
  (`client_secret.json`, `ms_client_id`).
- **Data** — `~/.local/share/mailsweep/`:
  - `accounts/<email>/token.json` — OAuth refresh/access tokens, or for IMAP
    accounts the connection settings **including the password in plaintext**.
  - `accounts/<email>/metadata.sqlite3` — a local cache of message headers,
    attachment metadata, and the sync checkpoint.
  - `archives/` — zip archives you create.

These are plain files protected by normal filesystem permissions. **The token
files grant access to your mailbox** — guard them like passwords, and revoke
access at <https://myaccount.google.com/permissions> (Google) or
<https://account.live.com/consent/Manage> (Microsoft) if a machine is lost.
Removing an account's directory and re-adding it issues fresh tokens.

## Destructive actions

Trash/spam are reversible (and Mailsweep has a one-level undo, `z`). Bulk
destructive actions over 100 messages require confirmation. Even so, this is
**alpha software that modifies real mail** — review what an action will do before
confirming.

## Reporting a vulnerability

This is a small hobby project without a formal disclosure process. If you find a
security issue, please open a GitHub issue describing it (omit any secrets), or
contact the maintainer privately if it's sensitive. There are no guarantees of
response time.
