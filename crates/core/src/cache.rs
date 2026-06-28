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

use crate::model::MessageMeta;

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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id                    TEXT PRIMARY KEY,
                thread_id             TEXT NOT NULL,
                from_name             TEXT,
                from_email            TEXT NOT NULL,
                subject               TEXT NOT NULL,
                list_unsubscribe      TEXT,
                list_unsubscribe_post TEXT
            );",
        )
        .context("initializing cache schema")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Look up cached metadata for the given IDs (missing IDs are simply absent).
    pub async fn get_many(&self, ids: &[String]) -> Result<HashMap<String, MessageMeta>> {
        let conn = self.conn.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || -> Result<HashMap<String, MessageMeta>> {
            let conn = conn.lock().expect("cache mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT id, thread_id, from_name, from_email, subject,
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
                        list_unsubscribe: r.get(5)?,
                        list_unsubscribe_post: r.get(6)?,
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
                         list_unsubscribe, list_unsubscribe_post)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for m in &metas {
                    stmt.execute(params![
                        m.id,
                        m.thread_id,
                        m.from_name,
                        m.from_email,
                        m.subject,
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
                let mut stmt = tx.prepare("DELETE FROM messages WHERE id = ?1")?;
                for id in &ids {
                    stmt.execute(params![id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }
}
