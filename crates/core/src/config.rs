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
