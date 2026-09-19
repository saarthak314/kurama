use std::{
    ffi::OsStr,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::fs_safe::{Directory, owner_only};
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

const METADATA_FILE: &str = "metadata.json";
const TAIL_SCAN_BYTES: usize = 8 * 1024;
const BLOB_SCAN_BYTES: usize = 64 * 1024;

#[derive(Debug, Default)]
struct LogScan {
    events: Vec<EventEnvelope>,
    next_sequence: u64,
    latest_timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct FsSessionStore {
    root: PathBuf,
    sessions: Directory,
    blobs: Directory,
    pending: Directory,
}

impl FsSessionStore {
    pub fn open(root: PathBuf) -> Result<Self, KuramaError> {
        let directory = Directory::ensure_root(&root)?;
        directory.ensure_file("config.toml", b"")?;
        let initial_state = serde_json::to_vec(&MutableState::default())
            .map_err(|error| storage_error("serialize initial state", error))?;
        directory.ensure_file("state.json", &initial_state)?;
        let sessions = directory.ensure_dir("sessions")?;
        let blobs = directory.ensure_dir("blobs")?;
        directory.ensure_dir("cache")?;
        let pending = directory.ensure_dir(".pending-sessions")?;
        Ok(Self {
            root,
            sessions,
            blobs,
            pending,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Verify the complete blob while retaining only its final `max_bytes` bytes.
    pub fn get_blob_tail(
        &self,
        reference: &BlobRef,
        max_bytes: usize,
    ) -> Result<Vec<u8>, KuramaError> {
        validate_hash(&reference.sha256)?;
        let mut file = self
            .blobs
            .open_file(&reference.sha256, false, false)
            .map_err(|error| storage_error("open blob", error))?;
        if file.metadata()?.len() != reference.bytes {
            return Err(KuramaError::Storage("blob length mismatch".into()));
        }
        let tail_len = usize::try_from(reference.bytes)
            .unwrap_or(usize::MAX)
            .min(max_bytes);
        let tail_start = reference.bytes - tail_len as u64;
        let mut tail = Vec::new();
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; BLOB_SCAN_BYTES];
        loop {
            let bytes_read = match file.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if bytes_read == 0 {
                break;
            }
            let next_total = total
                .checked_add(bytes_read as u64)
                .filter(|length| *length <= reference.bytes)
                .ok_or_else(|| KuramaError::Storage("blob length mismatch".into()))?;
            digest.update(&buffer[..bytes_read]);
            let start = tail_start.saturating_sub(total).min(bytes_read as u64) as usize;
            if start < bytes_read {
                if tail.is_empty() {
                    tail.reserve_exact(tail_len);
                }
                tail.extend_from_slice(&buffer[start..bytes_read]);
            }
            total = next_total;
        }
        if total != reference.bytes {
            return Err(KuramaError::Storage("blob length mismatch".into()));
        }
        if crate::id::hexadecimal("", digest.finalize().as_ref()) != reference.sha256 {
            return Err(KuramaError::Storage("blob hash mismatch".into()));
        }
        Ok(tail)
    }

    fn session_directory(&self, session_id: &SessionId) -> Result<Directory, KuramaError> {
        validate_identifier(session_id.as_ref(), "session")?;
        self.sessions.child(session_id.as_ref()).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                KuramaError::NotFound(session_id.to_string())
            } else {
                storage_io_error(error)
            }
        })
    }

    fn open_log(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
        create: bool,
        exclusive: bool,
    ) -> Result<File, KuramaError> {
        let session = self.session_directory(session_id)?;
        session.open_file(METADATA_FILE, false, false)?;
        let (directory, name) = match agent_id {
            Some(agent) => {
                validate_identifier(agent.as_ref(), "agent")?;
                (
                    session.child("agents")?,
                    format!("{}.jsonl", agent.as_ref()),
                )
            }
            None => (session, "events.jsonl".to_owned()),
        };
        open_locked(&directory, &name, create, exclusive)
    }

    fn open_for_append(&self, event: &EventEnvelope) -> Result<(File, u64), KuramaError> {
        if event.schema_version != SCHEMA_VERSION {
            return Err(KuramaError::Storage(format!(
                "unsupported session schema version {}",
                event.schema_version
            )));
        }
        let mut file = self.open_log(&event.session_id, event.agent_id.as_ref(), true, true)?;
        let prior = read_last_complete_record(&mut file)?;
        let next_sequence = match prior {
            Some(bytes) => deserialize_event(
                &bytes,
                &event.session_id,
                event.agent_id.as_ref(),
                "trailing",
            )?
            .sequence
            .checked_add(1)
            .ok_or_else(|| KuramaError::Storage("event sequence exceeds u64".into()))?,
            None => 0,
        };
        Ok((file, next_sequence))
    }

    fn replay_log(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
    ) -> Result<Vec<EventEnvelope>, KuramaError> {
        Ok(self.scan_log(session_id, agent_id, true)?.events)
    }

    fn summarize_log(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
    ) -> Result<Option<u64>, KuramaError> {
        Ok(self
            .scan_log(session_id, agent_id, false)?
            .latest_timestamp_ms)
    }

    fn scan_log(
        &self,
        session_id: &SessionId,
        agent_id: Option<&AgentId>,
        materialize_events: bool,
    ) -> Result<LogScan, KuramaError> {
        let mut file = self.open_log(session_id, agent_id, false, false)?;
        let (mut complete_end, mut removed_bytes) = complete_log_end(&mut file)?;
        if removed_bytes > 0 {
            // Never mutate under a shared lock. Another reader may repair the tail
            // during the upgrade, so recompute the boundary under exclusive ownership.
            file.unlock()?;
            file.lock()?;
            (complete_end, removed_bytes) = complete_log_end(&mut file)?;
        }
        let mut scan = scan_complete_log(
            &mut file,
            complete_end,
            session_id,
            agent_id,
            materialize_events,
        )?;
        if removed_bytes > 0 {
            let repair = EventEnvelope::new(
                scan.next_sequence,
                now_ms()?,
                session_id.clone(),
                agent_id.cloned(),
                SessionEvent::RecoveryRepair { removed_bytes },
            );
            let next_sequence = scan
                .next_sequence
                .checked_add(1)
                .ok_or_else(|| KuramaError::Storage("event sequence exceeds u64".into()))?;
            file.set_len(complete_end)?;
            commit_event(&mut file, &repair)?;
            scan.next_sequence = next_sequence;
            scan.latest_timestamp_ms = Some(
                scan.latest_timestamp_ms
                    .map_or(repair.timestamp_ms, |latest| {
                        latest.max(repair.timestamp_ms)
                    }),
            );
            if materialize_events {
                scan.events.push(repair);
            }
        }
        Ok(scan)
    }

    fn read_metadata(&self, session: &Directory) -> Result<SessionMetadata, KuramaError> {
        let bytes = session.read(METADATA_FILE)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| storage_error("deserialize session metadata", error))
    }
}

impl SessionStore for FsSessionStore {
    fn create(&self, metadata: &SessionMetadata) -> Result<(), KuramaError> {
        validate_identifier(metadata.id.as_ref(), "session")?;
        let encoded = serde_json::to_vec(metadata)
            .map_err(|error| storage_error("serialize session metadata", error))?;
        let directory_lock = self.sessions.lock_handle()?;
        directory_lock.lock()?;
        #[cfg(test)]
        fail_create_at(CreateStep::StageDirectory)?;
        let (temporary, session) = self.pending.temporary_directory()?;
        let mut published = false;
        let result = (|| {
            #[cfg(test)]
            fail_create_at(CreateStep::AgentsDirectory)?;
            let agents = session.create_dir("agents")?;
            #[cfg(test)]
            fail_create_at(CreateStep::SyncAgents)?;
            agents.sync()?;
            #[cfg(test)]
            fail_create_at(CreateStep::LogFile)?;
            let log = session.create_file("events.jsonl")?;
            #[cfg(test)]
            fail_create_at(CreateStep::SyncLog)?;
            log.sync_all()?;
            #[cfg(test)]
            fail_create_at(CreateStep::MetadataFile)?;
            let mut file = session.create_file(METADATA_FILE)?;
            #[cfg(test)]
            fail_create_at(CreateStep::MetadataWrite)?;
            file.write_all(&encoded)?;
            #[cfg(test)]
            fail_create_at(CreateStep::SyncMetadata)?;
            file.sync_all()?;
            #[cfg(test)]
            fail_create_at(CreateStep::SyncSession)?;
            session.sync()?;
            #[cfg(test)]
            fail_create_at(CreateStep::Publish)?;
            self.pending.publish_directory(
                &temporary,
                &self.sessions,
                OsStr::new(metadata.id.as_ref()),
            )?;
            published = true;
            // Both sides of the cross-directory rename must be durable.
            #[cfg(test)]
            fail_create_at(CreateStep::SyncSessions)?;
            self.sessions.sync()?;
            #[cfg(test)]
            fail_create_at(CreateStep::SyncPending)?;
            self.pending.sync()?;
            Ok(())
        })();
        if result.is_err() && !published {
            // Ordinary failures leave no partial session; a crash can leave only an
            // unpublished staging directory, never a visible half-created session.
            let _ = session.remove_file(METADATA_FILE);
            let _ = session.remove_file("events.jsonl");
            let _ = session.remove_dir("agents");
            let _ = self.pending.remove_dir(&temporary);
        }
        result
    }

    fn append(&self, event: &EventEnvelope) -> Result<(), KuramaError> {
        let (mut file, expected) = self.open_for_append(event)?;
        if event.sequence != expected {
            return Err(KuramaError::Storage(format!(
                "invalid event sequence {}; expected {expected}",
                event.sequence
            )));
        }
        commit_event(&mut file, event)
    }

    fn append_next(&self, event: &mut EventEnvelope) -> Result<(), KuramaError> {
        let (mut file, next_sequence) = self.open_for_append(event)?;
        event.sequence = next_sequence;
        commit_event(&mut file, event)
    }

    fn replay(&self, session_id: &SessionId) -> Result<Vec<EventEnvelope>, KuramaError> {
        self.replay_log(session_id, None)
    }

    fn replay_agent(
        &self,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<Vec<EventEnvelope>, KuramaError> {
        self.session_directory(session_id)?
            .open_file(METADATA_FILE, false, false)?;
        match self.replay_log(session_id, Some(agent_id)) {
            Err(KuramaError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Vec::new())
            }
            result => result,
        }
    }

    fn list(&self) -> Result<Vec<SessionSummary>, KuramaError> {
        let mut summaries = Vec::new();
        for name in self.sessions.entries()? {
            let name = name?;
            let session = match self.sessions.child(&name) {
                Ok(session) => session,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(storage_io_error(error)),
            };
            let metadata = match self.read_metadata(&session) {
                Ok(metadata) => metadata,
                Err(KuramaError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if name != OsStr::new(metadata.id.as_ref()) {
                return Err(KuramaError::Storage(
                    "session metadata identifier does not match directory".into(),
                ));
            }
            let mut updated_at_ms = metadata.created_at_ms;
            if let Some(timestamp) = self.summarize_log(&metadata.id, None)? {
                updated_at_ms = updated_at_ms.max(timestamp);
            }
            for name in session.child("agents")?.entries()? {
                let name = name?;
                let path = Path::new(&name);
                if path.extension() != Some(OsStr::new("jsonl")) {
                    continue;
                }
                let agent = path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| KuramaError::Storage("invalid child log file name".into()))?;
                if let Some(timestamp) =
                    self.summarize_log(&metadata.id, Some(&AgentId::from(agent)))?
                {
                    updated_at_ms = updated_at_ms.max(timestamp);
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
        let sha256 = crate::id::hexadecimal("", Sha256::digest(bytes).as_ref());
        let directory_lock = self.blobs.lock_handle()?;
        directory_lock.lock()?;
        match self.blobs.open_file(&sha256, false, false) {
            Ok(mut file) => verify_blob(&mut file, &sha256, bytes.len() as u64)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.blobs.replace(&sha256, bytes)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(BlobRef {
            sha256,
            bytes: bytes.len() as u64,
        })
    }

    fn get_blob(&self, reference: &BlobRef) -> Result<Vec<u8>, KuramaError> {
        validate_hash(&reference.sha256)?;
        let bytes = self
            .blobs
            .read(&reference.sha256)
            .map_err(|error| storage_error("read blob", error))?;
        verify_blob_bytes(&bytes, &reference.sha256, reference.bytes)?;
        Ok(bytes)
    }
}

fn commit_event(file: &mut File, event: &EventEnvelope) -> Result<(), KuramaError> {
    let mut encoded = serde_json::to_vec(event)
        .map_err(|error| storage_error("serialize session event", error))?;
    encoded.push(b'\n');
    file.seek(SeekFrom::End(0))?;
    file.write_all(&encoded)?;
    file.sync_data()?;
    Ok(())
}

fn complete_log_end(file: &mut File) -> Result<(u64, u64), KuramaError> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok((0, 0));
    }

    let mut last = [0_u8; 1];
    file.seek(SeekFrom::Start(length - 1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok((length, 0));
    }

    let mut position = length - 1;
    let mut buffer = [0_u8; TAIL_SCAN_BYTES];
    while position > 0 {
        let start = position.saturating_sub(TAIL_SCAN_BYTES as u64);
        let bytes_to_read = usize::try_from(position - start)
            .map_err(|_| KuramaError::Storage("JSONL tail exceeds address space".into()))?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..bytes_to_read])?;
        if let Some(index) = buffer[..bytes_to_read]
            .iter()
            .rposition(|byte| *byte == b'\n')
        {
            let complete_end = start + index as u64 + 1;
            return Ok((complete_end, length - complete_end));
        }
        position = start;
    }
    Ok((0, length))
}

fn read_last_complete_record(file: &mut File) -> Result<Option<Vec<u8>>, KuramaError> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(None);
    }

    let mut position = length;
    let mut saw_terminal_newline = false;
    let mut collecting_record = false;
    let mut reversed_record = Vec::new();
    let mut buffer = [0_u8; TAIL_SCAN_BYTES];
    while position > 0 {
        let start = position.saturating_sub(TAIL_SCAN_BYTES as u64);
        let bytes_to_read = usize::try_from(position - start)
            .map_err(|_| KuramaError::Storage("JSONL tail exceeds address space".into()))?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..bytes_to_read])?;

        for byte in buffer[..bytes_to_read].iter().rev().copied() {
            if !saw_terminal_newline {
                if byte != b'\n' {
                    return Err(KuramaError::Storage(
                        "incomplete trailing JSONL record; replay before append".into(),
                    ));
                }
                saw_terminal_newline = true;
                continue;
            }
            if !collecting_record {
                if byte == b'\n' {
                    continue;
                }
                collecting_record = true;
                reversed_record.push(byte);
                continue;
            }
            if byte == b'\n' {
                reversed_record.reverse();
                return Ok(Some(reversed_record));
            }
            reversed_record.push(byte);
        }
        position = start;
    }

    if collecting_record {
        reversed_record.reverse();
        Ok(Some(reversed_record))
    } else {
        Ok(None)
    }
}

fn deserialize_event(
    line: &[u8],
    session_id: &SessionId,
    agent_id: Option<&AgentId>,
    record: impl std::fmt::Display,
) -> Result<EventEnvelope, KuramaError> {
    let event: EventEnvelope = serde_json::from_slice(line).map_err(|error| {
        KuramaError::Storage(format!("malformed complete JSONL record {record}: {error}"))
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
    Ok(event)
}

fn scan_complete_log(
    file: &mut File,
    complete_end: u64,
    session_id: &SessionId,
    agent_id: Option<&AgentId>,
    materialize_events: bool,
) -> Result<LogScan, KuramaError> {
    file.seek(SeekFrom::Start(0))?;
    let limited = std::io::Read::take(&mut *file, complete_end);
    let mut reader = BufReader::new(limited);
    let mut line = Vec::new();
    let mut scan = LogScan::default();
    let mut record_number = 0_u64;
    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        record_number += 1;
        if line.last() != Some(&b'\n') {
            return Err(KuramaError::Storage(
                "incomplete trailing JSONL record; replay before append".into(),
            ));
        }
        line.pop();
        if line.is_empty() {
            continue;
        }
        let event = deserialize_event(&line, session_id, agent_id, record_number)?;
        if event.sequence != scan.next_sequence {
            return Err(KuramaError::Storage(format!(
                "invalid event sequence {}; expected {}",
                event.sequence, scan.next_sequence
            )));
        }
        scan.next_sequence = scan
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| KuramaError::Storage("event sequence exceeds u64".into()))?;
        scan.latest_timestamp_ms = Some(
            scan.latest_timestamp_ms
                .map_or(event.timestamp_ms, |latest| latest.max(event.timestamp_ms)),
        );
        if materialize_events {
            scan.events.push(event);
        }
    }
    Ok(scan)
}

fn open_locked(
    directory: &Directory,
    name: &str,
    create: bool,
    exclusive: bool,
) -> Result<File, KuramaError> {
    let file = match directory.open_file(name, true, false) {
        Ok(file) => file,
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            let file = match directory.create_file(name) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    directory.open_file(name, true, false)?
                }
                Err(error) => return Err(error.into()),
            };
            directory.sync()?;
            file
        }
        Err(error) => return Err(storage_io_error(error)),
    };
    if exclusive {
        file.lock()?;
    } else {
        file.lock_shared()?;
    }
    owner_only(&file, false)?;
    Ok(file)
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

fn verify_blob(
    file: &mut File,
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<(), KuramaError> {
    if file.metadata()?.len() != expected_bytes {
        return Err(KuramaError::Storage("blob length mismatch".into()));
    }
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; BLOB_SCAN_BYTES];
    loop {
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .filter(|total| *total <= expected_bytes)
            .ok_or_else(|| KuramaError::Storage("blob length mismatch".into()))?;
        digest.update(&buffer[..count]);
    }
    if total != expected_bytes {
        return Err(KuramaError::Storage("blob length mismatch".into()));
    }
    if crate::id::hexadecimal("", digest.finalize().as_ref()) != expected_hash {
        return Err(KuramaError::Storage("blob hash mismatch".into()));
    }
    Ok(())
}

fn verify_blob_bytes(
    bytes: &[u8],
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<(), KuramaError> {
    if bytes.len() as u64 != expected_bytes {
        return Err(KuramaError::Storage("blob length mismatch".into()));
    }
    let actual = crate::id::hexadecimal("", Sha256::digest(bytes).as_ref());
    if actual != expected_hash {
        return Err(KuramaError::Storage("blob hash mismatch".into()));
    }
    Ok(())
}

fn now_ms() -> Result<u64, KuramaError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| KuramaError::Storage(format!("system clock before epoch: {error}")))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| KuramaError::Storage("timestamp exceeds u64 milliseconds".into()))
}

fn storage_io_error(error: std::io::Error) -> KuramaError {
    if error.kind() == std::io::ErrorKind::NotFound {
        error.into()
    } else {
        storage_error("filesystem access", error)
    }
}

fn storage_error(context: &str, error: impl std::fmt::Display) -> KuramaError {
    KuramaError::Storage(format!("{context}: {error}"))
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CreateStep {
    StageDirectory,
    AgentsDirectory,
    SyncAgents,
    LogFile,
    SyncLog,
    MetadataFile,
    MetadataWrite,
    SyncMetadata,
    SyncSession,
    Publish,
    SyncSessions,
    SyncPending,
}

#[cfg(test)]
std::thread_local! {
    static CREATE_FAILURE: std::cell::Cell<Option<CreateStep>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fail_create_at(step: CreateStep) -> Result<(), KuramaError> {
    CREATE_FAILURE.with(|failure| {
        if failure.get() == Some(step) {
            failure.set(None);
            Err(std::io::Error::other("injected session creation failure").into())
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurama_protocol::policy::ExecutionMode;

    #[test]
    fn failed_session_creation_is_absent_or_complete_at_every_durability_boundary() {
        for step in [
            CreateStep::StageDirectory,
            CreateStep::AgentsDirectory,
            CreateStep::SyncAgents,
            CreateStep::LogFile,
            CreateStep::SyncLog,
            CreateStep::MetadataFile,
            CreateStep::MetadataWrite,
            CreateStep::SyncMetadata,
            CreateStep::SyncSession,
            CreateStep::Publish,
            CreateStep::SyncSessions,
            CreateStep::SyncPending,
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
            let metadata = SessionMetadata {
                id: SessionId::from("fault"),
                created_at_ms: 1,
                project_root: temp.path().to_string_lossy().into_owned(),
                profile: "profile".into(),
                mode: ExecutionMode::Supervised,
                redaction_best_effort: false,
            };
            CREATE_FAILURE.with(|failure| failure.set(Some(step)));
            assert!(store.create(&metadata).is_err());
            let reopened = FsSessionStore::open(temp.path().to_owned()).expect("reopen");
            let published = matches!(step, CreateStep::SyncSessions | CreateStep::SyncPending);
            assert_eq!(
                reopened.list().expect("no partial session").len(),
                usize::from(published)
            );
            if !published {
                assert!(!temp.path().join("sessions/fault").exists());
                reopened.create(&metadata).expect("retry succeeds");
            }
            assert!(
                reopened
                    .replay(&metadata.id)
                    .expect("complete log")
                    .is_empty()
            );
            assert!(temp.path().join("sessions/fault/agents").is_dir());
            assert!(
                std::fs::read_dir(temp.path().join(".pending-sessions"))
                    .expect("staging")
                    .next()
                    .is_none()
            );
            reopened
                .append(&EventEnvelope::new(
                    0,
                    2,
                    metadata.id.clone(),
                    None,
                    SessionEvent::UserMessage {
                        text: "after failure".into(),
                        explicit_delegation: false,
                    },
                ))
                .expect("appendable session");
            assert_eq!(
                reopened.replay(&metadata.id).expect("durable event").len(),
                1
            );
        }
    }
}
