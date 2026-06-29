//! Library core for Mailsweep: mail providers (Gmail, Outlook, IMAP), OAuth,
//! sync, caching, message grouping, archiving, and unsubscribe handling. The
//! `mailsweep` binary (`main.rs`, the terminal UI) builds on top of this.

pub mod accounts;
pub mod archive;
pub mod auth;
pub mod cache;
pub mod config;
pub mod gmail;
pub mod imap;
pub mod lock;
pub mod model;
pub mod outlook;
pub mod provider;
pub mod unsubscribe;

pub use archive::{archive_messages, ArchiveItem, ArchiveScope, ArchiveSummary};
pub use auth::{AuthPrompt, GmailAuth};
pub use cache::Cache;
pub use gmail::{AttachmentInfo, FetchProgress, FetchReport, GmailClient, HistoryDelta, Profile};
pub use model::{
    group_by_domain, group_messages, DomainGroup, MessageBody, MessageMeta, SenderEntry,
    SenderGroup,
};
pub use provider::{MailProvider, SyncResult};
pub use unsubscribe::UnsubscribeInfo;
