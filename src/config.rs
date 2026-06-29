//! Filesystem locations and OAuth scopes.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;

/// `gmail.modify` covers reading, trashing, and label changes (spam).
/// It intentionally does NOT allow permanent deletion — we trash instead.
pub const SCOPES: &[&str] = &["https://www.googleapis.com/auth/gmail.modify"];

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "mailsweep", "mailsweep")
}

/// Directory holding user configuration (OAuth client credentials, settings).
pub fn config_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Directory holding generated data (per-account tokens + caches, archives).
pub fn data_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One-time move of data that earlier versions kept under the config dir into
/// the data dir.
pub fn migrate_to_data_dir() {
    let config = config_dir();
    let data = data_dir();
    for sub in ["accounts", "archives"] {
        let old = config.join(sub);
        let new = data.join(sub);
        if old.exists() && !new.exists() {
            if let Some(parent) = new.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::rename(&old, &new).ok();
        }
    }
}

/// Path to the Google OAuth "Desktop app" client secret JSON.
///
/// Override with the `MAILSWEEP_CLIENT_SECRET` environment variable.
pub fn secret_path() -> PathBuf {
    std::env::var("MAILSWEEP_CLIENT_SECRET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config_dir().join("client_secret.json"))
}

/// Path where the refresh/access tokens are cached between runs.
pub fn token_cache_path() -> PathBuf {
    config_dir().join("token_cache.json")
}

/// Path to the SQLite message-metadata cache.
pub fn cache_path() -> PathBuf {
    config_dir().join("metadata.sqlite3")
}

/// Directory where attachment archives are written.
pub fn archive_dir() -> PathBuf {
    data_dir().join("archives")
}

/// Directory holding per-account state (tokens, caches), one subdir each.
pub fn accounts_dir() -> PathBuf {
    data_dir().join("accounts")
}

/// Microsoft Graph delegated scopes (personal accounts; device-code flow).
pub const MS_SCOPE: &str = "offline_access User.Read Mail.ReadWrite";

/// The user's Azure app (public client) ID for Outlook sign-in.
///
/// From `MAILSWEEP_MS_CLIENT_ID`, or `~/.config/mailsweep/ms_client_id`.
pub fn ms_client_id() -> Option<String> {
    if let Ok(v) = std::env::var("MAILSWEEP_MS_CLIENT_ID") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    std::fs::read_to_string(config_dir().join("ms_client_id"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether the Gmail OAuth client secret is configured.
pub fn gmail_configured() -> bool {
    secret_path().exists()
}

/// Whether the Outlook (Azure) app id is configured.
pub fn outlook_configured() -> bool {
    ms_client_id().is_some()
}

/// Treat `input` as a file path if it points to one, else as the raw value.
fn resolve_input(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let path = std::path::Path::new(trimmed);
    if path.is_file() {
        std::fs::read_to_string(path).with_context(|| format!("reading {trimmed}"))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Save a Gmail `client_secret.json` from a path or pasted JSON.
pub fn save_gmail_secret(input: &str) -> Result<()> {
    let content = resolve_input(input)?;
    if !content.trim_start().starts_with('{') {
        bail!("expected client_secret.json content (or a path to it)");
    }
    let dest = config_dir().join("client_secret.json");
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&dest, content.as_bytes()).context("writing client secret")?;
    Ok(())
}

/// Save the Outlook (Azure) app id from a path or pasted value.
pub fn save_ms_client_id(input: &str) -> Result<()> {
    let id = resolve_input(input)?.trim().to_string();
    if id.is_empty() {
        bail!("empty Azure app id");
    }
    let dest = config_dir().join("ms_client_id");
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&dest, id).context("writing Azure app id")?;
    Ok(())
}

/// Maximum number of messages to scan.
///
/// Defaults to no limit (the whole inbox). Override with the
/// `MAILSWEEP_SCAN_LIMIT` environment variable.
pub fn scan_limit() -> usize {
    std::env::var("MAILSWEEP_SCAN_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(usize::MAX)
}
