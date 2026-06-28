//! Per-account storage and the add-account OAuth flow.
//!
//! Each account lives in its own directory under [`config::accounts_dir`]:
//! ```text
//! accounts/<sanitized-email>/email              # the real address
//! accounts/<sanitized-email>/token.json         # OAuth tokens
//! accounts/<sanitized-email>/metadata.sqlite3   # message cache + historyId
//! ```
//! All accounts share the one user-provided OAuth client (`client_secret.json`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::auth::GmailAuth;
use crate::config;
use crate::gmail::GmailClient;

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

/// List the configured account emails (sorted).
pub fn list_accounts() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(config::accounts_dir()) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Ok(email) = std::fs::read_to_string(entry.path().join("email")) {
                    let email = email.trim().to_string();
                    if !email.is_empty() {
                        out.push(email);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Run the OAuth consent flow for a new account and persist it.
///
/// Opens a browser and blocks until the user consents. Returns the email of the
/// account that was authorized.
pub async fn add_account() -> Result<String> {
    let pending = config::accounts_dir().join(".pending");
    std::fs::create_dir_all(&pending).ok();
    let pending_token = pending.join("token.json");
    let _ = std::fs::remove_file(&pending_token);

    // Building the client and asking for the profile triggers the consent flow,
    // writing tokens to the pending path.
    let auth = GmailAuth::new(config::secret_path(), pending_token.clone(), config::SCOPES);
    let client = GmailClient::new(Arc::new(auth));
    let profile = client.profile().await?;
    let email = profile.email;

    let dir = account_dir(&email);
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::rename(&pending_token, token_path(&email));
    std::fs::write(dir.join("email"), &email)?;
    let _ = std::fs::remove_dir_all(&pending);
    Ok(email)
}

/// If no accounts exist yet but a legacy single-account token cache is present,
/// adopt it as the first account (no re-consent). Returns the adopted email.
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
    let profile = client.profile().await?;
    let email = profile.email;

    let dir = account_dir(&email);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(&legacy, token_path(&email)).ok();
    std::fs::write(dir.join("email"), &email)?;
    Ok(Some(email))
}
