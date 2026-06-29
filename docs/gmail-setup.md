# Gmail / Google Workspace setup

Mailsweep talks to your Gmail directly using **your own** OAuth client. Nothing
is sent to any third-party server. You create a free Google Cloud project, enable
the Gmail API, and download a "Desktop app" credential — a one-time setup of a
few minutes.

> Why your own client? Gmail's read/modify scopes are "restricted." Publishing a
> shared client to the public would require Google's annual third-party security
> assessment. With your own client in **testing** mode you authorize only your
> own accounts, no verification needed.

## 1. Create a project

1. Go to the [Google Cloud Console](https://console.cloud.google.com/).
2. Top bar → project dropdown → **New Project**. Name it anything (e.g.
   `mailsweep`) and create it. Make sure it's selected afterward.

## 2. Enable the Gmail API

1. Open **APIs & Services → Library** (or visit
   <https://console.cloud.google.com/apis/library/gmail.googleapis.com>).
2. Search for **Gmail API**, open it, and click **Enable**.

## 3. Configure the OAuth consent screen

1. **APIs & Services → OAuth consent screen**.
2. User type: **External**. Click **Create**.
3. Fill in the required fields (app name, your email for support/developer
   contact). You can leave optional fields blank.
4. **Scopes**: you don't have to add any here — Mailsweep requests
   `https://www.googleapis.com/auth/gmail.modify` at sign-in time. (Adding it
   here is fine too.)
5. **Test users**: click **Add users** and add **every Gmail address you intend
   to use with Mailsweep**. In testing mode only listed accounts can authorize.
6. Save. Leave the app in **Testing** (do not "Publish").

## 4. Create the OAuth client (Desktop app)

1. **APIs & Services → Credentials → Create Credentials → OAuth client ID**.
2. Application type: **Desktop app**. Name it (e.g. `mailsweep-desktop`) and
   create.
3. Click **Download JSON**. This is your `client_secret.json`.

## 5. Give it to Mailsweep

You can do this entirely in the app:

- Launch Mailsweep, focus the **Config** panel (`2`), select **Set Gmail
  credential**, and paste the **contents** of the downloaded JSON, or a **path**
  to the file.

Or set it up manually:

```sh
mkdir -p ~/.config/mailsweep
cp ~/Downloads/client_secret_*.json ~/.config/mailsweep/client_secret.json
# or point at it without copying:
export MAILSWEEP_CLIENT_SECRET=/path/to/client_secret.json
```

## 6. Add the account

In the Config panel choose **+ Add Gmail account**. Your browser opens for
consent. The first time you'll see an **"unverified app"** warning — this is
expected for a testing-mode app you created; choose **Continue**. Grant access,
and the account appears in the Accounts panel.

Tokens are cached under `~/.local/share/mailsweep/accounts/<email>/` and refresh
automatically; you won't need to re-consent.

## Troubleshooting

- **403 `accessNotConfigured` / "Gmail API has not been used…"** — the Gmail API
  isn't enabled for the project your OAuth client belongs to (step 2).
- **"Access blocked: app has not completed verification" / `access_denied`** —
  the Google account isn't in the **Test users** list (step 3.5), or the app was
  published out of testing.
- **Adding a second account fails** — each account must also be a test user.
