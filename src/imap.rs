//! Generic IMAP provider (Yahoo, iCloud, Fastmail, …).
//!
//! **Experimental and untested** — implemented but not yet verified against a
//! live account. IMAP is a stateful, synchronous protocol; each operation opens
//! a fresh TLS session (via `spawn_blocking`), which is simple but not the most
//! efficient. Message IDs are IMAP UIDs (as strings).

use std::net::TcpStream;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use native_tls::TlsStream;

use crate::cache::Cache;
use crate::gmail::{AttachmentInfo, FetchProgress, FetchReport, Profile};
use crate::model::{MessageBody, MessageMeta};
use crate::provider::{MailProvider, ProgressCallback, SyncResult};
use crate::unsubscribe::UnsubscribeInfo;

type Session = imap::Session<TlsStream<TcpStream>>;

/// Connection settings for one IMAP account.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Clone)]
pub struct ImapClient {
    cfg: ImapConfig,
    cache: Option<Cache>,
}

impl ImapClient {
    pub fn new(cfg: ImapConfig) -> Self {
        Self { cfg, cache: None }
    }

    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    fn cfg(&self) -> ImapConfig {
        self.cfg.clone()
    }
}

/// Open an authenticated session and SELECT INBOX.
///
/// The connection is always TLS-encrypted. Port 143 uses STARTTLS (connect in
/// the clear, then upgrade before authenticating); every other port — notably
/// 993, the default and what all major providers use — uses implicit TLS from
/// the first byte. Mailsweep never sends credentials over an unencrypted link.
fn open_inbox(cfg: &ImapConfig) -> Result<Session> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .context("building TLS connector")?;
    let addr = (cfg.host.as_str(), cfg.port);
    let client = if cfg.port == 143 {
        imap::connect_starttls(addr, cfg.host.as_str(), &tls)
            .context("connecting to IMAP server (STARTTLS)")?
    } else {
        imap::connect(addr, cfg.host.as_str(), &tls)
            .context("connecting to IMAP server (implicit TLS)")?
    };
    let mut session = client
        .login(&cfg.username, &cfg.password)
        .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))?;
    session.select("INBOX").context("selecting INBOX")?;
    Ok(session)
}

/// Validate credentials by logging in (used when adding an account).
pub fn verify(cfg: &ImapConfig) -> Result<()> {
    let mut session = open_inbox(cfg)?;
    let _ = session.logout();
    Ok(())
}

fn cow_string(b: Option<&[u8]>) -> Option<String> {
    b.map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Extract a header value from raw `BODY[HEADER.FIELDS (...)]` bytes.
fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

const FETCH_ITEMS: &str = "(UID ENVELOPE RFC822.SIZE INTERNALDATE \
    BODY.PEEK[HEADER.FIELDS (LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST)])";

fn fetch_metas(session: &mut Session, uids: &[String]) -> Result<Vec<MessageMeta>> {
    let set = uids.join(",");
    let fetches = session.uid_fetch(set, FETCH_ITEMS)?;
    let mut out = Vec::new();
    for f in fetches.iter() {
        let Some(uid) = f.uid else { continue };
        let env = f.envelope();
        let (from_name, from_email) =
            match env.and_then(|e| e.from.as_ref()).and_then(|v| v.first()) {
                Some(addr) => {
                    let name = cow_string(addr.name);
                    let mbox = cow_string(addr.mailbox).unwrap_or_default();
                    let host = cow_string(addr.host).unwrap_or_default();
                    (name, format!("{mbox}@{host}"))
                }
                None => (None, String::new()),
            };
        let subject = env
            .and_then(|e| cow_string(e.subject))
            .unwrap_or_else(|| "(no subject)".to_string());
        let internal_date = f.internal_date().map(|d| d.timestamp_millis()).unwrap_or(0);
        let headers = f.header().unwrap_or(&[]);
        out.push(MessageMeta {
            id: uid.to_string(),
            thread_id: String::new(),
            from_name,
            from_email,
            subject,
            size_estimate: f.size.unwrap_or(0) as u64,
            internal_date,
            list_unsubscribe: header_value(headers, "List-Unsubscribe"),
            list_unsubscribe_post: header_value(headers, "List-Unsubscribe-Post"),
        });
    }
    Ok(out)
}

#[async_trait]
impl MailProvider for ImapClient {
    async fn profile(&self) -> Result<Profile> {
        let cfg = self.cfg();
        tokio::task::spawn_blocking(move || -> Result<Profile> {
            let mut s = open_inbox(&cfg)?;
            let mailbox = s.select("INBOX")?;
            let total = mailbox.exists as u64;
            let _ = s.logout();
            Ok(Profile {
                email: cfg.username.clone(),
                messages_total: total,
                threads_total: 0,
                history_id: String::new(),
            })
        })
        .await?
    }

    async fn inbox_sync(&self, _token: Option<&str>, max: usize) -> Result<SyncResult> {
        // IMAP has no cheap delta; always a full snapshot of inbox UIDs.
        let cfg = self.cfg();
        let added = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut s = open_inbox(&cfg)?;
            let mut uids: Vec<u32> = s.uid_search("ALL")?.into_iter().collect();
            let _ = s.logout();
            uids.sort_unstable();
            Ok(uids
                .into_iter()
                .rev()
                .take(max)
                .map(|u| u.to_string())
                .collect())
        })
        .await??;
        Ok(SyncResult {
            added,
            removed: Vec::new(),
            next_token: String::new(),
            full: true,
        })
    }

    async fn list_attachment_ids(&self, _max: usize) -> Result<Vec<String>> {
        // No portable IMAP search for attachments; skipped.
        Ok(Vec::new())
    }

    async fn list_query_ids(&self, query: &str, max: usize) -> Result<Vec<String>> {
        let cfg = self.cfg();
        let query = query.to_string();
        let ids = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut s = open_inbox(&cfg)?;
            let mut uids: Vec<u32> = s.uid_search(&query)?.into_iter().collect();
            let _ = s.logout();
            uids.sort_unstable();
            Ok(uids
                .into_iter()
                .rev()
                .take(max)
                .map(|u| u.to_string())
                .collect())
        })
        .await??;
        Ok(ids)
    }

    async fn fetch_metadata(
        &self,
        ids: &[String],
        on_update: ProgressCallback<'_>,
    ) -> Result<FetchReport> {
        let total = ids.len();
        let mut known: std::collections::HashMap<String, MessageMeta> = match &self.cache {
            Some(c) => c.get_many(ids).await?,
            None => Default::default(),
        };
        let from_cache = known.len();
        let cached: Vec<MessageMeta> = known.values().cloned().collect();
        on_update(
            FetchProgress {
                resolved: from_cache,
                total,
            },
            &cached,
        );

        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !known.contains_key(*id))
            .cloned()
            .collect();

        let mut fetched = Vec::new();
        if !missing.is_empty() {
            let cfg = self.cfg();
            fetched = tokio::task::spawn_blocking(move || -> Result<Vec<MessageMeta>> {
                let mut s = open_inbox(&cfg)?;
                let mut out = Vec::new();
                for chunk in missing.chunks(200) {
                    out.extend(fetch_metas(&mut s, chunk)?);
                }
                let _ = s.logout();
                Ok(out)
            })
            .await??;
            if let Some(c) = &self.cache {
                c.put_many(&fetched).await.ok();
            }
        }

        on_update(
            FetchProgress {
                resolved: from_cache + fetched.len(),
                total,
            },
            &fetched,
        );
        for m in fetched.iter().cloned() {
            known.insert(m.id.clone(), m);
        }

        let metas = ids.iter().filter_map(|id| known.get(id).cloned()).collect();
        Ok(FetchReport {
            metas,
            requested: total,
            from_cache,
            fetched: fetched.len(),
            batch_errors: Vec::new(),
        })
    }

    async fn trash(&self, ids: &[String]) -> Result<()> {
        self.move_to("Trash", ids).await
    }

    async fn mark_spam(&self, ids: &[String]) -> Result<()> {
        self.move_to("Junk", ids).await
    }

    async fn mark_read(&self, ids: &[String]) -> Result<()> {
        let cfg = self.cfg();
        let set = ids.join(",");
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut s = open_inbox(&cfg)?;
            s.uid_store(set, "+FLAGS (\\Seen)")?;
            let _ = s.logout();
            Ok(())
        })
        .await?
    }

    async fn restore(&self, _ids: &[String]) -> Result<()> {
        // The message's UID changes when it moves to Trash, so we can't restore
        // it by its old inbox UID.
        bail!("undo is not supported on IMAP")
    }

    async fn unsubscribe_one_click(&self, info: &UnsubscribeInfo) -> Result<bool> {
        crate::unsubscribe::one_click(&reqwest::Client::new(), info).await
    }

    async fn message_attachments(&self, _id: &str) -> Result<Vec<AttachmentInfo>> {
        // Attachment enumeration (BODYSTRUCTURE) is not implemented for IMAP.
        Ok(Vec::new())
    }

    async fn download_attachment(
        &self,
        _message_id: &str,
        _attachment_id: &str,
    ) -> Result<Vec<u8>> {
        bail!("attachment download is not implemented for IMAP")
    }

    async fn download_raw_message(&self, message_id: &str) -> Result<Vec<u8>> {
        let cfg = self.cfg();
        let uid = message_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut s = open_inbox(&cfg)?;
            let fetches = s.uid_fetch(&uid, "BODY.PEEK[]")?;
            let body = fetches
                .iter()
                .next()
                .and_then(|f| f.body())
                .map(|b| b.to_vec())
                .unwrap_or_default();
            let _ = s.logout();
            Ok(body)
        })
        .await?
    }

    async fn fetch_message_body(&self, message_id: &str) -> Result<MessageBody> {
        let cfg = self.cfg();
        let uid = message_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<MessageBody> {
            let mut s = open_inbox(&cfg)?;
            let fetches = s.uid_fetch(&uid, "(ENVELOPE INTERNALDATE BODY.PEEK[TEXT])")?;
            let f = fetches.iter().next();
            let env = f.and_then(|f| f.envelope());
            let subject = env.and_then(|e| cow_string(e.subject)).unwrap_or_default();
            let from = env
                .and_then(|e| e.from.as_ref())
                .and_then(|v| v.first())
                .map(|a| {
                    format!(
                        "{}@{}",
                        cow_string(a.mailbox).unwrap_or_default(),
                        cow_string(a.host).unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            let date_ms = f
                .and_then(|f| f.internal_date())
                .map(|d| d.timestamp_millis())
                .unwrap_or(0);
            let text = f
                .and_then(|f| f.text())
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let _ = s.logout();
            Ok(MessageBody {
                subject,
                from,
                to: String::new(),
                date_ms,
                text,
            })
        })
        .await?
    }

    fn query_help(&self) -> &'static str {
        "IMAP search · UNSEEN · SINCE 1-Jan-2024 · LARGER 5000000 · FROM amazon"
    }

    fn query_examples(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("Unread", "UNSEEN"),
            ("Since a date", "SINCE 1-Jan-2024"),
            ("Larger than ~5 MB", "LARGER 5000000"),
            ("From a sender", "FROM amazon"),
            ("Subject contains", "SUBJECT invoice"),
        ]
    }
}

impl ImapClient {
    async fn move_to(&self, folder: &str, ids: &[String]) -> Result<()> {
        let cfg = self.cfg();
        let folder = folder.to_string();
        let set = ids.join(",");
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut s = open_inbox(&cfg)?;
            s.uid_mv(&set, &folder)
                .with_context(|| format!("moving to {folder} (folder may not exist)"))?;
            let _ = s.logout();
            Ok(())
        })
        .await??;
        if let Some(cache) = &self.cache {
            cache.remove(ids).await.ok();
        }
        Ok(())
    }
}
