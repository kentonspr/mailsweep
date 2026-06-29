//! Gmail REST API client (https://gmail.googleapis.com/gmail/v1).
//!
//! We hit the REST endpoints directly with `reqwest` rather than using a
//! generated client. Metadata is fetched via the Gmail batch endpoint (up to
//! 100 sub-requests per HTTP call) and cached on disk so rescans are cheap.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

use crate::auth::GmailAuth;
use crate::cache::Cache;
use crate::model::MessageMeta;
use crate::provider::{MailProvider, ProgressCallback, SyncResult};
use crate::unsubscribe::UnsubscribeInfo;

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const BATCH_URL: &str = "https://gmail.googleapis.com/batch/gmail/v1";
/// Messages per batch HTTP call. `messages.get` costs 5 quota units and the
/// per-user budget is ~250 units/sec, so a batch of 50 (≈250 units) sent once
/// per `BATCH_PACING` keeps us under the limit and avoids per-message 429s.
const BATCH_GET_LIMIT: usize = 50;
/// Minimum spacing between batch HTTP calls (rate-limit pacing).
const BATCH_PACING: Duration = Duration::from_millis(1100);
/// How many times to retry messages that a batch dropped (e.g. transient 429s).
const MAX_FETCH_RETRIES: usize = 4;
/// `batchModify` accepts at most 1000 IDs per call.
const MODIFY_LIMIT: usize = 1000;
/// Header set we request per message (cheap — no body).
const METADATA_QUERY: &str = "format=metadata\
    &metadataHeaders=From\
    &metadataHeaders=Subject\
    &metadataHeaders=List-Unsubscribe\
    &metadataHeaders=List-Unsubscribe-Post";

#[derive(Clone)]
pub struct GmailClient {
    http: Client,
    auth: Arc<GmailAuth>,
    cache: Option<Cache>,
}

/// Diagnostic summary of a metadata fetch.
#[derive(Debug, Clone)]
pub struct FetchReport {
    pub metas: Vec<MessageMeta>,
    /// Number of IDs we were asked to resolve.
    pub requested: usize,
    /// How many were served from the on-disk cache.
    pub from_cache: usize,
    /// How many were freshly fetched from the batch endpoint.
    pub fetched: usize,
    /// One message per failed batch HTTP call (empty when all succeeded).
    pub batch_errors: Vec<String>,
}

/// Progress update emitted while resolving metadata, for live UI feedback.
#[derive(Debug, Clone, Copy)]
pub struct FetchProgress {
    pub resolved: usize,
    pub total: usize,
}

/// Authenticated account profile (from `users.getProfile`).
#[derive(Debug, Clone)]
pub struct Profile {
    pub email: String,
    pub messages_total: u64,
    pub threads_total: u64,
    /// Mailbox history checkpoint, for incremental sync.
    pub history_id: String,
}

/// Inbox changes since a stored `historyId`, from `users.history.list`.
#[derive(Debug, Clone, Default)]
pub struct HistoryDelta {
    /// Message IDs added to the inbox (new mail, or moved back in).
    pub added: Vec<String>,
    /// Message IDs removed from the inbox (deleted, trashed, or archived).
    pub removed: Vec<String>,
}

impl GmailClient {
    pub fn new(auth: Arc<GmailAuth>) -> Self {
        Self {
            http: Client::new(),
            auth,
            cache: None,
        }
    }

    /// Attach an on-disk metadata cache.
    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.auth.access_token().await?))
    }

    /// List message IDs for a Gmail search query (e.g. `"in:inbox"`), up to `max`.
    pub async fn list_message_ids(&self, query: Option<&str>, max: usize) -> Result<Vec<String>> {
        let bearer = self.bearer().await?;
        let mut ids = Vec::new();
        let mut page: Option<String> = None;

        loop {
            let mut req = self
                .http
                .get(format!("{BASE}/messages"))
                .header("Authorization", &bearer)
                .query(&[("maxResults", "500")]);
            if let Some(q) = query {
                req = req.query(&[("q", q)]);
            }
            if let Some(ref token) = page {
                req = req.query(&[("pageToken", token.as_str())]);
            }

            let resp: ListResp = req.send().await?.error_for_status()?.json().await?;
            ids.extend(resp.messages.into_iter().map(|m| m.id));

            if ids.len() >= max {
                ids.truncate(max);
                break;
            }
            match resp.next_page_token {
                Some(token) => page = Some(token),
                None => break,
            }
        }
        Ok(ids)
    }

    /// Fetch header-only metadata for many messages.
    ///
    /// Cached IDs are served from disk; the rest are fetched via the batch
    /// endpoint and written back to the cache. Results preserve input order.
    pub async fn fetch_metadata(&self, ids: &[String]) -> Result<Vec<MessageMeta>> {
        Ok(self.fetch_metadata_report(ids).await?.metas)
    }

    /// Like [`fetch_metadata`], but also reports where the data came from and
    /// any batch errors — useful for diagnosing missing messages.
    pub async fn fetch_metadata_report(&self, ids: &[String]) -> Result<FetchReport> {
        self.fetch_metadata_with(ids, |_, _| {}).await
    }

    /// Like [`fetch_metadata_report`], invoking `on_update` as messages are
    /// resolved (first the cached set, then each batch) so callers can render a
    /// live, incrementally-populating UI. The slice holds the messages newly
    /// resolved in that step.
    pub async fn fetch_metadata_with(
        &self,
        ids: &[String],
        mut on_update: impl FnMut(FetchProgress, &[MessageMeta]),
    ) -> Result<FetchReport> {
        let total = ids.len();
        let mut known: HashMap<String, MessageMeta> = match &self.cache {
            Some(cache) => cache.get_many(ids).await?,
            None => HashMap::new(),
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

        let mut fetched = 0;
        let mut batch_errors = Vec::new();
        if !missing.is_empty() {
            let bearer = self.bearer().await?;
            let (metas, errors) = self
                .batch_fetch(&bearer, &missing, |batch, fetched_so_far| {
                    on_update(
                        FetchProgress {
                            resolved: from_cache + fetched_so_far,
                            total,
                        },
                        batch,
                    );
                })
                .await?;
            fetched = metas.len();
            batch_errors = errors;
            if let Some(cache) = &self.cache {
                cache.put_many(&metas).await?;
            }
            for m in metas {
                known.insert(m.id.clone(), m);
            }
        }

        let metas = ids.iter().filter_map(|id| known.get(id).cloned()).collect();
        Ok(FetchReport {
            metas,
            requested: total,
            from_cache,
            fetched,
            batch_errors,
        })
    }

    /// Fetch metadata for `ids` (any count) via paced batch HTTP calls.
    ///
    /// Batches are sent sequentially with [`BATCH_PACING`] between them to stay
    /// under Gmail's per-user quota. Any message a batch drops (per-message 429,
    /// transient error) is retried with exponential backoff so nothing is
    /// silently lost. Returns the metadata plus a description of anything that
    /// still failed after all retries.
    async fn batch_fetch(
        &self,
        bearer: &str,
        ids: &[String],
        mut on_batch: impl FnMut(&[MessageMeta], usize),
    ) -> Result<(Vec<MessageMeta>, Vec<String>)> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let mut pending: Vec<String> = ids.to_vec();

        for attempt in 0..=MAX_FETCH_RETRIES {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                // Exponential backoff before retrying dropped messages: 2s, 4s, …
                sleep(Duration::from_secs(1u64 << attempt)).await;
            }

            let mut still_failed = Vec::new();
            for chunk in pending.chunks(BATCH_GET_LIMIT) {
                match batch_get(&self.http, bearer, chunk).await {
                    Ok((metas, failed)) => {
                        still_failed.extend(failed);
                        on_batch(&metas, out.len() + metas.len());
                        out.extend(metas);
                    }
                    Err(e) => {
                        // Whole batch HTTP call failed — retry the entire chunk.
                        still_failed.extend(chunk.iter().cloned());
                        if attempt == MAX_FETCH_RETRIES {
                            errors.push(e.to_string());
                        }
                        on_batch(&[], out.len());
                    }
                }
                sleep(BATCH_PACING).await;
            }
            pending = still_failed;
        }

        if !pending.is_empty() {
            errors.push(format!(
                "{} message(s) unresolved after {MAX_FETCH_RETRIES} retries",
                pending.len()
            ));
        }
        Ok((out, errors))
    }

    async fn batch_modify(&self, ids: &[String], add: &[&str], remove: &[&str]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let bearer = self.bearer().await?;
        for chunk in ids.chunks(MODIFY_LIMIT) {
            let body = serde_json::json!({
                "ids": chunk,
                "addLabelIds": add,
                "removeLabelIds": remove,
            });
            self.http
                .post(format!("{BASE}/messages/batchModify"))
                .header("Authorization", &bearer)
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
        }
        // Keep the cache consistent with the inbox.
        if let Some(cache) = &self.cache {
            cache.remove(ids).await?;
        }
        Ok(())
    }

    /// Convenience wrapper so frontends don't need a `reqwest` dependency.
    pub async fn unsubscribe_one_click(&self, info: &UnsubscribeInfo) -> Result<bool> {
        crate::unsubscribe::one_click(&self.http, info).await
    }

    /// Fetch the authenticated account's profile (email + mailbox totals).
    pub async fn profile(&self) -> Result<Profile> {
        let bearer = self.bearer().await?;
        let resp: ProfileResp = self
            .http
            .get(format!("{BASE}/profile"))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Profile {
            email: resp.email_address,
            messages_total: resp.messages_total,
            threads_total: resp.threads_total,
            history_id: resp.history_id,
        })
    }

    /// Fetch inbox changes since `start_history_id`.
    ///
    /// Returns `Ok(None)` if the history is too old to use (Gmail expires it
    /// after roughly a week) — the caller should fall back to a full sync.
    pub async fn history_since(&self, start_history_id: &str) -> Result<Option<HistoryDelta>> {
        let bearer = self.bearer().await?;
        let mut added: HashSet<String> = HashSet::new();
        let mut removed: HashSet<String> = HashSet::new();
        let mut page: Option<String> = None;

        loop {
            let mut req = self
                .http
                .get(format!("{BASE}/history"))
                .header("Authorization", &bearer)
                .query(&[
                    ("startHistoryId", start_history_id),
                    ("labelId", "INBOX"),
                    ("historyTypes", "messageAdded"),
                    ("historyTypes", "messageDeleted"),
                    ("historyTypes", "labelAdded"),
                    ("historyTypes", "labelRemoved"),
                    ("maxResults", "500"),
                ]);
            if let Some(ref token) = page {
                req = req.query(&[("pageToken", token.as_str())]);
            }

            let resp = req.send().await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None); // history expired
            }
            let resp: HistoryResp = resp.error_for_status()?.json().await?;

            for record in resp.history {
                for m in record.messages_added {
                    if m.message.label_ids.iter().any(|l| l == "INBOX") {
                        removed.remove(&m.message.id);
                        added.insert(m.message.id);
                    }
                }
                for m in record.messages_deleted {
                    added.remove(&m.message.id);
                    removed.insert(m.message.id);
                }
                for l in record.labels_added {
                    if l.label_ids.iter().any(|x| x == "INBOX") {
                        removed.remove(&l.message.id);
                        added.insert(l.message.id);
                    }
                }
                for l in record.labels_removed {
                    if l.label_ids.iter().any(|x| x == "INBOX") {
                        added.remove(&l.message.id);
                        removed.insert(l.message.id);
                    }
                }
            }

            match resp.next_page_token {
                Some(token) => page = Some(token),
                None => break,
            }
        }

        Ok(Some(HistoryDelta {
            added: added.into_iter().collect(),
            removed: removed.into_iter().collect(),
        }))
    }

    /// List a message's attachments (filename, type, size, download handle).
    pub async fn message_attachments(&self, message_id: &str) -> Result<Vec<AttachmentInfo>> {
        let bearer = self.bearer().await?;
        let resp: FullMessageResp = self
            .http
            .get(format!("{BASE}/messages/{message_id}"))
            .header("Authorization", &bearer)
            .query(&[("format", "full")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut out = Vec::new();
        collect_attachments(&resp.payload, &mut out);
        Ok(out)
    }

    /// Download one attachment's raw bytes.
    pub async fn download_attachment(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>> {
        let bearer = self.bearer().await?;
        let resp: AttachmentResp = self
            .http
            .get(format!(
                "{BASE}/messages/{message_id}/attachments/{attachment_id}"
            ))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        base64_url()
            .decode(resp.data.as_bytes())
            .map_err(|e| anyhow!("decoding attachment data: {e}"))
    }
}

/// Information about a single attachment part.
#[derive(Debug, Clone)]
pub struct AttachmentInfo {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub attachment_id: String,
}

/// Gmail attachment data is base64url, sometimes unpadded — accept either.
fn base64_url() -> GeneralPurpose {
    GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
}

/// Walk a MIME part tree, collecting parts that are downloadable attachments.
fn collect_attachments(part: &FullPart, out: &mut Vec<AttachmentInfo>) {
    if !part.filename.is_empty() {
        if let Some(id) = &part.body.attachment_id {
            out.push(AttachmentInfo {
                filename: part.filename.clone(),
                mime_type: part.mime_type.clone(),
                size: part.body.size,
                attachment_id: id.clone(),
            });
        }
    }
    for child in &part.parts {
        collect_attachments(child, out);
    }
}

#[async_trait]
impl MailProvider for GmailClient {
    async fn profile(&self) -> Result<Profile> {
        GmailClient::profile(self).await
    }

    async fn inbox_sync(&self, token: Option<&str>, max: usize) -> Result<SyncResult> {
        // Capture the checkpoint before reading, so changes during the sync are
        // picked up next time rather than missed.
        let next_token = GmailClient::profile(self).await?.history_id;
        if let Some(start) = token {
            if let Some(delta) = self.history_since(start).await? {
                return Ok(SyncResult {
                    added: delta.added,
                    removed: delta.removed,
                    next_token,
                    full: false,
                });
            }
        }
        let ids = self.list_message_ids(Some("in:inbox"), max).await?;
        Ok(SyncResult {
            added: ids,
            removed: Vec::new(),
            next_token,
            full: true,
        })
    }

    async fn list_attachment_ids(&self, max: usize) -> Result<Vec<String>> {
        self.list_message_ids(Some("in:inbox has:attachment"), max)
            .await
    }

    async fn fetch_metadata(
        &self,
        ids: &[String],
        on_update: ProgressCallback<'_>,
    ) -> Result<FetchReport> {
        self.fetch_metadata_with(ids, |p, batch| on_update(p, batch))
            .await
    }

    async fn trash(&self, ids: &[String]) -> Result<()> {
        // Adding the TRASH label moves messages to trash (reversible).
        self.batch_modify(ids, &["TRASH"], &["INBOX"]).await
    }

    async fn mark_spam(&self, ids: &[String]) -> Result<()> {
        self.batch_modify(ids, &["SPAM"], &["INBOX"]).await
    }

    async fn unsubscribe_one_click(&self, info: &UnsubscribeInfo) -> Result<bool> {
        GmailClient::unsubscribe_one_click(self, info).await
    }

    async fn message_attachments(&self, id: &str) -> Result<Vec<AttachmentInfo>> {
        GmailClient::message_attachments(self, id).await
    }

    async fn download_attachment(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>> {
        GmailClient::download_attachment(self, message_id, attachment_id).await
    }

    async fn download_raw_message(&self, message_id: &str) -> Result<Vec<u8>> {
        let bearer = self.bearer().await?;
        let resp: RawMessageResp = self
            .http
            .get(format!("{BASE}/messages/{message_id}"))
            .header("Authorization", &bearer)
            .query(&[("format", "raw")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        base64_url()
            .decode(resp.raw.as_bytes())
            .map_err(|e| anyhow!("decoding raw message: {e}"))
    }
}

/// Issue a single Gmail batch request for up to `BATCH_GET_LIMIT` messages.
///
/// Returns the successfully-parsed metadata and the IDs that did not come back
/// (e.g. per-message 429s), so the caller can retry the latter.
async fn batch_get(
    http: &Client,
    bearer: &str,
    ids: &[String],
) -> Result<(Vec<MessageMeta>, Vec<String>)> {
    let boundary = "mailsweep_batch_boundary";

    let mut body = String::new();
    for id in ids {
        body.push_str(&format!("--{boundary}\r\n"));
        body.push_str("Content-Type: application/http\r\n");
        body.push_str(&format!("Content-ID: <{id}>\r\n\r\n"));
        body.push_str(&format!(
            "GET /gmail/v1/users/me/messages/{id}?{METADATA_QUERY} HTTP/1.1\r\n\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));

    let resp = http
        .post(BATCH_URL)
        .header("Authorization", bearer)
        .header(CONTENT_TYPE, format!("multipart/mixed; boundary={boundary}"))
        .body(body)
        .send()
        .await?
        .error_for_status()?;

    // The response uses its own boundary, announced in its Content-Type.
    let resp_boundary = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split("boundary=").nth(1))
        .map(|b| b.trim_matches('"').to_string());
    let text = resp.text().await?;

    let metas = parse_batch_response(&text, resp_boundary.as_deref());
    // Any requested ID missing from the parsed results was dropped (a failed
    // sub-response, e.g. a per-message 429) and should be retried.
    let resolved: HashSet<&str> = metas.iter().map(|m| m.id.as_str()).collect();
    let failed: Vec<String> = ids
        .iter()
        .filter(|id| !resolved.contains(id.as_str()))
        .cloned()
        .collect();
    Ok((metas, failed))
}

/// Parse a `multipart/mixed` batch response, extracting one `MessageMeta` per
/// successful sub-response. Sub-responses that errored (no JSON message body)
/// are skipped.
fn parse_batch_response(text: &str, boundary: Option<&str>) -> Vec<MessageMeta> {
    let delimiter = match boundary {
        Some(b) => format!("--{b}"),
        // Fall back to splitting on any boundary-looking line.
        None => "--batch".to_string(),
    };

    let mut out = Vec::new();
    for part in text.split(delimiter.as_str()) {
        // Each part embeds an HTTP response; the JSON body is the only `{...}`.
        let (Some(start), Some(end)) = (part.find('{'), part.rfind('}')) else {
            continue;
        };
        if end < start {
            continue;
        }
        if let Ok(resp) = serde_json::from_str::<MessageResp>(&part[start..=end]) {
            out.push(meta_from_resp(resp));
        }
    }
    out
}

fn meta_from_resp(resp: MessageResp) -> MessageMeta {
    let headers = &resp.payload.headers;
    let (from_name, from_email) = header(headers, "From")
        .map(parse_from)
        .unwrap_or((None, String::new()));

    MessageMeta {
        id: resp.id,
        thread_id: resp.thread_id,
        from_name,
        from_email,
        subject: header(headers, "Subject")
            .unwrap_or("(no subject)")
            .to_string(),
        size_estimate: resp.size_estimate,
        internal_date: resp.internal_date.parse().unwrap_or(0),
        list_unsubscribe: header(headers, "List-Unsubscribe").map(str::to_string),
        list_unsubscribe_post: header(headers, "List-Unsubscribe-Post").map(str::to_string),
    }
}

fn header<'a>(headers: &'a [HeaderField], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Parse `"Display Name <addr@host>"` into `(name, email)`.
fn parse_from(raw: &str) -> (Option<String>, String) {
    if let Some(start) = raw.rfind('<') {
        if let Some(end_rel) = raw[start..].find('>') {
            let email = raw[start + 1..start + end_rel].trim().to_string();
            let name = raw[..start].trim().trim_matches('"').trim().to_string();
            let name = (!name.is_empty()).then_some(name);
            return (name, email);
        }
    }
    (None, raw.trim().to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResp {
    #[serde(default)]
    messages: Vec<MsgRef>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct MsgRef {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageResp {
    id: String,
    thread_id: String,
    #[serde(default)]
    size_estimate: u64,
    #[serde(default)]
    internal_date: String,
    #[serde(default)]
    payload: Payload,
}

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(default)]
    headers: Vec<HeaderField>,
}

#[derive(Deserialize)]
struct HeaderField {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileResp {
    email_address: String,
    #[serde(default)]
    messages_total: u64,
    #[serde(default)]
    threads_total: u64,
    #[serde(default)]
    history_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryResp {
    #[serde(default)]
    history: Vec<HistoryRecord>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    #[serde(default)]
    messages_added: Vec<MessageChanged>,
    #[serde(default)]
    messages_deleted: Vec<MessageChanged>,
    #[serde(default)]
    labels_added: Vec<LabelChanged>,
    #[serde(default)]
    labels_removed: Vec<LabelChanged>,
}

#[derive(Deserialize)]
struct MessageChanged {
    message: HistMessage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelChanged {
    message: HistMessage,
    #[serde(default)]
    label_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistMessage {
    id: String,
    #[serde(default)]
    label_ids: Vec<String>,
}

#[derive(Deserialize)]
struct FullMessageResp {
    #[serde(default)]
    payload: FullPart,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FullPart {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    body: PartBody,
    #[serde(default)]
    parts: Vec<FullPart>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PartBody {
    #[serde(default)]
    attachment_id: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct AttachmentResp {
    #[serde(default)]
    data: String,
}

#[derive(Deserialize)]
struct RawMessageResp {
    #[serde(default)]
    raw: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_from_with_display_name() {
        assert_eq!(
            parse_from("Acme News <news@acme.com>"),
            (Some("Acme News".to_string()), "news@acme.com".to_string())
        );
        assert_eq!(
            parse_from("\"Quoted, Name\" <x@y.com>"),
            (Some("Quoted, Name".to_string()), "x@y.com".to_string())
        );
        assert_eq!(parse_from("bare@host.com"), (None, "bare@host.com".to_string()));
    }

    #[test]
    fn parses_batch_response_skipping_errors() {
        // One successful sub-response and one 404 error sub-response.
        let body = "--batch_xyz\r\n\
            Content-Type: application/http\r\n\
            Content-ID: <response-1>\r\n\r\n\
            HTTP/1.1 200 OK\r\n\
            Content-Type: application/json; charset=UTF-8\r\n\r\n\
            {\"id\":\"m1\",\"threadId\":\"t1\",\"payload\":{\"headers\":[\
            {\"name\":\"From\",\"value\":\"Acme <news@acme.com>\"},\
            {\"name\":\"Subject\",\"value\":\"Hello\"},\
            {\"name\":\"List-Unsubscribe\",\"value\":\"<https://acme.com/u>\"},\
            {\"name\":\"List-Unsubscribe-Post\",\"value\":\"List-Unsubscribe=One-Click\"}]}}\r\n\
            --batch_xyz\r\n\
            Content-Type: application/http\r\n\
            Content-ID: <response-2>\r\n\r\n\
            HTTP/1.1 404 Not Found\r\n\
            Content-Type: application/json; charset=UTF-8\r\n\r\n\
            {\"error\":{\"code\":404,\"message\":\"Not Found\"}}\r\n\
            --batch_xyz--\r\n";

        let metas = parse_batch_response(body, Some("batch_xyz"));
        assert_eq!(metas.len(), 1);
        let m = &metas[0];
        assert_eq!(m.id, "m1");
        assert_eq!(m.from_email, "news@acme.com");
        assert_eq!(m.domain(), "acme.com");
        assert_eq!(m.subject, "Hello");
        assert!(m.list_unsubscribe.is_some());
        assert!(m
            .list_unsubscribe_post
            .as_deref()
            .unwrap()
            .contains("One-Click"));
    }
}
