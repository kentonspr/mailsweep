//! Google OAuth via the installed-app (loopback redirect) flow.
//!
//! On first use this opens a browser for consent and caches the refresh token
//! to disk; subsequent runs refresh silently.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

/// Access tokens are valid for ~1h; refresh a little early.
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

/// A sign-in prompt surfaced to the UI during an interactive add-account flow.
#[derive(Debug, Clone)]
pub enum AuthPrompt {
    /// Gmail: a browser was opened to this consent URL.
    Browser { url: String },
    /// Outlook: enter `user_code` at `verification_uri`.
    DeviceCode {
        verification_uri: String,
        user_code: String,
        message: String,
    },
}

pub struct GmailAuth {
    secret_path: PathBuf,
    cache_path: PathBuf,
    scopes: Vec<String>,
    cached: Mutex<Option<(String, Instant)>>,
}

impl GmailAuth {
    pub fn new(secret_path: PathBuf, cache_path: PathBuf, scopes: &[&str]) -> Self {
        Self {
            secret_path,
            cache_path,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            cached: Mutex::new(None),
        }
    }

    /// Return a valid bearer access token, refreshing if the cached one is stale.
    pub async fn access_token(&self) -> Result<String> {
        let mut guard = self.cached.lock().await;
        if let Some((token, fetched)) = guard.as_ref() {
            if fetched.elapsed() < TOKEN_TTL {
                return Ok(token.clone());
            }
        }
        let token = self.refresh().await?;
        *guard = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn refresh(&self) -> Result<String> {
        let secret = yup_oauth2::read_application_secret(&self.secret_path)
            .await
            .with_context(|| {
                format!(
                    "reading OAuth client secret from {}",
                    self.secret_path.display()
                )
            })?;

        let authenticator =
            InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
                .persist_tokens_to_disk(&self.cache_path)
                .build()
                .await
                .context("building OAuth authenticator")?;

        let scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let token = authenticator
            .token(&scopes)
            .await
            .context("requesting access token")?;

        token
            .token()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("authenticator returned no access token"))
    }
}
