//! The provider abstraction. Gmail implements this today; Outlook (Microsoft
//! Graph) and IMAP-based providers (Yahoo, etc.) can implement it later without
//! touching the frontends.

use anyhow::Result;
use async_trait::async_trait;

use crate::model::MessageMeta;

#[async_trait]
pub trait MailProvider {
    /// List message IDs matching an optional provider-specific query, capped at `max`.
    async fn list_message_ids(&self, query: Option<&str>, max: usize) -> Result<Vec<String>>;

    /// Fetch lightweight metadata (headers only) for the given message IDs.
    async fn fetch_metadata(&self, ids: &[String]) -> Result<Vec<MessageMeta>>;

    /// Move messages to trash (reversible).
    async fn trash(&self, ids: &[String]) -> Result<()>;

    /// Mark messages as spam.
    async fn mark_spam(&self, ids: &[String]) -> Result<()>;
}
