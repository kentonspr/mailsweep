//! Provider-agnostic message metadata and sender grouping.

use std::collections::{BTreeMap, HashMap};

use crate::unsubscribe::{self, UnsubscribeInfo};

/// Lightweight metadata for one message (no body fetched).
#[derive(Debug, Clone)]
pub struct MessageMeta {
    pub id: String,
    pub thread_id: String,
    pub from_name: Option<String>,
    pub from_email: String,
    pub subject: String,
    /// Gmail's rough size estimate in bytes.
    pub size_estimate: u64,
    /// Internal received timestamp in epoch milliseconds (0 if unknown).
    pub internal_date: i64,
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
}

impl MessageMeta {
    /// The sender's domain (portion after `@`), or the whole address if absent.
    pub fn domain(&self) -> &str {
        self.from_email
            .rsplit('@')
            .next()
            .unwrap_or(&self.from_email)
    }
}

/// All messages from a single sending domain, plus an unsubscribe handle if any.
#[derive(Debug, Clone, Default)]
pub struct SenderGroup {
    pub domain: String,
    pub message_ids: Vec<String>,
    pub sample_subjects: Vec<String>,
    /// Distinct sender addresses within the domain and their message counts.
    pub senders: BTreeMap<String, usize>,
    pub unsubscribe: Option<UnsubscribeInfo>,
}

impl SenderGroup {
    pub fn count(&self) -> usize {
        self.message_ids.len()
    }
}

/// The readable content of a single message (for the message viewer).
#[derive(Debug, Clone, Default)]
pub struct MessageBody {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date_ms: i64,
    pub text: String,
}

/// Crudely convert HTML to readable plain text (strip tags, decode a few common
/// entities). Good enough for a terminal message viewer.
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One sender (email address) within a domain, holding its messages.
#[derive(Debug, Clone)]
pub struct SenderEntry {
    pub email: String,
    pub name: Option<String>,
    pub messages: Vec<MessageMeta>,
    pub unsubscribe: Option<UnsubscribeInfo>,
}

impl SenderEntry {
    pub fn count(&self) -> usize {
        self.messages.len()
    }

    pub fn size(&self) -> u64 {
        self.messages.iter().map(|m| m.size_estimate).sum()
    }

    pub fn message_ids(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.id.clone()).collect()
    }
}

/// All messages from one sending domain, broken down by individual sender.
#[derive(Debug, Clone)]
pub struct DomainGroup {
    pub domain: String,
    pub senders: Vec<SenderEntry>,
    pub unsubscribe: Option<UnsubscribeInfo>,
}

impl DomainGroup {
    pub fn count(&self) -> usize {
        self.senders.iter().map(SenderEntry::count).sum()
    }

    pub fn size(&self) -> u64 {
        self.senders.iter().map(SenderEntry::size).sum()
    }

    pub fn sender_count(&self) -> usize {
        self.senders.len()
    }

    pub fn message_ids(&self) -> Vec<String> {
        self.senders
            .iter()
            .flat_map(SenderEntry::message_ids)
            .collect()
    }
}

/// Group messages by domain, then by sender within each domain. Domains and
/// senders are sorted by message count descending; a sender's messages are
/// sorted newest-first. This is the tree the TUI renders (domain → sender →
/// message).
pub fn group_messages(messages: &[MessageMeta]) -> Vec<DomainGroup> {
    let mut domains: HashMap<String, HashMap<String, SenderEntry>> = HashMap::new();

    for m in messages {
        let senders = domains.entry(m.domain().to_string()).or_default();
        let entry = senders
            .entry(m.from_email.clone())
            .or_insert_with(|| SenderEntry {
                email: m.from_email.clone(),
                name: m.from_name.clone(),
                messages: Vec::new(),
                unsubscribe: None,
            });
        entry.messages.push(m.clone());
        if entry.unsubscribe.is_none() {
            entry.unsubscribe = unsubscribe::parse(
                m.list_unsubscribe.as_deref(),
                m.list_unsubscribe_post.as_deref(),
            );
        }
    }

    let mut groups: Vec<DomainGroup> = domains
        .into_iter()
        .map(|(domain, senders_map)| {
            let mut senders: Vec<SenderEntry> = senders_map.into_values().collect();
            for s in &mut senders {
                s.messages
                    .sort_by(|a, b| b.internal_date.cmp(&a.internal_date));
            }
            senders.sort_by(|a, b| b.count().cmp(&a.count()));
            let unsubscribe = senders.iter().find_map(|s| s.unsubscribe.clone());
            DomainGroup {
                domain,
                senders,
                unsubscribe,
            }
        })
        .collect();
    groups.sort_by(|a, b| b.count().cmp(&a.count()));
    groups
}

/// Bucket messages by sending domain, sorted by message count descending.
pub fn group_by_domain(messages: &[MessageMeta]) -> Vec<SenderGroup> {
    let mut map: HashMap<String, SenderGroup> = HashMap::new();

    for m in messages {
        let entry = map
            .entry(m.domain().to_string())
            .or_insert_with(|| SenderGroup {
                domain: m.domain().to_string(),
                ..Default::default()
            });

        entry.message_ids.push(m.id.clone());
        *entry.senders.entry(m.from_email.clone()).or_default() += 1;
        if entry.sample_subjects.len() < 5 {
            entry.sample_subjects.push(m.subject.clone());
        }
        if entry.unsubscribe.is_none() {
            entry.unsubscribe = unsubscribe::parse(
                m.list_unsubscribe.as_deref(),
                m.list_unsubscribe_post.as_deref(),
            );
        }
    }

    let mut groups: Vec<SenderGroup> = map.into_values().collect();
    groups.sort_by(|a, b| b.count().cmp(&a.count()));
    groups
}
