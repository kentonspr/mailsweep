//! Filesystem locations and OAuth scopes.

use std::path::PathBuf;

use directories::ProjectDirs;

/// `gmail.modify` covers reading, trashing, and label changes (spam).
/// It intentionally does NOT allow permanent deletion — we trash instead.
pub const SCOPES: &[&str] = &["https://www.googleapis.com/auth/gmail.modify"];

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "mailsweep", "mailsweep")
}

/// Directory holding the OAuth client secret and cached tokens.
pub fn config_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
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
    config_dir().join("archives")
}

/// Directory holding per-account state (tokens, caches), one subdir each.
pub fn accounts_dir() -> PathBuf {
    config_dir().join("accounts")
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
