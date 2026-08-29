use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use kurama_protocol::{
    KuramaError,
    config::MutableState,
    id::{AgentId, SessionId},
    session::{
        BlobRef, EventEnvelope, SCHEMA_VERSION, SessionEvent, SessionMetadata, SessionSummary,
    },
    traits::SessionStore,
};
use sha2::{Digest, Sha256};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const METADATA_FILE: &str = "metadata.json";

#[derive(Debug, Clone)]
pub struct FsSessionStore {
    root: PathBuf,
}

impl FsSessionStore {
    pub fn open(root: PathBuf) -> Result<Self, KuramaError> {
        ensure_directory(&root)?;
        ensure_file(&root.join("config.toml"), b"")?;
        let initial_state = serde_json::to_vec(&MutableState::default())
            .map_err(|error| storage_error("serialize initial state", error))?;
        ensure_file(&root.join("state.json"), &initial_state)?;
        for directory in ["sessions", "blobs", "cache"] {
            ensure_directory(&root.join(directory))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn session_dir(&self, session_id: &SessionId) -> Result<PathBuf, KuramaError> {
        validate_identifier(session_id.as_ref(), "session")?;
        Ok(self.sessions_dir().join(session_id.as_ref()))
    }

    fn log_path(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
    ) -> Result<PathBuf, KuramaError> {
        let session_dir = self.session_dir(session_id)?;
        match agent_id {
            Some(agent_id) => {
                validate_identifier(agent_id.as_ref(), "agent")?;
                Ok(session_dir
                    .join("agents")
                    .join(format!("{}.jsonl", agent_id.as_ref())))
            }
            None => Ok(session_dir.join("events.jsonl")),
        }
    }

    fn replay_log(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
    ) -> Result<Vec<EventEnvelope>, KuramaError> {
        let path = self.log_path(session_id, agent_id)?;
        let mut file = open_locked(&path, false)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let complete_end = if bytes.is_empty() || bytes.last() == Some(&b'\n') {
            bytes.len()
        } else {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |position| position + 1)
        };
        let removed_bytes = bytes.len() - complete_end;
        if removed_bytes > 0 {
            file.set_len(complete_end as u64)?;
            file.sync_data()?;
            bytes.truncate(complete_end);
        }

        let events = deserialize_log(&bytes, session_id, agent_id)?;
        if removed_bytes > 0 {
            let repair = EventEnvelope::new(
                events.last().map_or(0, |event| event.sequence + 1),
                now_ms()?,
                session_id.clone(),
                agent_id.cloned(),
                SessionEvent::RecoveryRepair {
                    removed_bytes: removed_bytes as u64,
                },
            );
            file.seek(SeekFrom::End(0))?;
            serde_json::to_writer(&mut file, &repair)
                .map_err(|error| storage_error("serialize recovery repair", error))?;
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        Ok(events)
    }

    fn read_metadata(&self, session_dir: &Path) -> Result<SessionMetadata, KuramaError> {
        let path = session_dir.join(METADATA_FILE);
        reject_symlink(&path)?;
        let bytes = fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| storage_error("deserialize session metadata", error))
    }
}

impl SessionStore for FsSessionStore {
    fn create(&self, metadata: &SessionMetadata) -> Result<(), KuramaError> {
        validate_identifier(metadata.id.as_ref(), "session")?;
        let session_dir = self.session_dir(&metadata.id)?;
        create_new_directory(&session_dir)?;
        ensure_directory(&session_dir.join("agents"))?;
        ensure_file(&session_dir.join("events.jsonl"), b"")?;

        let encoded = serde_json::to_vec(metadata)
            .map_err(|error| storage_error("serialize session metadata", error))?;
        create_new_file(&session_dir.join(METADATA_FILE), &encoded)?;
        sync_directory(&session_dir)?;
        Ok(())
    }

    fn append(&self, event: &EventEnvelope) -> Result<(), KuramaError> {
        if event.schema_version != SCHEMA_VERSION {
            return Err(KuramaError::Storage(format!(
                "unsupported session schema version {}",
                event.schema_version
            )));
        }
        let session_dir = self.session_dir(&event.session_id)?;
        if !session_dir.join(METADATA_FILE).is_file() {
            return Err(KuramaError::NotFound(format!(
                "session {}",
                event.session_id
            )));
        }
        if event.agent_id.is_some() {
            ensure_directory(&session_dir.join("agents"))?;
        }
        let path = self.log_path(&event.session_id, event.agent_id.as_ref())?;
        let mut file = open_locked(&path, true)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(KuramaError::Storage(
                "incomplete trailing JSONL record; replay before append".into(),
            ));
        }
        let existing = deserialize_log(&bytes, &event.session_id, event.agent_id.as_ref())?;
        let expected = existing.last().map_or(0, |prior| prior.sequence + 1);
        if event.sequence != expected {
            return Err(KuramaError::Storage(format!(
                "invalid event sequence {}; expected {expected}",
                event.sequence
            )));
        }

        file.seek(SeekFrom::End(0))?;
        serde_json::to_writer(&mut file, event)
            .map_err(|error| storage_error("serialize session event", error))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }

    fn replay(&self, session_id: &SessionId) -> Result<Vec<EventEnvelope>, KuramaError> {
        self.replay_log(session_id, None)
    }

    fn replay_agent(
        &self,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<Vec<EventEnvelope>, KuramaError> {
        let session_dir = self.session_dir(session_id)?;
        if !session_dir.join(METADATA_FILE).is_file() {
            return Err(KuramaError::NotFound(format!("session {session_id}")));
        }
        if !self.log_path(session_id, Some(agent_id))?.exists() {
            return Ok(Vec::new());
        }
        self.replay_log(session_id, Some(agent_id))
    }

    fn list(&self) -> Result<Vec<SessionSummary>, KuramaError> {
        let mut summaries = Vec::new();
        for entry in fs::read_dir(self.sessions_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let metadata_path = entry.path().join(METADATA_FILE);
            if !metadata_path.is_file() {
                continue;
            }
            let metadata = self.read_metadata(&entry.path())?;
            if entry.file_name() != OsStr::new(metadata.id.as_ref()) {
                return Err(KuramaError::Storage(
                    "session metadata identifier does not match directory".into(),
                ));
            }

            let mut updated_at_ms = metadata.created_at_ms;
            for event in self.replay(&metadata.id)? {
                updated_at_ms = updated_at_ms.max(event.timestamp_ms);
            }
            let agents_dir = entry.path().join("agents");
            if agents_dir.is_dir() {
                for agent_entry in fs::read_dir(agents_dir)? {
                    let agent_entry = agent_entry?;
                    if !agent_entry.file_type()?.is_file()
                        || agent_entry.path().extension() != Some(OsStr::new("jsonl"))
                    {
                        continue;
                    }
                    let agent = agent_entry
                        .path()
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .ok_or_else(|| KuramaError::Storage("invalid child log file name".into()))?
                        .to_owned();
                    let agent_id = AgentId::from(agent);
                    for event in self.replay_agent(&metadata.id, &agent_id)? {
                        updated_at_ms = updated_at_ms.max(event.timestamp_ms);
                    }
                }
            }
            summaries.push(SessionSummary {
                id: metadata.id,
                created_at_ms: metadata.created_at_ms,
                updated_at_ms,
                project_root: metadata.project_root,
                profile: metadata.profile,
                mode: metadata.mode,
            });
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(summaries)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<BlobRef, KuramaError> {
        let digest = Sha256::digest(bytes);
        let sha256 = lowercase_hex(&digest);
        let blobs_dir = self.blobs_dir();
        let directory_lock = File::open(&blobs_dir)?;
        File::lock(&directory_lock)?;

        let final_path = blobs_dir.join(&sha256);
        if final_path.exists() {
            verify_blob(&final_path, &sha256, bytes.len() as u64)?;
            return Ok(BlobRef {
                sha256,
                bytes: bytes.len() as u64,
            });
        }

        let temporary_path = blobs_dir.join(format!(".{sha256}.tmp"));
        if temporary_path.exists() {
            reject_symlink(&temporary_path)?;
            fs::remove_file(&temporary_path)?;
        }
        create_new_file(&temporary_path, bytes)?;
        match fs::rename(&temporary_path, &final_path) {
            Ok(()) => {}
            Err(_) if final_path.exists() => {
                let _ = fs::remove_file(&temporary_path);
                verify_blob(&final_path, &sha256, bytes.len() as u64)?;
            }
            Err(error) => return Err(error.into()),
        }
        sync_directory(&blobs_dir)?;
        Ok(BlobRef {
            sha256,
            bytes: bytes.len() as u64,
        })
    }

    fn get_blob(&self, reference: &BlobRef) -> Result<Vec<u8>, KuramaError> {
        validate_hash(&reference.sha256)?;
        let path = self.blobs_dir().join(&reference.sha256);
        reject_symlink(&path)?;
        let bytes = fs::read(&path)?;
        verify_blob_bytes(&bytes, &reference.sha256, reference.bytes)?;
        Ok(bytes)
    }
}

fn deserialize_log(
    bytes: &[u8],
    session_id: &SessionId,
    agent_id: Option<&AgentId>,
) -> Result<Vec<EventEnvelope>, KuramaError> {
    let mut events = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let event: EventEnvelope = serde_json::from_slice(line).map_err(|error| {
            KuramaError::Storage(format!(
                "malformed complete JSONL record {}: {error}",
                index + 1
            ))
        })?;
        if event.schema_version != SCHEMA_VERSION {
            return Err(KuramaError::Storage(format!(
                "unsupported session schema version {}",
                event.schema_version
            )));
        }
        if &event.session_id != session_id || event.agent_id.as_ref() != agent_id {
            return Err(KuramaError::Storage(
                "event routed to the wrong session log".into(),
            ));
        }
        let expected = events
            .last()
            .map_or(0, |prior: &EventEnvelope| prior.sequence + 1);
        if event.sequence != expected {
            return Err(KuramaError::Storage(format!(
                "invalid event sequence {}; expected {expected}",
                event.sequence
            )));
        }
        events.push(event);
    }
    Ok(events)
}

fn ensure_directory(path: &Path) -> Result<(), KuramaError> {
    if path.exists() {
        reject_symlink(path)?;
        if !path.is_dir() {
            return Err(KuramaError::Storage(format!(
                "{} is not a directory",
                path.display()
            )));
        }
    } else {
        create_new_directory(path)?;
    }
    set_directory_permissions(path)?;
    Ok(())
}

fn create_new_directory(path: &Path) -> Result<(), KuramaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(DIRECTORY_MODE);
        builder.create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    set_directory_permissions(path)?;
    Ok(())
}

fn ensure_file(path: &Path, initial: &[u8]) -> Result<(), KuramaError> {
    if path.exists() {
        reject_symlink(path)?;
        if !path.is_file() {
            return Err(KuramaError::Storage(format!(
                "{} is not a file",
                path.display()
            )));
        }
        set_file_permissions(path)?;
        return Ok(());
    }
    create_new_file(path, initial)
}

fn create_new_file(path: &Path, bytes: &[u8]) -> Result<(), KuramaError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(FILE_MODE);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_file_permissions(path)?;
    Ok(())
}

fn open_locked(path: &Path, create: bool) -> Result<File, KuramaError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        reject_symlink(path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    options.mode(FILE_MODE);
    let file = options.open(path)?;
    File::lock(&file)?;
    set_file_permissions(path)?;
    Ok(file)
}

fn reject_symlink(path: &Path) -> Result<(), KuramaError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(KuramaError::Storage(format!(
            "refusing symbolic link {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_identifier(value: &str, kind: &str) -> Result<(), KuramaError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0'])
    {
        return Err(KuramaError::Storage(format!("invalid {kind} identifier")));
    }
    Ok(())
}

fn validate_hash(value: &str) -> Result<(), KuramaError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(KuramaError::Storage("invalid blob hash".into()));
    }
    Ok(())
}

fn verify_blob(path: &Path, expected_hash: &str, expected_bytes: u64) -> Result<(), KuramaError> {
    reject_symlink(path)?;
    let bytes = fs::read(path)?;
    verify_blob_bytes(&bytes, expected_hash, expected_bytes)
}

fn verify_blob_bytes(
    bytes: &[u8],
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<(), KuramaError> {
    if bytes.len() as u64 != expected_bytes {
        return Err(KuramaError::Storage("blob length mismatch".into()));
    }
    let actual = lowercase_hex(&Sha256::digest(bytes));
    if actual != expected_hash {
        return Err(KuramaError::Storage("blob hash mismatch".into()));
    }
    Ok(())
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn set_directory_permissions(path: &Path) -> Result<(), KuramaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))?;
    }
    Ok(())
}

fn set_file_permissions(path: &Path) -> Result<(), KuramaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), KuramaError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn now_ms() -> Result<u64, KuramaError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| KuramaError::Storage(format!("system clock before epoch: {error}")))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| KuramaError::Storage("timestamp exceeds u64 milliseconds".into()))
}

fn storage_error(context: &str, error: impl std::fmt::Display) -> KuramaError {
    KuramaError::Storage(format!("{context}: {error}"))
}
