//! Session presence state machine and process lock management.
//!
//! Aligns with `grok-build`'s `SessionPresence` (`Resident`, `Attaching`, `Evicted`,
//! `Closed`, `DeadFailed`, `Dormant`) and provides robust crash detection across processes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Real-time activity of an active session actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Idle,
    Working,
}

/// Liveness and presence state of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionPresence {
    /// In-memory resident session currently running in an active process.
    Resident { activity: Activity },
    /// Client is in the process of attaching / reconnecting.
    Attaching,
    /// Evicted from resident memory to disk by cache policy.
    Evicted,
    /// Explicitly closed/finalized cleanly by the user or exit command.
    Closed,
    /// Abnormally terminated or crashed.
    DeadFailed { error: String },
    /// Stored on disk and inactive (normal resting state between runs).
    Dormant,
}

impl SessionPresence {
    pub fn is_resident(&self) -> bool {
        matches!(self, SessionPresence::Resident { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SessionPresence::Resident { .. } | SessionPresence::Attaching
        )
    }

    pub fn is_crashed(&self) -> bool {
        matches!(self, SessionPresence::DeadFailed { .. })
    }

    pub fn display_label(&self) -> &'static str {
        match self {
            SessionPresence::Resident {
                activity: Activity::Working,
            } => "working",
            SessionPresence::Resident {
                activity: Activity::Idle,
            } => "resident",
            SessionPresence::Attaching => "attaching",
            SessionPresence::Evicted => "evicted",
            SessionPresence::Closed => "closed",
            SessionPresence::DeadFailed { .. } => "crashed",
            SessionPresence::Dormant => "dormant",
        }
    }
}

/// Lockfile representation for active session tracking (`<session_stem>.lock`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLockData {
    pub session_id: String,
    pub pid: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub hostname: String,
    pub activity: Activity,
}

pub fn lock_path_for(session_jsonl: &Path) -> PathBuf {
    let parent = session_jsonl.parent().unwrap_or_else(|| Path::new("."));
    let stem = session_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    parent.join(format!("{stem}.lock"))
}

/// Check if a process with the given PID is currently alive on the local OS.
pub fn is_process_alive(pid: u32) -> bool {
    // `kill -0` interprets negative values as process groups (and -1 as all
    // processes); never pass a u32 PID that would wrap to a negative pid_t.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // On Linux /proc/{pid} is the fastest, reliable non-signal check
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    if proc_path.exists() {
        return true;
    }
    #[cfg(unix)]
    {
        // Fallback for BSD / macOS or systems where /proc is not mounted
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) => s.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Try to acquire session lock. Returns `Ok(SessionLock)` on success.
pub struct SessionLock {
    lock_file: PathBuf,
    file: std::fs::File,
    pub data: SessionLockData,
}

impl SessionLock {
    pub fn acquire(session_jsonl: &Path, session_id: &str) -> std::io::Result<Self> {
        let lock_file = lock_path_for(session_jsonl);
        let current_pid = std::process::id();

        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        let data = SessionLockData {
            session_id: session_id.to_string(),
            pid: current_pid,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            hostname,
            activity: Activity::Idle,
        };

        let json = serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Hold an OS file lock for the session lifetime. The file remains in
        // place so contenders always lock the same inode; stale JSON is simply
        // overwritten after the previous process has exited.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_file)?;
        file.try_lock().map_err(|err| match err {
            std::fs::TryLockError::WouldBlock => std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session is locked by another writer",
            ),
            std::fs::TryLockError::Error(err) => err,
        })?;
        // A successful OS lock is authoritative. The old JSON may contain a
        // PID that has since been reused, so always replace it.
        use std::io::{Seek, Write};
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(json.as_bytes())?;
        file.sync_data()?;
        Ok(Self {
            lock_file,
            file,
            data,
        })
    }

    pub fn update_activity(&mut self, activity: Activity) -> std::io::Result<()> {
        if !self.owns_path() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session lock path changed",
            ));
        }
        self.data.activity = activity;
        self.data.updated_at = Utc::now();
        let json = serde_json::to_string_pretty(&self.data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        use std::io::{Seek, Write};
        self.file.set_len(0)?;
        self.file.rewind()?;
        self.file.write_all(json.as_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }

    fn owns_path(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return std::fs::metadata(&self.lock_file)
                .ok()
                .zip(self.file.metadata().ok())
                .is_some_and(|(current, owned)| {
                    current.dev() == owned.dev() && current.ino() == owned.ino()
                });
        }
        #[cfg(not(unix))]
        {
            self.lock_file.exists()
        }
    }

    pub fn release(self) {
        drop(self);
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if self.owns_path() {
            let _ = std::fs::remove_file(&self.lock_file);
        }
    }
}

/// Inspect the liveness/presence status of any session file on disk.
pub fn inspect_session_presence(session_jsonl: &Path) -> SessionPresence {
    let lock_file = lock_path_for(session_jsonl);
    if lock_file.exists() {
        if let Ok(raw) = std::fs::read_to_string(&lock_file) {
            if let Ok(data) = serde_json::from_str::<SessionLockData>(&raw) {
                if is_process_alive(data.pid) {
                    return SessionPresence::Resident {
                        activity: data.activity,
                    };
                } else {
                    return SessionPresence::DeadFailed {
                        error: format!("Process PID {} terminated unexpectedly", data.pid),
                    };
                }
            }
        }
    }
    SessionPresence::Dormant
}
