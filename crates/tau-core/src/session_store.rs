//! Append-only on-disk persistence of [`SessionTree`]s and per-session
//! protocol-event sidecars.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{ConnectionId, Event, LogEventId, SessionId};

use crate::session::{
    NodeId, PersistedSessionEvent, PersistedSessionRecord, SessionEntry, SessionMeta, SessionNode,
    SessionTree, ToolActivityRecord,
};

/// Errors returned by the append-only session store.
#[derive(Debug)]
pub enum SessionStoreError {
    CreateParentDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: tau_proto::DecodeError,
    },
    Encode {
        path: PathBuf,
        source: tau_proto::EncodeError,
    },
    /// Another process holds the exclusive lock on this session.
    Locked {
        path: PathBuf,
        holder: String,
    },
    InvalidSessionDir {
        path: PathBuf,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create parent directory for session store {}: {source}",
                path.display()
            ),
            Self::Open { path, source } => {
                write!(
                    f,
                    "failed to open session store {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    f,
                    "failed to read session store {}: {source}",
                    path.display()
                )
            }
            Self::Write { path, source } => {
                write!(
                    f,
                    "failed to write session store {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => write!(
                f,
                "failed to decode session store record from {}: {source}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode session store record for {}: {source}",
                path.display()
            ),
            Self::Locked { path, holder } => write!(
                f,
                "session lock at {} held by another process ({})",
                path.display(),
                holder.trim()
            ),
            Self::InvalidSessionDir { path } => write!(
                f,
                "invalid session directory name (non-utf8): {}",
                path.display()
            ),
        }
    }
}

impl Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::Open { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
            Self::Locked { .. } => None,
            Self::InvalidSessionDir { .. } => None,
        }
    }
}

/// Append-only persistence for tree-structured session history.
///
/// Each session lives in its own directory under `state_dir`:
///
/// ```text
/// <state_dir>/<session_id>/
///   log.cbor      # length-prefixed PersistedSessionRecord stream
///   meta.json     # SessionMeta sidecar (cwd, created_at, last_touched)
///   lock          # exclusively flock'd while this store has the session loaded for write
/// ```
///
/// Existing session dirs are eagerly loaded into memory at `open()`; their
/// flocks are taken lazily on first write so read-only consumers (e.g.
/// inspection commands) don't contend with a running daemon.
#[derive(Debug)]
pub struct SessionStore {
    state_dir: PathBuf,
    sessions: HashMap<SessionId, SessionTree>,
    /// Held flocks per session, acquired lazily on first write. Released when
    /// this store is dropped (the OS releases the flock when the file
    /// handle closes).
    locks: HashMap<SessionId, File>,
}

impl SessionStore {
    /// Opens the session store rooted at `state_dir`, eagerly loading every
    /// session subdirectory found there.
    pub fn open(state_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let state_dir = state_dir.into();
        fs::create_dir_all(&state_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: state_dir.clone(),
                source,
            }
        })?;

        let mut sessions = HashMap::new();
        for entry in fs::read_dir(&state_dir).map_err(|source| SessionStoreError::Read {
            path: state_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionStoreError::Read {
                path: state_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let log_path = path.join("log.cbor");
            if !log_path.exists() {
                continue;
            }
            let session_id_str = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| SessionStoreError::InvalidSessionDir { path: path.clone() })?;
            let sid: SessionId = session_id_str.into();
            let tree = load_session_log(&log_path, &sid)?;
            sessions.insert(sid, tree);
        }

        Ok(Self {
            state_dir,
            sessions,
            locks: HashMap::new(),
        })
    }

    /// Returns the path to one session's directory (created lazily on write).
    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.state_dir.join(session_id)
    }

    /// Acquires an exclusive flock on the session's `lock` file if not already
    /// held.
    fn ensure_locked(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        let sid: SessionId = session_id.into();
        if self.locks.contains_key(&sid) {
            return Ok(());
        }
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        let lock_path = session_dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| SessionStoreError::Open {
                path: lock_path.clone(),
                source,
            })?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            return Err(SessionStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
        // Replace lock contents with our PID + start time.
        file.set_len(0).map_err(|source| SessionStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            })?;
        let pid = std::process::id();
        let now = unix_now();
        writeln!(&mut file, "pid={pid} start={now}").map_err(|source| {
            SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            }
        })?;
        self.locks.insert(sid, file);
        Ok(())
    }

    /// Appends an entry at the current head, returns the new node ID.
    pub fn append(
        &mut self,
        session_id: &str,
        entry: SessionEntry,
    ) -> Result<NodeId, SessionStoreError> {
        self.ensure_locked(session_id)?;
        let sid: SessionId = session_id.into();
        let tree = self
            .sessions
            .entry(sid.clone())
            .or_insert_with(|| SessionTree {
                session_id: sid.clone(),
                nodes: Vec::new(),
                head: None,
            });
        let parent_id = tree.head;
        let id = tree.append_node(entry.clone());
        let record = PersistedSessionRecord::Node {
            id,
            parent_id,
            entry,
        };
        let session_dir = self.session_dir(session_id);
        append_record(&session_dir.join("log.cbor"), &record)?;
        touch_meta(&session_dir.join("meta.json"))?;
        Ok(id)
    }

    /// Moves the head pointer to an existing node (branch switch).
    pub fn set_head(&mut self, session_id: &str, node_id: NodeId) -> Result<(), SessionStoreError> {
        self.ensure_locked(session_id)?;
        let session_dir = self.session_dir(session_id);
        let tree = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionStoreError::Open {
                path: session_dir.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "session not found"),
            })?;
        tree.head = Some(node_id);
        let record = PersistedSessionRecord::SetHead { node_id };
        append_record(&session_dir.join("log.cbor"), &record)?;
        touch_meta(&session_dir.join("meta.json"))
    }

    /// Appends one user message to a session.
    pub fn append_user_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(
            &session_id.into(),
            SessionEntry::UserMessage { text: text.into() },
        )
    }

    /// Appends one agent message to a session.
    pub fn append_agent_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<NodeId, SessionStoreError> {
        self.append_agent_message_with_thinking(session_id, text, None)
    }

    /// Appends one agent message to a session with an optional
    /// reasoning summary captured during the turn.
    pub fn append_agent_message_with_thinking(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
        thinking: Option<String>,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(
            &session_id.into(),
            SessionEntry::AgentMessage {
                text: text.into(),
                thinking,
            },
        )
    }

    /// Appends one tool activity record to a session.
    pub fn append_tool_activity(
        &mut self,
        session_id: impl Into<String>,
        activity: ToolActivityRecord,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(&session_id.into(), SessionEntry::ToolActivity(activity))
    }

    /// Appends one non-transient protocol event to the durable per-session
    /// event log.
    pub fn append_session_event(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
        event: Event,
    ) -> Result<LogEventId, SessionStoreError> {
        self.ensure_locked(session_id)?;
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        let events_path = session_dir.join("events.cbor");
        let next_id = next_session_event_id(&events_path)?;
        let record = PersistedSessionEvent {
            id: next_id,
            source,
            event,
        };
        append_cbor_record(&events_path, &record)?;
        touch_meta(&session_dir.join("meta.json"))?;
        Ok(next_id)
    }

    /// Loads durable per-session protocol events.
    pub fn session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        let path = self.session_dir(session_id).join("events.cbor");
        load_session_events(&path)
    }

    /// Returns the state dir this store is rooted at.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns one session tree if it exists.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionTree> {
        self.sessions.get(session_id)
    }

    /// Returns all known sessions.
    #[must_use]
    pub fn sessions(&self) -> Vec<&SessionTree> {
        self.sessions.values().collect()
    }

    /// Records initial cwd metadata for a session if not already present.
    /// Idempotent: subsequent calls only update `last_touched` via
    /// [`touch_meta`].
    pub fn record_session_meta(
        &mut self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<(), SessionStoreError> {
        self.ensure_locked(session_id)?;
        let path = self.session_dir(session_id).join("meta.json");
        let now = unix_now();
        let mut meta = read_meta(&path).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        if meta.cwd.is_none() {
            meta.cwd = cwd;
        }
        meta.last_touched = now;
        write_meta(&path, &meta)
    }
}

/// Lists session metadata across `state_dir` without taking any flocks.
///
/// Sessions whose `meta.json` is missing or malformed are skipped silently;
/// the goal is best-effort discovery for `-r` resumption, not strict listing.
pub fn list_session_metas(state_dir: &Path) -> io::Result<Vec<(SessionId, SessionMeta)>> {
    let mut out = Vec::new();
    if !state_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let meta_path = path.join("meta.json");
        let Ok(meta) = read_meta(&meta_path) else {
            continue;
        };
        out.push((SessionId::from(name), meta));
    }
    Ok(out)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_meta(path: &Path) -> io::Result<SessionMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_meta(path: &Path, meta: &SessionMeta) -> Result<(), SessionStoreError> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SessionStoreError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    fs::write(path, bytes).map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Updates `last_touched` on the session's meta sidecar (creating it with
/// `created_at = now` if absent).
fn touch_meta(path: &Path) -> Result<(), SessionStoreError> {
    let now = unix_now();
    let mut meta = read_meta(path).unwrap_or_default();
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    meta.last_touched = now;
    write_meta(path, &meta)
}

fn load_session_log(
    log_path: &Path,
    session_id: &SessionId,
) -> Result<SessionTree, SessionStoreError> {
    let mut tree = SessionTree {
        session_id: session_id.clone(),
        nodes: Vec::new(),
        head: None,
    };
    let mut file = File::open(log_path).map_err(|source| SessionStoreError::Open {
        path: log_path.to_path_buf(),
        source,
    })?;
    loop {
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(tree),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: log_path.to_path_buf(),
                    source,
                });
            }
        }

        let record_length = u64::from_le_bytes(length_bytes) as usize;
        let mut record_bytes = vec![0_u8; record_length];
        file.read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: log_path.to_path_buf(),
                source,
            })?;

        let record: PersistedSessionRecord = ciborium::from_reader(record_bytes.as_slice())
            .map_err(|source| SessionStoreError::Decode {
                path: log_path.to_path_buf(),
                source,
            })?;

        match record {
            PersistedSessionRecord::Node {
                id,
                parent_id,
                entry,
            } => {
                debug_assert!(id.0 == tree.nodes.len() as u64);
                tree.nodes.push(SessionNode {
                    id,
                    parent_id,
                    entry,
                });
                tree.head = Some(id);
            }
            PersistedSessionRecord::SetHead { node_id } => {
                tree.head = Some(node_id);
            }
        }
    }
}

fn append_record(path: &Path, record: &PersistedSessionRecord) -> Result<(), SessionStoreError> {
    append_cbor_record(path, record)
}

fn append_cbor_record<T: Serialize>(path: &Path, record: &T) -> Result<(), SessionStoreError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| SessionStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SessionStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn load_session_events(path: &Path) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    read_cbor_records(path, |record: PersistedSessionEvent| {
        events.push(record);
    })?;
    Ok(events)
}

fn next_session_event_id(path: &Path) -> Result<LogEventId, SessionStoreError> {
    let events = load_session_events(path)?;
    Ok(events
        .last()
        .map(|record| LogEventId::new(record.id.get() + 1))
        .unwrap_or_else(|| LogEventId::new(0)))
}

fn read_cbor_records<T, F>(path: &Path, mut handle: F) -> Result<(), SessionStoreError>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T),
{
    let mut file = File::open(path).map_err(|source| SessionStoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    loop {
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        let record_length = u64::from_le_bytes(length_bytes) as usize;
        let mut record_bytes = vec![0_u8; record_length];
        file.read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let record: T = ciborium::from_reader(record_bytes.as_slice()).map_err(|source| {
            SessionStoreError::Decode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        handle(record);
    }
}
