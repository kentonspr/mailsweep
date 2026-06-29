//! The provider abstraction. Gmail implements this today; Outlook (Microsoft
//! Graph) and IMAP-based providers can implement it without touching the
//! frontends.

use anyhow::Result;
use async_trait::async_trait;

use crate::gmail::{AttachmentInfo, FetchProgress, FetchReport, Profile};
use crate::model::{MessageBody, MessageMeta};
use crate::unsubscribe::UnsubscribeInfo;

/// Result of an inbox sync, full or incremental.
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Message IDs currently in the inbox (full sync) or newly added (incremental).
    pub added: Vec<String>,
    /// Message IDs removed from the inbox (incremental only).
    pub removed: Vec<String>,
    /// Opaque checkpoint token to store and pass back next time.
    pub next_token: String,
    /// True for a full snapshot (`added` is the entire inbox), false for a delta.
    pub full: bool,
}

/// A boxed progress callback for streaming metadata fetches (object-safe, so
/// the trait can be used as `dyn MailProvider`).
pub type ProgressCallback<'a> = &'a mut (dyn FnMut(FetchProgress, &[MessageMeta]) + Send);

#[async_trait]
pub trait MailProvider: Send + Sync {
    /// The authenticated account's profile (email, mailbox totals).
    async fn profile(&self) -> Result<Profile>;

    /// Sync the inbox. With `token = None` (or an expired token) this returns a
    /// full snapshot; otherwise the changes since `token`. Always returns a
    /// fresh `next_token`.
    async fn inbox_sync(&self, token: Option<&str>, max: usize) -> Result<SyncResult>;

    /// Message IDs in the inbox that carry attachments.
    async fn list_attachment_ids(&self, max: usize) -> Result<Vec<String>>;

    /// Fetch header metadata for `ids`, reporting progress via `on_update`.
    async fn fetch_metadata(
        &self,
        ids: &[String],
        on_update: ProgressCallback<'_>,
    ) -> Result<FetchReport>;

    /// Move messages to trash (reversible).
    async fn trash(&self, ids: &[String]) -> Result<()>;

    /// Mark messages as spam/junk.
    async fn mark_spam(&self, ids: &[String]) -> Result<()>;

    /// Perform a one-click (RFC 8058) unsubscribe; `Ok(true)` on success.
    async fn unsubscribe_one_click(&self, info: &UnsubscribeInfo) -> Result<bool>;

    /// List a message's attachments (filename, type, size, download handle).
    async fn message_attachments(&self, id: &str) -> Result<Vec<AttachmentInfo>>;

    /// Download one attachment's raw bytes.
    async fn download_attachment(&self, message_id: &str, attachment_id: &str)
        -> Result<Vec<u8>>;

    /// Download the full raw RFC 822 message (`.eml`), including attachments.
    async fn download_raw_message(&self, message_id: &str) -> Result<Vec<u8>>;

    /// Fetch a message's readable body + headers for the in-app viewer.
    async fn fetch_message_body(&self, message_id: &str) -> Result<MessageBody>;
}
