//! Asynchronous, non-blocking persistence actor for Session I/O.
//!
//! Inspired by `grok-build`'s `SessionActor` and `PersistenceMsg` pipeline,
//! this decouples disk operations (JSONL appending, Sidecar atomic writes, Summary updates)
//! from the interactive TUI / agent event loop.

use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::entries::SessionEntry;
use crate::error::{Result, SessionError};
use crate::presence::{Activity, SessionLock};
use crate::sidecars::{write_sidecar_json_async, SidecarKind};
use crate::summary::{write_summary_file, SessionSummary};

/// Message sent across the persistence channel.
#[derive(Debug)]
pub enum PersistenceMsg {
    /// Append a single entry to session JSONL.
    AppendEntry(SessionEntry),
    /// Append multiple entries to session JSONL.
    AppendEntries(Vec<SessionEntry>),
    /// Update or refresh the session summary sidecar.
    UpdateSummary(SessionSummary),
    /// Write a structured sidecar file (Todo, Plan, Hunks, etc.).
    SaveSidecar { kind: SidecarKind, content: Value },
    /// Update active session presence/activity in lockfile.
    UpdateActivity(Activity),
    /// Request an immediate sync/flush to disk and wait for completion.
    Flush(oneshot::Sender<Result<()>>),
    /// Clean shutdown of the persistence actor.
    Shutdown,
}

/// Actor handle held by callers (TUI / Agent runtime / CLI).
#[derive(Clone)]
pub struct SessionActorHandle {
    tx: mpsc::Sender<PersistenceMsg>,
    session_id: String,
    file_path: PathBuf,
}

impl SessionActorHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Non-blocking fire-and-forget append of a session entry.
    pub async fn append_entry(&self, entry: SessionEntry) -> Result<()> {
        self.tx
            .send(PersistenceMsg::AppendEntry(entry))
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("actor send failed: {e}"))))
    }

    /// Non-blocking fire-and-forget append of multiple entries.
    pub async fn append_entries(&self, entries: Vec<SessionEntry>) -> Result<()> {
        self.tx
            .send(PersistenceMsg::AppendEntries(entries))
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("actor send failed: {e}"))))
    }

    /// Non-blocking sidecar write.
    pub async fn save_sidecar(&self, kind: SidecarKind, content: Value) -> Result<()> {
        self.tx
            .send(PersistenceMsg::SaveSidecar { kind, content })
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("actor send failed: {e}"))))
    }

    /// Update summary sidecar.
    pub async fn update_summary(&self, summary: SessionSummary) -> Result<()> {
        self.tx
            .send(PersistenceMsg::UpdateSummary(summary))
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("actor send failed: {e}"))))
    }

    /// Update activity state (e.g. Idle vs Working).
    pub async fn set_activity(&self, activity: Activity) -> Result<()> {
        self.tx
            .send(PersistenceMsg::UpdateActivity(activity))
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("actor send failed: {e}"))))
    }

    /// Flush all pending writes and wait for completion.
    pub async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(PersistenceMsg::Flush(tx)).await.map_err(|e| {
            SessionError::Io(std::io::Error::other(format!("actor send failed: {e}")))
        })?;
        rx.await
            .map_err(|e| SessionError::Io(std::io::Error::other(format!("flush rx failed: {e}"))))?
    }

    /// Shutdown the persistence worker.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(PersistenceMsg::Shutdown).await;
    }
}

/// The persistence worker running in a dedicated Tokio task.
pub struct SessionActor {
    rx: mpsc::Receiver<PersistenceMsg>,
    file_path: PathBuf,
    _session_id: String,
    lock: Option<SessionLock>,
}

impl SessionActor {
    /// Spawn a new background persistence actor for a session.
    pub fn spawn(file_path: PathBuf, session_id: String, buffer_size: usize) -> SessionActorHandle {
        let (tx, rx) = mpsc::channel(buffer_size.max(32));
        let handle = SessionActorHandle {
            tx,
            session_id: session_id.clone(),
            file_path: file_path.clone(),
        };

        let lock = SessionLock::acquire(&file_path, &session_id).ok();

        let actor = SessionActor {
            rx,
            file_path,
            _session_id: session_id,
            lock,
        };

        tokio::spawn(actor.run());
        handle
    }

    async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                PersistenceMsg::AppendEntry(entry) => {
                    let _ = self.write_entry(&entry).await;
                }
                PersistenceMsg::AppendEntries(entries) => {
                    let _ = self.write_entries(&entries).await;
                }
                PersistenceMsg::UpdateSummary(summary) => {
                    let fp = self.file_path.clone();
                    let _ = tokio::task::spawn_blocking(move || write_summary_file(&fp, &summary))
                        .await;
                }
                PersistenceMsg::SaveSidecar { kind, content } => {
                    let _ = write_sidecar_json_async(&self.file_path, kind, content).await;
                }
                PersistenceMsg::UpdateActivity(activity) => {
                    if let Some(lock) = &mut self.lock {
                        let _ = lock.update_activity(activity);
                    }
                }
                PersistenceMsg::Flush(reply_tx) => {
                    let res = self.flush_file().await;
                    let _ = reply_tx.send(res);
                }
                PersistenceMsg::Shutdown => {
                    let _ = self.flush_file().await;
                    break;
                }
            }
        }
        if let Some(lock) = self.lock.take() {
            lock.release();
        }
    }

    async fn write_entry(&self, entry: &SessionEntry) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .await?;
        let json = serde_json::to_string(entry)?;
        file.write_all(json.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        Ok(())
    }

    async fn write_entries(&self, entries: &[SessionEntry]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .await?;
        let mut buf = Vec::new();
        for entry in entries {
            let json = serde_json::to_string(entry)?;
            buf.extend_from_slice(json.as_bytes());
            buf.push(b'\n');
        }
        file.write_all(&buf).await?;
        file.flush().await?;
        Ok(())
    }

    async fn flush_file(&self) -> Result<()> {
        if self.file_path.exists() {
            let file = OpenOptions::new().write(true).open(&self.file_path).await?;
            file.sync_all().await?;
        }
        Ok(())
    }
}
