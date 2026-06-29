# Outlook / Hotmail setup

> **Status: experimental and untested against a live account.** The Outlook
> provider (Microsoft Graph) is implemented but has not yet been verified
> end-to-end. Treat it as alpha and double-check what it does.

Mailsweep signs in to consumer Microsoft accounts (outlook.com, hotmail.com,
live.com) with **your own** Azure app registration, using the OAuth 2.0
**device-code** flow — the app shows a URL and a code, you enter them in any
browser. Microsoft's consumer Graph mail scopes don't require a paid security
assessment, so this is a free, one-time setup.

## 1. Register an app in Azure

1. Go to the [Azure Portal](https://portal.azure.com/) and open
   **Microsoft Entra ID → App registrations → New registration**
   (or visit the [App registrations](https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade)
   page directly).
2. **Name**: anything (e.g. `mailsweep`).
3. **Supported account types**: choose **Personal Microsoft accounts only**
   (this matches the `consumers` tenant Mailsweep uses).
4. Leave **Redirect URI** empty. Click **Register**.
5. Copy the **Application (client) ID** from the overview page — you'll paste
   this into Mailsweep.

## 2. Allow the device-code (public client) flow

1. In your app: **Authentication → Advanced settings**.
2. Set **Allow public client flows** to **Yes**. Save.

## 3. Add the Graph permissions

1. **API permissions → Add a permission → Microsoft Graph → Delegated
   permissions**.
2. Add: **Mail.ReadWrite**, **User.Read**, **offline_access**.
3. (Personal accounts don't need admin consent for these.)

## 4. Give the client ID to Mailsweep

In the app: focus the **Config** panel (`2`) → **Set Outlook credential** →
paste the **Application (client) ID** (or a path to a file containing it).

Or set it manually:

```sh
mkdir -p ~/.config/mailsweep
echo 'YOUR-CLIENT-ID' > ~/.config/mailsweep/ms_client_id
# or:
export MAILSWEEP_MS_CLIENT_ID=YOUR-CLIENT-ID
```

## 5. Add the account

Config panel → **+ Add Outlook account**. Mailsweep prints a microsoft.com URL
and a short code; open the URL, enter the code, and sign in. The account then
appears in the Accounts panel; tokens are cached and refreshed automatically.

## Notes & limitations

- **Query syntax differs from Gmail.** The scan-scope query (`f`) uses Microsoft
  Graph `$search` (KQL): `from:amazon`, `subject:invoice`, `hasAttachments:true`,
  `received<=2023-01-01`. Gmail operators like `older_than:` / `larger:` do not
  apply. Press `Tab` in the scan-scope modal for provider-specific examples.
- **Undo (`z`) is best-effort on Outlook.** Graph changes a message's ID when it
  moves, so restoring a trashed message by its old ID may not always succeed.
- **No per-message size**, so the Size sort/column reflects attachment sizes only.
