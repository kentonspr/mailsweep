//! Per-account storage and the add-account flows (Gmail + Outlook).
//!
//! Each account lives in its own directory under [`config::accounts_dir`]:
//! ```text
//! accounts/<sanitized-email>/email              # the real address
//! accounts/<sanitized-email>/provider           # "gmail" | "outlook"
//! accounts/<sanitized-email>/token.json         # OAuth tokens
//! accounts/<sanitized-email>/metadata.sqlite3   # message cache + sync token
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::auth::GmailAuth;
use crate::config;
use crate::gmail::GmailClient;
use crate::outlook::{MsAuth, OutlookClient};
use crate::provider::MailProvider;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Gmail,
    Outlook,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Gmail => "gmail",
            Provider::Outlook => "outlook",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Gmail => "Gmail",
            Provider::Outlook => "Outlook",
        }
    }

    fn parse(s: &str) -> Provider {
        match s.trim() {
            "outlook" => Provider::Outlook,
            _ => Provider::Gmail,
        }
    }
}

/// A configured account.
#[derive(Clone, Debug)]
pub struct Account {
    pub email: String,
    pub provider: Provider,
}

fn sanitize(email: &str) -> String {
    let cleaned: String = email
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}

pub fn account_dir(email: &str) -> PathBuf {
    config::accounts_dir().join(sanitize(email))
}

pub fn token_path(email: &str) -> PathBuf {
    account_dir(email).join("token.json")
}

pub fn cache_path(email: &str) -> PathBuf {
    account_dir(email).join("metadata.sqlite3")
}

/// List the configured accounts (sorted by email).
pub fn list_accounts() -> Vec<Account> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(config::accounts_dir()) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(email) = std::fs::read_to_string(dir.join("email")) else {
                continue;
            };
            let email = email.trim().to_string();
            if email.is_empty() {
                continue;
            }
            let provider = std::fs::read_to_string(dir.join("provider"))
                .map(|s| Provider::parse(&s))
                .unwrap_or(Provider::Gmail);
            out.push(Account { email, provider });
        }
    }
    out.sort_by(|a, b| a.email.cmp(&b.email));
    out
}

fn persist_account(email: &str, provider: Provider, pending_token: &PathBuf) -> Result<()> {
    let dir = account_dir(email);
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::rename(pending_token, token_path(email));
    std::fs::write(dir.join("email"), email)?;
    std::fs::write(dir.join("provider"), provider.as_str())?;
    Ok(())
}

/// Run the sign-in flow for a new account of `provider`. Blocks on user consent
/// (browser for Gmail, device code for Outlook). Returns the account email.
pub async fn add_account(provider: Provider) -> Result<String> {
    let pending = config::accounts_dir().join(".pending");
    std::fs::create_dir_all(&pending).ok();
    let pending_token = pending.join("token.json");
    let _ = std::fs::remove_file(&pending_token);

    let email = match provider {
        Provider::Gmail => {
            let auth =
                GmailAuth::new(config::secret_path(), pending_token.clone(), config::SCOPES);
            let client = GmailClient::new(Arc::new(auth));
            client.profile().await?.email
        }
        Provider::Outlook => {
            let client_id = config::ms_client_id().context(
                "set MAILSWEEP_MS_CLIENT_ID (or ~/.config/mailsweep/ms_client_id) to your Azure app id",
            )?;
            let auth = MsAuth::new(client_id, pending_token.clone());
            auth.device_login().await?;
            let client = OutlookClient::new(Arc::new(auth));
            client.profile().await?.email
        }
    };

    persist_account(&email, provider, &pending_token)?;
    let _ = std::fs::remove_dir_all(&pending);
    Ok(email)
}

/// If no accounts exist but a legacy single-account Gmail token cache is
/// present, adopt it (no re-consent). Returns the adopted email.
pub async fn migrate_legacy_if_needed() -> Result<Option<String>> {
    if !list_accounts().is_empty() {
        return Ok(None);
    }
    let legacy = config::token_cache_path();
    if !legacy.exists() {
        return Ok(None);
    }

    let auth = GmailAuth::new(config::secret_path(), legacy.clone(), config::SCOPES);
    let client = GmailClient::new(Arc::new(auth));
    let email = client.profile().await?.email;

    let dir = account_dir(&email);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(&legacy, token_path(&email)).ok();
    std::fs::write(dir.join("email"), &email)?;
    std::fs::write(dir.join("provider"), Provider::Gmail.as_str())?;
    Ok(Some(email))
}
