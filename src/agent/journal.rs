use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum EventData {
    Created {
        id: Uuid,
        workspace: PathBuf,
    },
    ImportedMessages {
        messages: Vec<AgentMessage>,
    },
    UserInput {
        content: String,
    },
    RunStarted {
        model: String,
        context_length: u32,
        max_steps: u32,
        max_duration_secs: u64,
        compact_only: bool,
    },
    InferenceStarted {
        model: String,
        compaction: bool,
        step: u32,
    },
    AssistantTurn {
        turn: AgentTurn,
    },
    InterruptedOutput {
        content: String,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        id: Uuid,
        approved: bool,
    },
    ToolStarted {
        call: AgentToolCall,
    },
    ToolResult {
        call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    WorkStateUpdated {
        state: WorkState,
    },
    Compacted {
        summary: CompactionSummary,
    },
    StatusChanged {
        status: SessionStatus,
        message: Option<String>,
    },
    Recovery {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Record {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub event: EventData,
}

pub(super) struct Journal {
    pub directory: PathBuf,
    pub records: Vec<Record>,
    partials: Vec<PathBuf>,
}

pub(super) struct SessionLock(File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        // The descriptor is owned by this guard for the complete run.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(super) fn private_directory(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err(format!(
            "Session storage must be an owned, non-symlink 0700 directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "Session record must be an owned 0600 regular file: {}",
            path.display()
        ));
    }
    Ok(file)
}

impl Journal {
    pub fn create(root: &Path, id: Uuid, workspace: &Path) -> Result<Self, String> {
        if !root.exists() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)
                .map_err(|e| e.to_string())?;
        }
        private_directory(root)?;
        let directory = fs::canonicalize(root)
            .map_err(|e| e.to_string())?
            .join(id.to_string());
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|e| {
                format!("Cannot create a new session without overwriting existing data: {e}")
            })?;
        let mut journal = Self {
            directory,
            records: Vec::new(),
            partials: Vec::new(),
        };
        journal.append(EventData::Created {
            id,
            workspace: workspace.to_owned(),
        })?;
        File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(journal)
    }

    pub fn open(root: &Path, id: Uuid) -> Result<Self, String> {
        private_directory(root)?;
        let directory = fs::canonicalize(root)
            .map_err(|e| e.to_string())?
            .join(id.to_string());
        private_directory(&directory)?;
        let mut files = Vec::new();
        let mut partials = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pending") {
                partials.push(entry.path());
            }
            if name.len() == 25
                && name.ends_with(".json")
                && name[..20].bytes().all(|c| c.is_ascii_digit())
            {
                files.push(entry.path());
            }
        }
        files.sort();
        let mut records = Vec::new();
        for path in files {
            let mut file = private_file(&path)?;
            if file.metadata().map_err(|e| e.to_string())?.len() > MAX_RECORD_BYTES {
                return Err(format!("Oversized session record: {}", path.display()));
            }
            let mut data = String::new();
            file.read_to_string(&mut data).map_err(|e| e.to_string())?;
            let record: Record = serde_json::from_str(&data).map_err(|e| {
                format!(
                    "Invalid committed journal record; retained unchanged at {}: {e}",
                    path.display()
                )
            })?;
            let next = records.len() as u64 + 1;
            if record.seq != next
                || path.file_name().and_then(|n| n.to_str()) != Some(&format!("{next:020}.json"))
            {
                return Err(
                    "Journal sequence is incomplete; original records were retained".into(),
                );
            }
            records.push(record);
        }
        match records.first().map(|r| &r.event) {
            Some(EventData::Created { id: stored, .. }) if *stored == id => {}
            _ => {
                return Err(
                    "Session has no valid creation record; existing data was retained".into(),
                );
            }
        }
        Ok(Self {
            directory,
            records,
            partials,
        })
    }

    pub fn lock(&self) -> Result<SessionLock, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("run.lock"))
            .map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
        {
            return Err("Unsafe session lock file".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("This session already has an active run".into());
        }
        Ok(SessionLock(file))
    }

    pub fn append(&mut self, event: EventData) -> Result<u64, String> {
        let seq = self.records.len() as u64 + 1;
        let record = Record {
            seq,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            event,
        };
        let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("Session event exceeds the durable record limit".into());
        }
        let partial = self
            .directory
            .join(format!("{seq:020}.{}.pending", Uuid::new_v4()));
        let final_path = self.directory.join(format!("{seq:020}.json"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&partial)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        // hard_link is an atomic no-replace publication. A crash before unlink
        // may leave the same inode under .pending; recovery preserves its bytes.
        fs::hard_link(&partial, &final_path)
            .map_err(|e| format!("Cannot publish session record: {e}"))?;
        fs::remove_file(&partial).map_err(|e| e.to_string())?;
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        self.records.push(record);
        Ok(seq)
    }

    pub fn preserve_partials(&mut self) -> Result<(), String> {
        let mut names = Vec::new();
        for path in std::mem::take(&mut self.partials) {
            let name = path
                .file_name()
                .ok_or("Invalid partial record")?
                .to_string_lossy();
            let destination = self.directory.join(format!("{name}.interrupted"));
            if destination.exists() {
                return Err("Partial journal recovery destination already exists".into());
            }
            fs::rename(&path, &destination).map_err(|e| e.to_string())?;
            names.push(name.into_owned());
        }
        if !names.is_empty() {
            self.append(EventData::Recovery {
                message: format!(
                    "Uncommitted journal bytes retained as .interrupted; not replayed: {}",
                    names.join(", ")
                ),
            })?;
        }
        Ok(())
    }

    pub fn seen_call_ids(&self) -> BTreeSet<String> {
        self.records
            .iter()
            .flat_map(|record| match &record.event {
                EventData::AssistantTurn { turn } => {
                    turn.tool_calls.iter().map(|c| c.id.clone()).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }
}
