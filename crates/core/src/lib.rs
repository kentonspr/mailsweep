//! Shared core for Mailsweep: OAuth, the Gmail provider, message grouping,
//! and unsubscribe handling. Frontends (TUI / GUI) build on top of this.

pub mod accounts;
pub mod archive;
pub mod auth;
pub mod cache;
pub mod config;
pub mod gmail;
pub mod model;
pub mod outlook;
pub mod provider;
pub mod unsubscribe;

pub use archive::{archive_attachments, ArchiveItem, ArchiveSummary};
pub use auth::GmailAuth;
pub use cache::Cache;
pub use gmail::{
    AttachmentInfo, FetchProgress, FetchReport, GmailClient, HistoryDelta, Profile,
};
pub use model::{
    group_by_domain, group_messages, DomainGroup, MessageMeta, SenderEntry, SenderGroup,
};
pub use provider::{MailProvider, SyncResult};
pub use unsubscribe::UnsubscribeInfo;
