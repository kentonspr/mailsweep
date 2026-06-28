//! Download attachments and pack them into a navigable zip archive.
//!
//! Layout inside the zip:
//! ```text
//! <domain>/<sender>/<message-id>__<filename>
//! manifest.json   # metadata for every archived message + attachment
//! ```

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::CompressionMethod;

use crate::gmail::GmailClient;

/// One message whose attachments should be archived.
#[derive(Debug, Clone)]
pub struct ArchiveItem {
    pub message_id: String,
    pub domain: String,
    pub sender: String,
    pub subject: String,
    pub date_ms: i64,
}

/// Result of an archive run.
#[derive(Debug, Clone)]
pub struct ArchiveSummary {
    pub path: PathBuf,
    pub messages: usize,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Serialize)]
struct ManifestEntry {
    domain: String,
    sender: String,
    message_id: String,
    subject: String,
    date_ms: i64,
    attachments: Vec<ManifestAttachment>,
}

#[derive(Serialize)]
struct ManifestAttachment {
    filename: String,
    mime_type: String,
    size: u64,
}

/// Fetch and zip the attachments for `items`, writing to `out_path`.
///
/// Messages without attachments are skipped; individual download failures are
/// skipped rather than aborting the whole archive.
pub async fn archive_attachments(
    client: &GmailClient,
    items: &[ArchiveItem],
    out_path: &Path,
) -> Result<ArchiveSummary> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = File::create(out_path)
        .with_context(|| format!("creating archive {}", out_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut manifest = Vec::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut files = 0;
    let mut bytes = 0u64;
    let mut messages = 0;

    for item in items {
        let attachments = client
            .message_attachments(&item.message_id)
            .await
            .unwrap_or_default();
        if attachments.is_empty() {
            continue;
        }
        messages += 1;

        let mut entry_attachments = Vec::new();
        for att in &attachments {
            let data = match client
                .download_attachment(&item.message_id, &att.attachment_id)
                .await
            {
                Ok(d) => d,
                Err(_) => continue,
            };

            let dir = format!("{}/{}", sanitize(&item.domain), sanitize(&item.sender));
            let mut name = format!("{dir}/{}__{}", item.message_id, sanitize(&att.filename));
            while !used_names.insert(name.clone()) {
                name.push('_');
            }

            zip.start_file(&name, options)?;
            zip.write_all(&data)?;
            files += 1;
            bytes += data.len() as u64;

            entry_attachments.push(ManifestAttachment {
                filename: att.filename.clone(),
                mime_type: att.mime_type.clone(),
                size: att.size,
            });
        }

        manifest.push(ManifestEntry {
            domain: item.domain.clone(),
            sender: item.sender.clone(),
            message_id: item.message_id.clone(),
            subject: item.subject.clone(),
            date_ms: item.date_ms,
            attachments: entry_attachments,
        });
    }

    let json = serde_json::to_string_pretty(&manifest)?;
    zip.start_file("manifest.json", options)?;
    zip.write_all(json.as_bytes())?;
    zip.finish()?;

    Ok(ArchiveSummary {
        path: out_path.to_path_buf(),
        messages,
        files,
        bytes,
    })
}

/// Make a string safe to use as a path component inside the archive.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}
