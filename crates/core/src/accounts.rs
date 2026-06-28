//! Per-account storage and the add-account flows (Gmail + Outlook).
//!
//! Each account lives in its own directory under [`config::accounts_dir`]:
//! ```text
//! accounts/<sanitized-email>/email              # the real address
//! accounts/<sanitized-email>/provider           # "gmail" | "outlook"
//! accounts/<sanitized-email>/token.json         # OAuth tokens
//! accounts/<sanitized-email>/metadata.sqlite3   # message cache + sync token
//! ```

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

use crate::auth::{AuthPrompt, GmailAuth};
use crate::config;
use crate::gmail::GmailClient;
use crate::outlook::{MsAuth, OutlookClient};
use crate::provider::MailProvider;

/// Callback the add-account flow uses to surface sign-in prompts to the UI.
pub type PromptFn = Arc<dyn Fn(AuthPrompt) + Send + Sync>;

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

/// An `InstalledFlowDelegate` that opens the browser and reports the consent
/// URL to the UI instead of printing to stdout (which would corrupt the TUI).
struct BrowserDelegate {
    on_prompt: PromptFn,
}

impl InstalledFlowDelegate for BrowserDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<String, String>> + Send + 'a>> {
        let on_prompt = self.on_prompt.clone();
        let url = url.to_string();
        Box::pin(async move {
            let _ = open::that(&url);
            on_prompt(AuthPrompt::Browser { url });
            Ok(String::new())
        })
    }
}

async fn gmail_consent(token_path: &Path, on_prompt: PromptFn) -> Result<()> {
    let secret = yup_oauth2::read_application_secret(config::secret_path())
        .await
        .context("reading Gmail client secret (configure it first)")?;
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .persist_tokens_to_disk(token_path.to_path_buf())
        .flow_delegate(Box::new(BrowserDelegate { on_prompt }))
        .build()
        .await
        .context("building authenticator")?;
    let scopes: Vec<&str> = config::SCOPES.to_vec();
    auth.token(&scopes).await.context("authorizing Gmail")?;
    Ok(())
}

/// Run the sign-in flow for a new account of `provider`, reporting prompts via
/// `on_prompt`. Returns the account email.
pub async fn add_account(provider: Provider, on_prompt: PromptFn) -> Result<String> {
    let pending = config::accounts_dir().join(".pending");
    std::fs::create_dir_all(&pending).ok();
    let pending_token = pending.join("token.json");
    let _ = std::fs::remove_file(&pending_token);

    let email = match provider {
        Provider::Gmail => {
            gmail_consent(&pending_token, on_prompt).await?;
            let auth =
                GmailAuth::new(config::secret_path(), pending_token.clone(), config::SCOPES);
            GmailClient::new(Arc::new(auth)).profile().await?.email
        }
        Provider::Outlook => {
            let client_id = config::ms_client_id().context(
                "set your Azure app id (Outlook) before adding the account",
            )?;
            let auth = MsAuth::new(client_id, pending_token.clone());
            auth.device_login(on_prompt.as_ref()).await?;
            OutlookClient::new(Arc::new(auth)).profile().await?.email
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
