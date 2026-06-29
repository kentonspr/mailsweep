//! Outlook / Hotmail (consumer) provider via Microsoft Graph.
//!
//! Auth is the OAuth 2.0 device-code flow against the `consumers` tenant, using
//! the user's own Azure "public client" app id (no client secret). Tokens are
//! cached per account like the Gmail provider.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::auth::AuthPrompt;
use crate::cache::Cache;
use crate::gmail::{AttachmentInfo, FetchProgress, FetchReport, Profile};
use crate::model::{strip_html, MessageBody, MessageMeta};
use crate::provider::{MailProvider, ProgressCallback, SyncResult};
use crate::unsubscribe::UnsubscribeInfo;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const AUTH: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";
/// Graph `$batch` accepts at most 20 sub-requests.
const BATCH_LIMIT: usize = 20;
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

// ---- auth -------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct StoredTokens {
    refresh_token: String,
    #[serde(default)]
    access_token: String,
}

/// Device-code authenticator with a disk-cached refresh token.
pub struct MsAuth {
    client_id: String,
    token_path: PathBuf,
    http: Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl MsAuth {
    pub fn new(client_id: String, token_path: PathBuf) -> Self {
        Self {
            client_id,
            token_path,
            http: Client::new(),
            cached: Mutex::new(None),
        }
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut guard = self.cached.lock().await;
        if let Some((token, at)) = guard.as_ref() {
            if at.elapsed() < TOKEN_TTL {
                return Ok(token.clone());
            }
        }
        let token = self.refresh().await?;
        *guard = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    fn load(&self) -> StoredTokens {
        std::fs::read_to_string(&self.token_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn store(&self, tokens: &StoredTokens) {
        if let Some(parent) = self.token_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string(tokens) {
            std::fs::write(&self.token_path, json).ok();
        }
    }

    async fn refresh(&self) -> Result<String> {
        let stored = self.load();
        if stored.refresh_token.is_empty() {
            bail!("no saved Outlook credentials — add the account again");
        }
        let resp = self
            .http
            .post(format!("{AUTH}/token"))
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", stored.refresh_token.as_str()),
                ("scope", crate::config::MS_SCOPE),
            ])
            .send()
            .await?
            .error_for_status()
            .context("refreshing Outlook token")?
            .json::<TokenResp>()
            .await?;

        let refresh_token = resp.refresh_token.unwrap_or(stored.refresh_token);
        self.store(&StoredTokens {
            refresh_token,
            access_token: resp.access_token.clone(),
        });
        Ok(resp.access_token)
    }

    /// Run the interactive device-code flow, reporting the URL/code via
    /// `on_prompt` and polling until the user completes sign-in. Saves the
    /// refresh token.
    pub async fn device_login(&self, on_prompt: &(dyn Fn(AuthPrompt) + Send + Sync)) -> Result<()> {
        let code: DeviceCodeResp = self
            .http
            .post(format!("{AUTH}/devicecode"))
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", crate::config::MS_SCOPE),
            ])
            .send()
            .await?
            .error_for_status()
            .context("requesting device code")?
            .json()
            .await?;

        on_prompt(AuthPrompt::DeviceCode {
            verification_uri: code.verification_uri.clone(),
            user_code: code.user_code.clone(),
            message: code.message.clone(),
        });

        let deadline = Instant::now() + Duration::from_secs(code.expires_in.max(60));
        let mut interval = Duration::from_secs(code.interval.max(1));
        loop {
            if Instant::now() > deadline {
                bail!("device-code sign-in timed out");
            }
            sleep(interval).await;

            let resp = self
                .http
                .post(format!("{AUTH}/token"))
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", code.device_code.as_str()),
                ])
                .send()
                .await?;

            if resp.status().is_success() {
                let tok: TokenResp = resp.json().await?;
                self.store(&StoredTokens {
                    refresh_token: tok.refresh_token.unwrap_or_default(),
                    access_token: tok.access_token,
                });
                return Ok(());
            }

            let err: TokenError = resp.json().await.unwrap_or_default();
            match err.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += Duration::from_secs(5),
                other => bail!("sign-in failed: {} ({})", other, err.error_description),
            }
        }
    }
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct DeviceCodeResp {
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    message: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    interval: u64,
}

// ---- client -----------------------------------------------------------------

#[derive(Clone)]
pub struct OutlookClient {
    http: Client,
    auth: Arc<MsAuth>,
    cache: Option<Cache>,
}

impl OutlookClient {
    pub fn new(auth: Arc<MsAuth>) -> Self {
        Self {
            http: Client::new(),
            auth,
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.auth.access_token().await?))
    }

    /// Page a Graph collection of `{id}` objects following `@odata.nextLink`.
    async fn list_ids(&self, first_url: String, max: usize) -> Result<Vec<String>> {
        let bearer = self.bearer().await?;
        let mut ids = Vec::new();
        let mut url = Some(first_url);
        while let Some(u) = url {
            let resp: IdListResp = self
                .http
                .get(&u)
                .header("Authorization", &bearer)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ids.extend(resp.value.into_iter().map(|m| m.id));
            if ids.len() >= max {
                ids.truncate(max);
                break;
            }
            url = resp.next_link;
        }
        Ok(ids)
    }

    /// Run a Graph `$batch` of pre-built request objects, returning the
    /// `responses` array.
    async fn batch(&self, requests: Vec<serde_json::Value>) -> Result<Vec<BatchResponse>> {
        let bearer = self.bearer().await?;
        let body = serde_json::json!({ "requests": requests });
        let resp: BatchResp = self
            .http
            .post(format!("{GRAPH}/$batch"))
            .header("Authorization", &bearer)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.responses)
    }
}

#[async_trait]
impl MailProvider for OutlookClient {
    async fn profile(&self) -> Result<Profile> {
        let bearer = self.bearer().await?;
        let me: GraphMe = self
            .http
            .get(format!("{GRAPH}/me?$select=mail,userPrincipalName"))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let inbox: MailFolder = self
            .http
            .get(format!(
                "{GRAPH}/me/mailFolders/inbox?$select=totalItemCount"
            ))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Profile {
            email: me.mail.or(me.user_principal_name).unwrap_or_default(),
            messages_total: inbox.total_item_count,
            threads_total: 0,
            history_id: String::new(),
        })
    }

    async fn inbox_sync(&self, token: Option<&str>, max: usize) -> Result<SyncResult> {
        let full = !matches!(token, Some(t) if t.starts_with("http"));
        let first = match token {
            Some(t) if t.starts_with("http") => t.to_string(),
            _ => format!("{GRAPH}/me/mailFolders/inbox/messages/delta?$select=id"),
        };

        let bearer = self.bearer().await?;
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut delta_link = String::new();
        let mut url = Some(first);

        while let Some(u) = url {
            let resp = self
                .http
                .get(&u)
                .header("Authorization", &bearer)
                .send()
                .await?;
            if resp.status() == StatusCode::GONE {
                // Delta token expired — restart with a full snapshot.
                return self.inbox_sync(None, max).await;
            }
            let body: DeltaResp = resp.error_for_status()?.json().await?;
            for item in body.value {
                if item.removed.is_some() {
                    removed.push(item.id);
                } else {
                    added.push(item.id);
                }
            }
            if added.len() >= max {
                added.truncate(max);
                break;
            }
            match body.next_link {
                Some(next) => url = Some(next),
                None => {
                    delta_link = body.delta_link.unwrap_or_default();
                    url = None;
                }
            }
        }

        Ok(SyncResult {
            added,
            removed,
            next_token: delta_link,
            full,
        })
    }

    async fn list_attachment_ids(&self, max: usize) -> Result<Vec<String>> {
        self.list_ids(
            format!(
                "{GRAPH}/me/mailFolders/inbox/messages?$filter=hasAttachments eq true&$select=id&$top=100"
            ),
            max,
        )
        .await
    }

    async fn list_query_ids(&self, query: &str, max: usize) -> Result<Vec<String>> {
        // Graph $search is free-text (KQL); Gmail-style operators won't apply.
        let enc = query
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('"', "%22");
        self.list_ids(
            format!("{GRAPH}/me/messages?$search=%22{enc}%22&$select=id&$top=100"),
            max,
        )
        .await
    }

    fn query_help(&self) -> &'static str {
        "Outlook search (KQL) · from:amazon · subject:invoice · hasAttachments:true"
    }

    fn query_examples(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("From a sender", "from:amazon"),
            ("Subject contains", "subject:invoice"),
            ("Has an attachment", "hasAttachments:true"),
            ("Received before a date", "received<=2023-01-01"),
            ("Unread", "isRead:false"),
            ("Keyword anywhere", "newsletter"),
        ]
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

        let select = "id,subject,from,receivedDateTime,internetMessageHeaders";
        let mut fetched = 0;
        for chunk in missing.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "GET",
                        "url": format!("/me/messages/{id}?$select={select}"),
                    })
                })
                .collect();
            let responses = match self.batch(requests).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut batch_metas = Vec::new();
            for r in responses {
                if r.status / 100 != 2 {
                    continue;
                }
                if let Ok(msg) = serde_json::from_value::<GraphMessage>(r.body) {
                    batch_metas.push(meta_from_graph(msg));
                }
            }
            fetched += batch_metas.len();
            if let Some(c) = &self.cache {
                c.put_many(&batch_metas).await.ok();
            }
            for m in &batch_metas {
                known.insert(m.id.clone(), m.clone());
            }
            on_update(
                FetchProgress {
                    resolved: from_cache + fetched,
                    total,
                },
                &batch_metas,
            );
        }

        let metas = ids.iter().filter_map(|id| known.get(id).cloned()).collect();
        Ok(FetchReport {
            metas,
            requested: total,
            from_cache,
            fetched,
            batch_errors: Vec::new(),
        })
    }

    async fn trash(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "DELETE",
                        "url": format!("/me/messages/{id}"),
                    })
                })
                .collect();
            self.batch(requests).await?;
        }
        if let Some(cache) = &self.cache {
            cache.remove(ids).await.ok();
        }
        Ok(())
    }

    async fn mark_spam(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "POST",
                        "url": format!("/me/messages/{id}/move"),
                        "headers": { "Content-Type": "application/json" },
                        "body": { "destinationId": "junkemail" },
                    })
                })
                .collect();
            self.batch(requests).await?;
        }
        if let Some(cache) = &self.cache {
            cache.remove(ids).await.ok();
        }
        Ok(())
    }

    async fn mark_read(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "PATCH",
                        "url": format!("/me/messages/{id}"),
                        "headers": { "Content-Type": "application/json" },
                        "body": { "isRead": true },
                    })
                })
                .collect();
            self.batch(requests).await?;
        }
        Ok(())
    }

    async fn restore(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "POST",
                        "url": format!("/me/messages/{id}/move"),
                        "headers": { "Content-Type": "application/json" },
                        "body": { "destinationId": "inbox" },
                    })
                })
                .collect();
            self.batch(requests).await?;
        }
        Ok(())
    }

    async fn permanent_delete(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BATCH_LIMIT) {
            let requests: Vec<serde_json::Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    serde_json::json!({
                        "id": i.to_string(),
                        "method": "POST",
                        "url": format!("/me/messages/{id}/permanentDelete"),
                    })
                })
                .collect();
            self.batch(requests).await?;
        }
        if let Some(cache) = &self.cache {
            cache.remove(ids).await.ok();
        }
        Ok(())
    }

    async fn unsubscribe_one_click(&self, info: &UnsubscribeInfo) -> Result<bool> {
        crate::unsubscribe::one_click(&self.http, info).await
    }

    async fn message_attachments(&self, id: &str) -> Result<Vec<AttachmentInfo>> {
        let bearer = self.bearer().await?;
        let resp: AttachmentListResp = self
            .http
            .get(format!(
                "{GRAPH}/me/messages/{id}/attachments?$select=id,name,contentType,size"
            ))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp
            .value
            .into_iter()
            .map(|a| AttachmentInfo {
                filename: a.name.unwrap_or_default(),
                mime_type: a.content_type.unwrap_or_default(),
                size: a.size,
                attachment_id: a.id,
            })
            .collect())
    }

    async fn download_attachment(&self, message_id: &str, attachment_id: &str) -> Result<Vec<u8>> {
        let bearer = self.bearer().await?;
        let bytes = self
            .http
            .get(format!(
                "{GRAPH}/me/messages/{message_id}/attachments/{attachment_id}/$value"
            ))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    async fn download_raw_message(&self, message_id: &str) -> Result<Vec<u8>> {
        let bearer = self.bearer().await?;
        let bytes = self
            .http
            .get(format!("{GRAPH}/me/messages/{message_id}/$value"))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    async fn fetch_message_body(&self, message_id: &str) -> Result<MessageBody> {
        let bearer = self.bearer().await?;
        let m: GraphFullMessage = self
            .http
            .get(format!(
                "{GRAPH}/me/messages/{message_id}?$select=subject,from,toRecipients,receivedDateTime,body"
            ))
            .header("Authorization", &bearer)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let from = m
            .from
            .and_then(|r| r.email_address.address)
            .unwrap_or_default();
        let to = m
            .to_recipients
            .into_iter()
            .filter_map(|r| r.email_address.address)
            .collect::<Vec<_>>()
            .join(", ");
        let date_ms = m
            .received_date_time
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0);
        let text = match m.body {
            Some(b) if b.content_type.eq_ignore_ascii_case("html") => strip_html(&b.content),
            Some(b) => b.content,
            None => String::new(),
        };
        Ok(MessageBody {
            subject: m.subject.unwrap_or_default(),
            from,
            to,
            date_ms,
            text,
        })
    }
}

fn meta_from_graph(msg: GraphMessage) -> MessageMeta {
    let (from_name, from_email) = match msg.from.map(|f| f.email_address) {
        Some(addr) => (addr.name, addr.address.unwrap_or_default()),
        None => (None, String::new()),
    };
    let internal_date = msg
        .received_date_time
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    let header = |name: &str| {
        msg.internet_message_headers
            .as_ref()
            .and_then(|hs| hs.iter().find(|h| h.name.eq_ignore_ascii_case(name)))
            .map(|h| h.value.clone())
    };

    MessageMeta {
        id: msg.id,
        thread_id: msg.conversation_id.unwrap_or_default(),
        from_name,
        from_email,
        subject: msg.subject.unwrap_or_else(|| "(no subject)".to_string()),
        size_estimate: 0,
        internal_date,
        list_unsubscribe: header("List-Unsubscribe"),
        list_unsubscribe_post: header("List-Unsubscribe-Post"),
    }
}

// ---- Graph response shapes --------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMe {
    mail: Option<String>,
    user_principal_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailFolder {
    #[serde(default)]
    total_item_count: u64,
}

#[derive(Deserialize)]
struct IdListResp {
    #[serde(default)]
    value: Vec<IdOnly>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct IdOnly {
    id: String,
}

#[derive(Deserialize)]
struct DeltaResp {
    #[serde(default)]
    value: Vec<DeltaItem>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    delta_link: Option<String>,
}

#[derive(Deserialize)]
struct DeltaItem {
    id: String,
    #[serde(rename = "@removed", default)]
    removed: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct BatchResp {
    #[serde(default)]
    responses: Vec<BatchResponse>,
}

#[derive(Deserialize)]
struct BatchResponse {
    status: u16,
    #[serde(default)]
    body: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    id: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<Recipient>,
    #[serde(default)]
    received_date_time: Option<String>,
    #[serde(default)]
    internet_message_headers: Option<Vec<GraphHeader>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    email_address: EmailAddress,
}

#[derive(Deserialize)]
struct EmailAddress {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

#[derive(Deserialize)]
struct GraphHeader {
    name: String,
    value: String,
}

#[derive(Deserialize)]
struct AttachmentListResp {
    #[serde(default)]
    value: Vec<GraphAttachment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttachment {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFullMessage {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<Recipient>,
    #[serde(default)]
    to_recipients: Vec<Recipient>,
    #[serde(default)]
    received_date_time: Option<String>,
    #[serde(default)]
    body: Option<GraphBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphBody {
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    content: String,
}
