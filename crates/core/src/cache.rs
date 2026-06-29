//! On-disk SQLite cache of fetched message metadata.
//!
//! Keyed by Gmail message ID. A rescan only fetches IDs not already cached, so
//! re-running against a large mailbox is cheap. Trashed/spammed messages are
//! evicted so the cache stays consistent with the live inbox.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::gmail::AttachmentInfo;
use crate::model::MessageMeta;

/// Bump whenever the cached row shape changes. On mismatch the cache is reset
/// (it is just a fetch cache, so discarding and refetching is always safe) —
/// this avoids silently serving rows missing newly-added fields.
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub struct Cache {
    conn: Arc<Mutex<Connection>>,
}

impl Cache {
    /// Open (creating if needed) the cache database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).context("opening metadata cache")?;

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version != SCHEMA_VERSION {
            // Drop everything together: a stale history checkpoint with an empty
            // message table would yield a near-empty incremental sync.
            conn.execute_batch(
                "DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS state;
                 DROP TABLE IF EXISTS attachments;
                 DROP TABLE IF EXISTS attachment_state;",
            )
            .context("resetting outdated cache")?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id                    TEXT PRIMARY KEY,
                thread_id             TEXT NOT NULL,
                from_name             TEXT,
                from_email            TEXT NOT NULL,
                subject               TEXT NOT NULL,
                size_estimate         INTEGER NOT NULL DEFAULT 0,
                internal_date         INTEGER NOT NULL DEFAULT 0,
                list_unsubscribe      TEXT,
                list_unsubscribe_post TEXT
            );
            CREATE TABLE IF NOT EXISTS state (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS attachments (
                message_id    TEXT NOT NULL,
                attachment_id TEXT NOT NULL,
                filename      TEXT NOT NULL,
                mime_type     TEXT NOT NULL,
                size          INTEGER NOT NULL,
                PRIMARY KEY (message_id, attachment_id)
            );
            CREATE TABLE IF NOT EXISTS attachment_state (
                message_id TEXT PRIMARY KEY
            );",
        )
        .context("initializing cache schema")?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .ok();

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Load every cached message (used as the base set for incremental sync).
    pub async fn all(&self) -> Result<Vec<MessageMeta>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MessageMeta>> {
            let conn = conn.lock().expect("cache mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT id, thread_id, from_name, from_email, subject,
                        size_estimate, internal_date,
                        list_unsubscribe, list_unsubscribe_post
                 FROM messages",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(MessageMeta {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    from_name: r.get(2)?,
                    from_email: r.get(3)?,
                    subject: r.get(4)?,
                    size_estimate: r.get(5)?,
                    internal_date: r.get(6)?,
                    list_unsubscribe: r.get(7)?,
                    list_unsubscribe_post: r.get(8)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    /// Read a value from the key/value state table.
    pub async fn get_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let conn = conn.lock().expect("cache mutex poisoned");
            match conn.query_row(
                "SELECT value FROM state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            ) {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    /// Write a value to the key/value state table.
    pub async fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.clone();
        let key = key.to_string();
        let value = value.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().expect("cache mutex poisoned");
            conn.execute(
                "INSERT OR REPLACE INTO state (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
        .await?
    }

    /// Look up cached metadata for the given IDs (missing IDs are simply absent).
    pub async fn get_many(&self, ids: &[String]) -> Result<HashMap<String, MessageMeta>> {
        let conn = self.conn.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || -> Result<HashMap<String, MessageMeta>> {
            let conn = conn.lock().expect("cache mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT id, thread_id, from_name, from_email, subject,
                        size_estimate, internal_date,
                        list_unsubscribe, list_unsubscribe_post
                 FROM messages WHERE id = ?1",
            )?;
            let mut out = HashMap::new();
            for id in &ids {
                let row = stmt.query_row(params![id], |r| {
                    Ok(MessageMeta {
                        id: r.get(0)?,
                        thread_id: r.get(1)?,
                        from_name: r.get(2)?,
                        from_email: r.get(3)?,
                        subject: r.get(4)?,
                        size_estimate: r.get(5)?,
                        internal_date: r.get(6)?,
                        list_unsubscribe: r.get(7)?,
                        list_unsubscribe_post: r.get(8)?,
                    })
                });
                match row {
                    Ok(m) => {
                        out.insert(m.id.clone(), m);
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(out)
        })
        .await?
    }

    /// Insert or replace metadata for many messages in a single transaction.
    pub async fn put_many(&self, metas: &[MessageMeta]) -> Result<()> {
        let conn = self.conn.clone();
        let metas = metas.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = conn.lock().expect("cache mutex poisoned");
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR REPLACE INTO messages
                        (id, thread_id, from_name, from_email, subject,
                         size_estimate, internal_date,
                         list_unsubscribe, list_unsubscribe_post)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for m in &metas {
                    stmt.execute(params![
                        m.id,
                        m.thread_id,
                        m.from_name,
                        m.from_email,
                        m.subject,
                        m.size_estimate,
                        m.internal_date,
                        m.list_unsubscribe,
                        m.list_unsubscribe_post,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    /// Remove messages from the cache (after trashing or marking as spam).
    pub async fn remove(&self, ids: &[String]) -> Result<()> {
        let conn = self.conn.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = conn.lock().expect("cache mutex poisoned");
            let tx = conn.transaction()?;
            {
                let mut msg = tx.prepare("DELETE FROM messages WHERE id = ?1")?;
                let mut att = tx.prepare("DELETE FROM attachments WHERE message_id = ?1")?;
                let mut state = tx.prepare("DELETE FROM attachment_state WHERE message_id = ?1")?;
                for id in &ids {
                    msg.execute(params![id])?;
                    att.execute(params![id])?;
                    state.execute(params![id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    /// Load cached attachment details for any of `ids` already resolved.
    /// Resolved messages appear in the map (possibly with an empty list).
    pub async fn get_attachments_many(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<AttachmentInfo>>> {
        let conn = self.conn.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || -> Result<HashMap<String, Vec<AttachmentInfo>>> {
            let conn = conn.lock().expect("cache mutex poisoned");
            let mut resolved =
                conn.prepare("SELECT 1 FROM attachment_state WHERE message_id = ?1")?;
            let mut details = conn.prepare(
                "SELECT attachment_id, filename, mime_type, size
                 FROM attachments WHERE message_id = ?1",
            )?;
            let mut out = HashMap::new();
            for id in &ids {
                if !resolved.exists(params![id])? {
                    continue;
                }
                let rows = details.query_map(params![id], |r| {
                    Ok(AttachmentInfo {
                        attachment_id: r.get(0)?,
                        filename: r.get(1)?,
                        mime_type: r.get(2)?,
                        size: r.get(3)?,
                    })
                })?;
                let mut list = Vec::new();
                for row in rows {
                    list.push(row?);
                }
                out.insert(id.clone(), list);
            }
            Ok(out)
        })
        .await?
    }

    /// Store a message's attachment details and mark it resolved.
    pub async fn put_attachments(&self, message_id: &str, atts: &[AttachmentInfo]) -> Result<()> {
        let conn = self.conn.clone();
        let message_id = message_id.to_string();
        let atts = atts.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = conn.lock().expect("cache mutex poisoned");
            let tx = conn.transaction()?;
            {
                tx.execute(
                    "INSERT OR REPLACE INTO attachment_state (message_id) VALUES (?1)",
                    params![message_id],
                )?;
                tx.execute(
                    "DELETE FROM attachments WHERE message_id = ?1",
                    params![message_id],
                )?;
                let mut stmt = tx.prepare(
                    "INSERT OR REPLACE INTO attachments
                        (message_id, attachment_id, filename, mime_type, size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for a in &atts {
                    stmt.execute(params![
                        message_id,
                        a.attachment_id,
                        a.filename,
                        a.mime_type,
                        a.size
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }
}
