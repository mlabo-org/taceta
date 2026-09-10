//! Bounded workspace tools. Approval belongs to the agent loop; prepared effects
//! contain the exact bytes/command shown by that loop and cannot be retargeted.

use super::{AgentToolCall, ToolDefinition};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::watch;

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_ENTRIES: usize = 4_096;
const MAX_SEARCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEARCH_DEPTH: usize = 24;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

// These components are excluded from every file tool and from sandboxed
// commands, including files created after the command was approved.
const PRIVATE_NAMES: &[&str] = &[
    ".git",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config",
    ".codex",
    ".claude",
    ".gemini",
    ".netrc",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pypirc",
    ".pgpass",
    ".git-credentials",
    ".vault-token",
    ".boto",
    ".s3cfg",
    ".gitconfig",
    "credentials",
    "credentials.json",
    "credentials.toml",
    "credentials.yaml",
    "credentials.yml",
    "secrets",
    "secrets.json",
    "secrets.toml",
    "secrets.yaml",
    "secrets.yml",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "auth.json",
    "tokens.json",
    "service-account.json",
    "service_account.json",
    "application_default_credentials.json",
    "keychains",
    "terraform.tfstate",
    "terraform.tfstate.backup",
];
const PRIVATE_SUFFIXES: &[&str] = &[
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keystore",
    ".keychain",
    ".keychain-db",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(file: &File) -> Result<Self, String> {
        let metadata = file.metadata().map_err(io_error)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

struct Root {
    path: PathBuf,
    directory: File,
    identity: Identity,
}

pub(super) struct WorkspaceTools {
    root: Arc<Root>,
}

#[derive(Clone)]
struct RelativePath {
    path: PathBuf,
    parts: Vec<CString>,
}

impl RelativePath {
    fn parse(value: &str, allow_root: bool) -> Result<Self, String> {
        if value.len() > 4_096 || value.contains('\0') {
            return Err("Path is too long or contains a NUL byte.".into());
        }
        let mut path = PathBuf::new();
        let mut parts = Vec::new();
        for component in Path::new(value).components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => {
                    let name = name.to_str().ok_or("Path must be UTF-8.")?;
                    if private_name(name) {
                        return Err("Git metadata and credential paths are not accessible.".into());
                    }
                    parts.push(c_string(OsStr::new(name))?);
                    path.push(name);
                }
                _ => return Err("Use a relative path without '..' inside the workspace.".into()),
            }
        }
        if parts.len() > 64 || (!allow_root && parts.is_empty()) {
            return Err("Expected a file path inside the workspace.".into());
        }
        Ok(Self { path, parts })
    }

    fn display(&self) -> String {
        if self.parts.is_empty() {
            ".".into()
        } else {
            self.path.to_string_lossy().into_owned()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "root_path")]
    path: String,
    #[serde(default = "default_list_limit")]
    max_entries: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "first_line")]
    start_line: usize,
    #[serde(default = "default_line_limit")]
    max_lines: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "root_path")]
    path: String,
    #[serde(default = "default_search_limit")]
    max_results: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    /// None creates a new file. An existing file requires one exact nonempty match.
    old_text: Option<String>,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: String,
    #[serde(default = "root_path")]
    working_directory: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn root_path() -> String {
    ".".into()
}
fn default_list_limit() -> usize {
    200
}
fn first_line() -> usize {
    1
}
fn default_line_limit() -> usize {
    200
}
fn default_search_limit() -> usize {
    50
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

pub(super) struct PreparedTool {
    root: Arc<Root>,
    action: Action,
}

enum Action {
    List(RelativePath, usize),
    Read(RelativePath, usize, usize),
    Search(RelativePath, String, usize),
    Edit(FrozenEdit),
    Command(FrozenCommand),
}

struct FrozenEdit {
    path: RelativePath,
    parent: Identity,
    original: Option<(Identity, Vec<u8>, u32)>,
    replacement: Vec<u8>,
}

struct FrozenCommand {
    directory: RelativePath,
    identity: Identity,
    command: String,
    timeout: Duration,
}

impl WorkspaceTools {
    pub(super) fn new(workspace: &Path) -> Result<Self, String> {
        if workspace.as_os_str().is_empty() || !workspace.is_absolute() {
            return Err("Select an explicit absolute workspace directory.".into());
        }
        let path = workspace.canonicalize().map_err(io_error)?;
        let home = std::env::var_os("HOME").and_then(|p| PathBuf::from(p).canonicalize().ok());
        let temporary = std::env::temp_dir().canonicalize().map_err(io_error)?;
        if path.parent().is_none()
            || home.as_ref().is_some_and(|home| home.starts_with(&path))
            || home.as_ref().is_some_and(|home| {
                path.parent() == Some(home.as_path())
                    && path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| {
                            [
                                "Desktop",
                                "Documents",
                                "Downloads",
                                "Library",
                                "Pictures",
                                "Movies",
                                "Music",
                            ]
                            .contains(&name)
                        })
            })
            || path == temporary
            || [
                "/Users",
                "/Applications",
                "/Library",
                "/System",
                "/Volumes",
                "/private",
                "/private/tmp",
                "/private/var",
                "/usr",
                "/opt",
            ]
            .iter()
            .any(|broad| path == Path::new(broad))
            || path.components().any(|component| match component {
                Component::Normal(name) => name.to_str().is_none_or(private_name),
                _ => false,
            })
        {
            return Err("Select one project directory; root, home, shared system roots and credential directories are not workspaces.".into());
        }
        let directory = open_directory(&path)?;
        let identity = Identity::of(&directory)?;
        Ok(Self {
            root: Arc::new(Root {
                path,
                directory,
                identity,
            }),
        })
    }

    pub(super) fn workspace(&self) -> &Path {
        &self.root.path
    }

    pub(super) fn definitions() -> Vec<ToolDefinition> {
        vec![
            definition(
                "list_directory",
                "List a workspace-relative directory. Git metadata, credentials, and symbolic links are excluded. Results are bounded.",
                json!({"path":{"type":"string"},"max_entries":{"type":"integer","minimum":1,"maximum":200}}),
                &[],
            ),
            definition(
                "read_file",
                "Read bounded UTF-8 text using a relative path. Read applicable nested AGENTS.md instructions before changing files. No symlinks or hardlinks are followed.",
                json!({"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":1000}}),
                &["path"],
            ),
            definition(
                "search_files",
                "Search literal, case-sensitive text in UTF-8 workspace files. Traversal, bytes, and results are bounded; protected paths and symlinks are excluded.",
                json!({"query":{"type":"string","minLength":1},"path":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":100}}),
                &["query"],
            ),
            definition(
                "edit_file",
                "Request approval for one exact text replacement. Existing files require exactly one nonempty old_text match. Use old_text:null only to create a file in an existing directory. Execution refuses changed originals and protected paths.",
                json!({"path":{"type":"string"},"old_text":{"type":["string","null"]},"new_text":{"type":"string"}}),
                &["path", "old_text", "new_text"],
            ),
            definition(
                "run_command",
                "Request approval for an exact /bin/sh command in the workspace. macOS sandbox-exec is mandatory. Network and credential access are denied; writes are limited to the workspace and private temporary storage. The inherited environment is cleared. Background processes end with this command. Maximum duration is 120 seconds; output is bounded.",
                json!({"command":{"type":"string","minLength":1},"working_directory":{"type":"string"},"timeout_ms":{"type":"integer","minimum":100,"maximum":MAX_TIMEOUT_MS}}),
                &["command"],
            ),
        ]
    }

    pub(super) fn instructions(&self) -> Result<Vec<(String, String)>, String> {
        let path = RelativePath::parse("AGENTS.md", false)?;
        match self.root.read(&path) {
            Ok((_, bytes, _)) => {
                let text =
                    String::from_utf8(bytes).map_err(|_| "Workspace AGENTS.md must be UTF-8.")?;
                if text.len() > MAX_RESULT_BYTES {
                    return Err("Workspace AGENTS.md exceeds the 64 KiB instruction limit.".into());
                }
                Ok(vec![(
                    self.root
                        .path
                        .join("AGENTS.md")
                        .to_string_lossy()
                        .into_owned(),
                    text,
                )])
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(io_error(error)),
        }
    }

    pub(super) fn prepare(&self, call: &AgentToolCall) -> Result<PreparedTool, String> {
        self.root.check()?;
        let action = match call.name.as_str() {
            "list_directory" => {
                let args: ListArgs = parse_arguments(&call.arguments)?;
                bounded(args.max_entries, 1, 200, "max_entries")?;
                let path = RelativePath::parse(&args.path, true)?;
                self.root.directory(&path)?;
                Action::List(path, args.max_entries)
            }
            "read_file" => {
                let args: ReadArgs = parse_arguments(&call.arguments)?;
                bounded(args.start_line, 1, 1_000_000, "start_line")?;
                bounded(args.max_lines, 1, 1_000, "max_lines")?;
                Action::Read(
                    RelativePath::parse(&args.path, false)?,
                    args.start_line,
                    args.max_lines,
                )
            }
            "search_files" => {
                let args: SearchArgs = parse_arguments(&call.arguments)?;
                bounded(args.query.len(), 1, 1_024, "query bytes")?;
                bounded(args.max_results, 1, 100, "max_results")?;
                let path = RelativePath::parse(&args.path, true)?;
                self.root.directory(&path)?;
                Action::Search(path, args.query, args.max_results)
            }
            "edit_file" => {
                let args: EditArgs = parse_arguments(&call.arguments)?;
                if !call
                    .arguments
                    .as_object()
                    .is_some_and(|args| args.contains_key("old_text"))
                {
                    return Err("old_text is required; use null only for a new file.".into());
                }
                bounded(args.new_text.len(), 0, MAX_FILE_BYTES, "new_text bytes")?;
                let path = RelativePath::parse(&args.path, false)?;
                let (parent, _) = self.root.parent(&path)?;
                let original = match self.root.read(&path) {
                    Ok(original) => Some(original),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(io_error(error)),
                };
                let replacement = match (&original, &args.old_text) {
                    (None, None) => args.new_text.into_bytes(),
                    (None, Some(_)) => {
                        return Err("File does not exist; use old_text:null to create it.".into());
                    }
                    (Some(_), None) => {
                        return Err("File already exists; provide its exact old_text.".into());
                    }
                    (Some((_, bytes, _)), Some(old)) => {
                        bounded(old.len(), 1, MAX_FILE_BYTES, "old_text bytes")?;
                        let text = std::str::from_utf8(bytes)
                            .map_err(|_| "Only UTF-8 text files can be edited.")?;
                        if text.match_indices(old.as_str()).take(2).count() != 1 {
                            return Err(
                                "old_text must match exactly once; no edit was prepared.".into()
                            );
                        }
                        text.replacen(old.as_str(), &args.new_text, 1).into_bytes()
                    }
                };
                bounded(replacement.len(), 0, MAX_FILE_BYTES, "resulting file bytes")?;
                Action::Edit(FrozenEdit {
                    path,
                    parent: Identity::of(&parent)?,
                    original,
                    replacement,
                })
            }
            "run_command" => {
                let args: CommandArgs = parse_arguments(&call.arguments)?;
                bounded(args.command.len(), 1, MAX_COMMAND_BYTES, "command bytes")?;
                if args.command.trim().is_empty() || args.command.contains('\0') {
                    return Err("Command must be nonempty text without NUL bytes.".into());
                }
                if !(100..=MAX_TIMEOUT_MS).contains(&args.timeout_ms) {
                    return Err(format!(
                        "timeout_ms must be between 100 and {MAX_TIMEOUT_MS}."
                    ));
                }
                sandbox_available()?;
                let directory = RelativePath::parse(&args.working_directory, true)?;
                let identity = Identity::of(&self.root.directory(&directory)?)?;
                self.root.command_boundary()?;
                Action::Command(FrozenCommand {
                    directory,
                    identity,
                    command: args.command,
                    timeout: Duration::from_millis(args.timeout_ms),
                })
            }
            _ => {
                return Err(format!(
                    "Unknown workspace tool: {}",
                    call.name.chars().take(100).collect::<String>()
                ));
            }
        };
        Ok(PreparedTool {
            root: Arc::clone(&self.root),
            action,
        })
    }
}

impl PreparedTool {
    pub(super) fn proposal(&self) -> Option<(String, String)> {
        match &self.action {
            Action::Edit(edit) => Some((
                format!("ファイルを{}: {}", if edit.original.is_some() { "編集" } else { "作成" }, edit.path.display()),
                json!({"workspace":self.root.path,"path":edit.path.display(),"before":edit.original.as_ref().map(|(_,bytes,_)|String::from_utf8_lossy(bytes)),"after":String::from_utf8_lossy(&edit.replacement)}).to_string(),
            )),
            Action::Command(command) => Some((
                "ワークスペース内でコマンドを実行".into(),
                json!({"workspace":self.root.path,"working_directory":command.directory.display(),"command":command.command,"timeout_ms":command.timeout.as_millis(),"network":"denied","writes":"workspace and private temporary storage only"}).to_string(),
            )),
            _ => None,
        }
    }

    pub(super) async fn execute(self, cancel: watch::Receiver<bool>) -> Result<String, String> {
        if *cancel.borrow() {
            return Err("Tool execution cancelled before it started.".into());
        }
        self.root.check()?;
        match self.action {
            Action::List(path, limit) => self.root.list(&path, limit),
            Action::Read(path, start, limit) => self.root.read_text(&path, start, limit),
            Action::Search(path, query, limit) => self.root.search(&path, &query, limit, &cancel),
            Action::Edit(edit) => self.root.edit(edit, &cancel),
            Action::Command(command) => run_command(self.root, command, cancel).await,
        }
    }
}

impl Root {
    fn check(&self) -> Result<(), String> {
        let current = open_directory(&self.path)?;
        if Identity::of(&current)? != self.identity {
            return Err("Workspace directory changed; select it again before continuing.".into());
        }
        Ok(())
    }

    fn directory(&self, path: &RelativePath) -> Result<File, String> {
        self.check()?;
        let mut directory = open_at(&self.directory, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(io_error)?;
        for part in &path.parts {
            directory = open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(
                |error| format!("Cannot open workspace directory without following links: {error}"),
            )?;
        }
        self.check_directory(&directory, &self.path.join(&path.path))?;
        Ok(directory)
    }

    fn check_directory(&self, directory: &File, expected: &Path) -> Result<(), String> {
        self.check()?;
        #[cfg(target_os = "macos")]
        {
            let mut bytes = vec![0u8; libc::PATH_MAX as usize];
            // F_GETPATH asks the kernel for this opened vnode's current path.
            if unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) }
                < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            let actual = unsafe { CStr::from_ptr(bytes.as_ptr().cast()) };
            if Path::new(OsStr::from_bytes(actual.to_bytes())) != expected {
                return Err("Workspace directory moved during tool execution.".into());
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let actual = expected.canonicalize().map_err(io_error)?;
            if !actual.starts_with(&self.path)
                || Identity::of(&open_directory(&actual)?)? != Identity::of(directory)?
            {
                return Err("Workspace directory moved during tool execution.".into());
            }
        }
        Ok(())
    }

    fn parent(&self, path: &RelativePath) -> Result<(File, CString), String> {
        let name = path.parts.last().ok_or("Expected a file path.")?.clone();
        let parent = RelativePath {
            path: path.path.parent().unwrap_or(Path::new("")).to_path_buf(),
            parts: path.parts[..path.parts.len() - 1].to_vec(),
        };
        Ok((self.directory(&parent)?, name))
    }

    fn read(&self, path: &RelativePath) -> std::io::Result<(Identity, Vec<u8>, u32)> {
        let (parent, name) = self.parent(path).map_err(std::io::Error::other)?;
        let file = open_at(&parent, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        read_regular(file)
    }

    fn list(&self, path: &RelativePath, limit: usize) -> Result<String, String> {
        let directory = self.directory(path)?;
        let (entries, scanned_limit) = directory_entries(&directory, MAX_ENTRIES)?;
        let mut output = Vec::new();
        let mut output_bytes = 0usize;
        let mut truncated = scanned_limit;
        for name in entries {
            let Some(text) = name.to_str() else {
                continue;
            };
            if private_name(text) {
                continue;
            }
            let Some(kind) = entry_kind(&directory, &name)? else {
                continue;
            };
            let entry = json!({"name":text,"kind":kind});
            output_bytes += entry.to_string().len() + 1;
            if output.len() == limit || output_bytes > MAX_RESULT_BYTES / 2 {
                truncated = true;
                break;
            }
            output.push(entry);
        }
        Ok(json!({"path":path.display(),"entries":output,"truncated":truncated}).to_string())
    }

    fn read_text(&self, path: &RelativePath, start: usize, limit: usize) -> Result<String, String> {
        let (_, bytes, _) = self.read(path).map_err(io_error)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "File is not UTF-8 text.")?;
        let mut output = String::new();
        let mut truncated = false;
        for (offset, line) in text.split_inclusive('\n').enumerate().skip(start - 1) {
            if offset >= start - 1 + limit {
                truncated = true;
                break;
            }
            let remaining = (MAX_RESULT_BYTES / 12).saturating_sub(output.len());
            let selected = truncate_utf8(line, remaining);
            output.push_str(selected);
            if selected.len() != line.len() {
                truncated = true;
                break;
            }
        }
        Ok(
            json!({"path":path.display(),"start_line":start,"text":output,"truncated":truncated})
                .to_string(),
        )
    }

    fn search(
        &self,
        path: &RelativePath,
        query: &str,
        limit: usize,
        cancel: &watch::Receiver<bool>,
    ) -> Result<String, String> {
        let mut stack = vec![(path.clone(), 0usize)];
        let mut scanned = 0usize;
        let mut bytes_read = 0usize;
        let mut results = Vec::new();
        let mut result_bytes = 0usize;
        let mut truncated = false;
        'walk: while let Some((path, depth)) = stack.pop() {
            if *cancel.borrow() {
                return Err("Search cancelled.".into());
            }
            let directory = self.directory(&path)?;
            let (entries, more) =
                directory_entries(&directory, MAX_ENTRIES.saturating_sub(scanned))?;
            truncated |= more;
            for name in entries {
                scanned += 1;
                let Some(name) = name.to_str() else {
                    continue;
                };
                if private_name(name) {
                    continue;
                }
                let child = RelativePath::parse(&path.path.join(name).to_string_lossy(), false)?;
                match entry_kind(&directory, OsStr::new(name))?.as_deref() {
                    Some("directory") if depth < MAX_SEARCH_DEPTH => stack.push((child, depth + 1)),
                    Some("directory") => truncated = true,
                    Some("file") => {
                        let (_, bytes, _) = match self.read(&child) {
                            Ok(file) => file,
                            Err(_) => {
                                truncated = true;
                                continue;
                            }
                        };
                        bytes_read += bytes.len();
                        if bytes_read > MAX_SEARCH_BYTES {
                            truncated = true;
                            break 'walk;
                        }
                        let Ok(text) = std::str::from_utf8(&bytes) else {
                            continue;
                        };
                        for (line, text) in text.lines().enumerate() {
                            if text.contains(query) {
                                let snippet = truncate_utf8(text, 1_024);
                                let result =
                                    json!({"path":child.display(),"line":line+1,"text":snippet});
                                result_bytes += result.to_string().len() + 1;
                                if results.len() == limit || result_bytes > MAX_RESULT_BYTES - 1_024
                                {
                                    truncated = true;
                                    break 'walk;
                                }
                                results.push(result);
                            }
                        }
                    }
                    _ => {}
                }
                if scanned == MAX_ENTRIES {
                    truncated = true;
                    break 'walk;
                }
            }
        }
        Ok(json!({"matches":results,"truncated":truncated,"scanned_entries":scanned}).to_string())
    }

    fn edit(&self, edit: FrozenEdit, cancel: &watch::Receiver<bool>) -> Result<String, String> {
        let (parent, name) = self.parent(&edit.path)?;
        if Identity::of(&parent)? != edit.parent {
            return Err(
                "Edit refused: parent directory changed after approval was requested.".into(),
            );
        }
        self.check_original(&edit)?;
        let temporary_name = CString::new(format!(".taceta-edit-{}.tmp", uuid::Uuid::new_v4()))
            .expect("UUID contains no NUL");
        let mut temporary = TemporaryFile {
            parent: parent.try_clone().map_err(io_error)?,
            name: temporary_name,
            present: false,
        };
        let mut output = open_at(
            &parent,
            &temporary.name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(io_error)?;
        temporary.present = true;
        output.write_all(&edit.replacement).map_err(io_error)?;
        let mode = edit
            .original
            .as_ref()
            .map_or(0o600, |(_, _, mode)| *mode & 0o777);
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(io_error)?;
        output.sync_all().map_err(io_error)?;
        if *cancel.borrow() {
            return Err("Edit cancelled before replacing the file.".into());
        }
        self.check_directory(
            &parent,
            &self
                .path
                .join(edit.path.path.parent().unwrap_or(Path::new(""))),
        )?;
        self.check_original(&edit)?;
        if edit.original.is_some() {
            // Rename replaces the directory entry, never follows a raced symlink
            // and never modifies an inode shared through a hardlink.
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.name.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            temporary.present = false;
        } else {
            // linkat is an atomic no-replace publication for a newly created file.
            if unsafe {
                libc::linkat(
                    parent.as_raw_fd(),
                    temporary.name.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(format!(
                    "New file was not created (the destination may have appeared): {}",
                    std::io::Error::last_os_error()
                ));
            }
            if unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.name.as_ptr(), 0) } != 0 {
                return Err("File created, but its temporary link could not be removed.".into());
            }
            temporary.present = false;
        }
        parent.sync_all().map_err(|error| {
            format!("File replaced, but directory durability could not be confirmed: {error}")
        })?;
        Ok(json!({"path":edit.path.display(),"bytes_written":edit.replacement.len(),"status":"written"}).to_string())
    }

    fn check_original(&self, edit: &FrozenEdit) -> Result<(), String> {
        match (&edit.original, self.read(&edit.path)) {
            (Some((expected_identity, expected_bytes, expected_mode)), Ok((identity, bytes, mode)))
                if *expected_identity == identity && *expected_bytes == bytes && *expected_mode == mode => Ok(()),
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("Edit refused: original file changed after approval was requested. Prepare a new proposal.".into()),
        }
    }

    fn command_boundary(&self) -> Result<(), String> {
        // A hardlink already in the workspace can alias a file outside it.
        // Reject it before launching arbitrary code, including at execution.
        let mut stack = vec![(RelativePath::parse(".", true)?, 0usize)];
        let mut scanned = 0usize;
        while let Some((path, depth)) = stack.pop() {
            let directory = self.directory(&path)?;
            let (entries, more) =
                directory_entries(&directory, MAX_ENTRIES.saturating_sub(scanned))?;
            if more {
                return Err(
                    "Command refused: workspace exceeds the 4096-entry safety scan limit.".into(),
                );
            }
            for name in entries {
                scanned += 1;
                let Some(name) = name.to_str() else {
                    return Err("Command refused: workspace contains a non-UTF-8 name.".into());
                };
                if private_name(name) {
                    continue;
                }
                let child = RelativePath::parse(&path.path.join(name).to_string_lossy(), false)?;
                let stat = stat_at(&directory, &c_string(OsStr::new(name))?).map_err(io_error)?;
                match stat.st_mode & libc::S_IFMT {
                    libc::S_IFREG if stat.st_nlink > 1 => {
                        return Err(format!(
                            "Command refused: hardlinked file {}.",
                            child.display()
                        ));
                    }
                    libc::S_IFDIR => {
                        if depth >= MAX_SEARCH_DEPTH {
                            return Err(
                                "Command refused: workspace exceeds the safety scan depth.".into(),
                            );
                        }
                        stack.push((child, depth + 1));
                    }
                    libc::S_IFLNK => {
                        let target = self
                            .path
                            .join(&child.path)
                            .canonicalize()
                            .map_err(io_error)?;
                        let relative = target
                            .strip_prefix(&self.path)
                            .map_err(|_| "Command refused: symbolic link leaves the workspace.")?;
                        RelativePath::parse(&relative.to_string_lossy(), true)?;
                    }
                    libc::S_IFREG => {}
                    _ => {
                        return Err(
                            "Command refused: workspace contains a socket, device, or named pipe."
                                .into(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}

fn parse_arguments<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone())
        .map_err(|error| format!("Invalid tool arguments: {error}"))
}

fn bounded(value: usize, minimum: usize, maximum: usize, field: &str) -> Result<(), String> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} must be between {minimum} and {maximum}."))
    }
}

fn private_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(".env")
        || PRIVATE_NAMES.contains(&lower.as_str())
        || PRIVATE_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

fn truncate_utf8(text: &str, maximum: usize) -> &str {
    let mut end = maximum.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn c_string(value: &OsStr) -> Result<CString, String> {
    CString::new(value.as_bytes()).map_err(|_| "Path contains a NUL byte.".into())
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

fn open_directory(path: &Path) -> Result<File, String> {
    let path = c_string(path.as_os_str())?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io_error(std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_at(parent: &File, name: &CStr, flags: i32, mode: libc::mode_t) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn read_regular(mut file: File) -> std::io::Result<(Identity, Vec<u8>, u32)> {
    let before = file.metadata()?;
    if !before.is_file() || before.nlink() != 1 {
        return Err(std::io::Error::other(
            "Only regular files with one hardlink are accessible.",
        ));
    }
    if before.len() > MAX_FILE_BYTES as u64 {
        return Err(std::io::Error::other(
            "File exceeds the 256 KiB tool limit.",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > MAX_FILE_BYTES
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || after.nlink() != 1
    {
        return Err(std::io::Error::other("File changed while being read."));
    }
    Ok((
        Identity {
            device: before.dev(),
            inode: before.ino(),
        },
        bytes,
        before.mode(),
    ))
}

struct DirectoryCursor(*mut libc::DIR);
impl Drop for DirectoryCursor {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn directory_entries(
    directory: &File,
    maximum: usize,
) -> Result<(Vec<std::ffi::OsString>, bool), String> {
    let fresh =
        open_at(directory, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(io_error)?;
    let raw = fresh.as_raw_fd();
    let pointer = unsafe { libc::fdopendir(raw) };
    if pointer.is_null() {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    std::mem::forget(fresh); // fdopendir owns the descriptor after success.
    let cursor = DirectoryCursor(pointer);
    let mut entries = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let item = unsafe { libc::readdir(cursor.0) };
        if item.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(io_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*item).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if entries.len() == maximum {
            return Ok((entries, true));
        }
        entries.push(OsStr::from_bytes(name).to_owned());
    }
    entries.sort();
    Ok((entries, false))
}

fn stat_at(directory: &File, name: &CStr) -> std::io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { stat.assume_init() })
    }
}

fn entry_kind(directory: &File, name: &OsStr) -> Result<Option<String>, String> {
    let stat = stat_at(directory, &c_string(name)?).map_err(io_error)?;
    Ok(match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => Some("directory".into()),
        libc::S_IFREG if stat.st_nlink == 1 => Some("file".into()),
        _ => None,
    })
}

struct TemporaryFile {
    parent: File,
    name: CString,
    present: bool,
}
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.present {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

struct PrivateTemporary {
    path: PathBuf,
    parent: File,
    name: CString,
    identity: Identity,
}

impl PrivateTemporary {
    fn new() -> Result<Self, String> {
        let parent_path = std::env::temp_dir().canonicalize().map_err(io_error)?;
        let parent = open_directory(&parent_path)?;
        let name = CString::new(format!("taceta-command-{}", uuid::Uuid::new_v4()))
            .expect("UUID contains no NUL");
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let directory =
            open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(io_error)?;
        let identity = Identity::of(&directory)?;
        let path = parent_path.join(OsStr::from_bytes(name.to_bytes()));
        Ok(Self {
            path,
            parent,
            name,
            identity,
        })
    }
}

impl Drop for PrivateTemporary {
    fn drop(&mut self) {
        let Ok(directory) = open_at(
            &self.parent,
            &self.name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        ) else {
            return;
        };
        if Identity::of(&directory).ok() != Some(self.identity) {
            return;
        }
        let _ = remove_private_contents(&directory);
        unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            );
        }
    }
}

fn remove_private_contents(directory: &File) -> Result<(), String> {
    loop {
        let (entries, _) = directory_entries(directory, MAX_ENTRIES)?;
        if entries.is_empty() {
            return Ok(());
        }
        for name in entries {
            let name = c_string(&name)?;
            let stat = stat_at(directory, &name).map_err(io_error)?;
            let flags = if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = open_at(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                    .map_err(io_error)?;
                remove_private_contents(&child)?;
                libc::AT_REMOVEDIR
            } else {
                0
            };
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } != 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
        }
    }
}

fn sandbox_available() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        return Err(
            "run_command requires macOS sandbox-exec; no unsandboxed fallback is available.".into(),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let metadata = fs::metadata("/usr/bin/sandbox-exec")
            .map_err(|_| "Missing /usr/bin/sandbox-exec; command execution is unavailable.")?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(
                "/usr/bin/sandbox-exec is not executable; command execution is unavailable.".into(),
            );
        }
        Ok(())
    }
}

fn sandbox_profile(workspace: &Path, temporary: &Path) -> (String, Vec<(String, PathBuf)>, String) {
    let mut parameters = vec![
        ("WORKSPACE".into(), workspace.to_path_buf()),
        ("TEMPORARY".into(), temporary.to_path_buf()),
    ];
    let mut readable = vec![
        PathBuf::from("/System/Library"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/share"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/Library/Developer/CommandLineTools"),
        PathBuf::from("/Applications/Xcode.app/Contents/Developer"),
        PathBuf::from("/Library/Apple/System"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/opt/homebrew/Cellar"),
        PathBuf::from("/opt/homebrew/opt"),
        PathBuf::from("/opt/homebrew/lib"),
        PathBuf::from("/opt/homebrew/share"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/usr/local/Cellar"),
        PathBuf::from("/private/var/db/timezone"),
    ];
    let mut search_path = vec![
        "/usr/bin".to_owned(),
        "/bin".into(),
        "/usr/sbin".into(),
        "/sbin".into(),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
    ];
    // Direct compiler binaries avoid rustup's home settings and inherited hooks.
    if let Some(home) = std::env::var_os("HOME") {
        let toolchains = PathBuf::from(home).join(".rustup/toolchains");
        if let Ok(canonical) = toolchains.canonicalize()
            && canonical == toolchains
        {
            if let Ok(entries) = fs::read_dir(&canonical) {
                let mut bins: Vec<_> = entries
                    .take(64)
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let bin = entry.path().join("bin");
                        (name.starts_with("stable-")
                            && bin.join("cargo").is_file()
                            && bin.join("rustc").is_file())
                        .then_some(bin)
                    })
                    .collect();
                bins.sort();
                if let Some(bin) = bins.first() {
                    search_path.insert(0, bin.to_string_lossy().into_owned());
                }
            }
            readable.push(canonical);
        }
    }
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(import \"dyld-support.sb\")\n(allow process-exec process-fork)\n(allow signal (target self))\n(allow sysctl-read)\n(allow file-read* file-map-executable (subpath (param \"WORKSPACE\")) (subpath (param \"TEMPORARY\")))\n(allow file-write* (subpath (param \"WORKSPACE\")) (subpath (param \"TEMPORARY\")))\n(allow file-read* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/random\") (literal \"/dev/urandom\") (literal \"/private/etc/localtime\"))\n(allow file-write-data (literal \"/dev/null\"))\n",
    );
    for (index, path) in readable.iter().enumerate() {
        let key = format!("READ{index}");
        profile.push_str(&format!("(allow file-read* file-map-executable (subpath (param \"{key}\")))\n"));
        parameters.push((key, path.clone()));
    }
    let mut ancestors = BTreeSet::new();
    for path in [workspace, temporary] {
        for ancestor in path.ancestors().skip(1) {
            ancestors.insert(ancestor.to_path_buf());
        }
    }
    for (index, path) in ancestors.into_iter().enumerate() {
        let key = format!("ANCESTOR{index}");
        profile.push_str(&format!(
            "(allow file-read-metadata (literal (param \"{key}\")))\n"
        ));
        parameters.push((key, path));
    }
    // Seatbelt limits the length of individual string tokens. Emit the same
    // case-insensitive component policy as separate bounded regular expressions.
    for name in PRIVATE_NAMES {
        profile.push_str(&format!(
            "(deny file-read* file-write* (regex #\"/{}(/|$)\"))\n",
            regex_case_insensitive(name)
        ));
    }
    for suffix in PRIVATE_SUFFIXES {
        profile.push_str(&format!(
            "(deny file-read* file-write* (regex #\"/[^/]*{}(/|$)\"))\n",
            regex_case_insensitive(suffix)
        ));
    }
    profile.push_str(&format!(
        "(deny file-read* file-write* (regex #\"/{}[^/]*(/|$)\"))\n(deny network*)\n",
        regex_case_insensitive(".env")
    ));
    (profile, parameters, search_path.join(":"))
}

fn regex_case_insensitive(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphabetic() {
                format!(
                    "[{}{}]",
                    character.to_ascii_lowercase(),
                    character.to_ascii_uppercase()
                )
            } else if character == '.' {
                "[.]".into()
            } else {
                character.to_string()
            }
        })
        .collect()
}

struct RunningCommand {
    child: Child,
    group: i32,
    _temporary: PrivateTemporary,
}

impl RunningCommand {
    fn kill_group(&mut self) {
        if self.group > 0 {
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        self.kill_group();
    }
}

async fn run_command(
    root: Arc<Root>,
    command: FrozenCommand,
    mut cancel: watch::Receiver<bool>,
) -> Result<String, String> {
    sandbox_available()?;
    let directory = root.directory(&command.directory)?;
    if Identity::of(&directory)? != command.identity {
        return Err(
            "Command refused: working directory changed after approval was requested.".into(),
        );
    }
    root.command_boundary()?;
    let temporary = PrivateTemporary::new()?;
    let (profile, parameters, search_path) = sandbox_profile(&root.path, &temporary.path);
    let mut process = Command::new("/usr/bin/sandbox-exec");
    process.arg("-p").arg(profile);
    for (key, value) in parameters {
        let mut parameter = std::ffi::OsString::from(format!("{key}="));
        parameter.push(value);
        process.arg("-D").arg(parameter);
    }
    process
        .arg("/bin/sh")
        .arg("-c")
        .arg(&command.command)
        .env_clear()
        .env("PATH", search_path)
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env("HOME", &temporary.path)
        .env("TMPDIR", &temporary.path)
        .env("CARGO_HOME", temporary.path.join("cargo"))
        .env("CARGO_NET_OFFLINE", "true")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    root.check_directory(&directory, &root.path.join(&command.directory.path))?;
    // fchdir fixes the actual approved directory, rather than following a path
    // again between validation and spawning. Only async-signal-safe calls here.
    unsafe {
        process.pre_exec(move || {
            if libc::fchdir(directory.as_raw_fd()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::umask(0o077);
            Ok(())
        });
    }
    if *cancel.borrow() {
        return Err("Command cancelled before spawning.".into());
    }
    let child = process.spawn().map_err(|error| {
        format!("Sandbox process could not start; no unsandboxed fallback was attempted: {error}")
    })?;
    let group = child
        .id()
        .ok_or("Sandbox process did not provide a process id.")? as i32;
    let mut running = RunningCommand {
        child,
        group,
        _temporary: temporary,
    };
    let mut stdout = running
        .child
        .stdout
        .take()
        .ok_or("Sandbox stdout pipe is unavailable.")?;
    let mut stderr = running
        .child
        .stderr
        .take()
        .ok_or("Sandbox stderr pipe is unavailable.")?;
    let deadline = tokio::time::sleep(command.timeout);
    tokio::pin!(deadline);
    let drain_deadline = tokio::time::sleep(command.timeout + Duration::from_secs(2));
    tokio::pin!(drain_deadline);
    let mut draining = false;
    let mut out_buffer = [0u8; 8_192];
    let mut err_buffer = [0u8; 8_192];
    let mut output = Vec::new();
    let mut output_truncated = false;
    let mut out_open = true;
    let mut err_open = true;
    let mut status = None;
    let mut stopped = None;
    while status.is_none() || out_open || err_open {
        tokio::select! {
            biased;
            _ = cancel.changed(), if stopped.is_none() => {
                if *cancel.borrow() || cancel.has_changed().is_err() {
                    stopped = Some("cancelled");
                    running.kill_group();
                    drain_deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(2));
                    draining = true;
                }
            }
            _ = &mut deadline, if stopped.is_none() => {
                stopped = Some("timed out");
                running.kill_group();
                drain_deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(2));
                draining = true;
            }
            _ = &mut drain_deadline, if draining => {
                if status.is_none() {
                    return Err("Sandbox process did not report termination after SIGKILL; execution was abandoned.".into());
                }
                output_truncated = true;
                break;
            }
            result = running.child.wait(), if status.is_none() => {
                status = Some(result.map_err(|error| format!("Could not wait for sandbox process: {error}"))?);
                // A command is not allowed to leave background descendants behind.
                running.kill_group();
                if !draining {
                    drain_deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(2));
                    draining = true;
                }
            }
            result = stdout.read(&mut out_buffer), if out_open => {
                let count = result.map_err(io_error)?;
                out_open = count != 0;
                append_output(&mut output, &out_buffer[..count], &mut output_truncated);
            }
            result = stderr.read(&mut err_buffer), if err_open => {
                let count = result.map_err(io_error)?;
                err_open = count != 0;
                append_output(&mut output, &err_buffer[..count], &mut output_truncated);
            }
        }
    }
    let lossy = String::from_utf8_lossy(&output);
    let mut text = truncate_utf8(&lossy, MAX_RESULT_BYTES - 1_024).to_owned();
    output_truncated |= text.len() != lossy.len();
    if let Some(reason) = stopped {
        return Err(format!(
            "Command {reason}; its process group was terminated. Output{}:\n{text}",
            if output_truncated { " (truncated)" } else { "" }
        ));
    }
    let status = status.ok_or("Sandbox process ended without an exit status.")?;
    if !status.success()
        && (text.contains("sandbox-exec:")
            || text.contains("sandbox_apply")
            || text.contains("sandbox_init"))
    {
        return Err(format!(
            "Sandbox application failed; the command was not retried outside the sandbox:\n{text}"
        ));
    }
    loop {
        let result = json!({"exit_code":status.code(),"success":status.success(),"output":text,"truncated":output_truncated}).to_string();
        if result.len() <= MAX_RESULT_BYTES {
            return Ok(result);
        }
        text.truncate(truncate_utf8(&text, text.len() / 2).len());
        output_truncated = true;
    }
}

fn append_output(output: &mut Vec<u8>, incoming: &[u8], truncated: &mut bool) {
    let remaining = MAX_RESULT_BYTES.saturating_sub(output.len());
    let retained = remaining.min(incoming.len());
    output.extend_from_slice(&incoming[..retained]);
    *truncated |= retained < incoming.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};

    struct Fixture(PrivateTemporary);

    impl Fixture {
        fn new() -> Self {
            Self(PrivateTemporary::new().unwrap())
        }
        fn path(&self) -> &Path {
            &self.0.path
        }
        fn file(&self, name: &str) -> PathBuf {
            self.path().join(name)
        }
        fn write(&self, name: &str, bytes: impl AsRef<[u8]>) {
            let path = self.file(name);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path.parent().unwrap())
                .unwrap();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            file.write_all(bytes.as_ref()).unwrap();
        }
        fn tools(&self) -> WorkspaceTools {
            WorkspaceTools::new(self.path()).unwrap()
        }
    }

    fn call(name: &str, arguments: Value) -> AgentToolCall {
        AgentToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
        }
    }

    async fn execute(tool: PreparedTool) -> Result<String, String> {
        let (_sender, receiver) = watch::channel(false);
        tool.execute(receiver).await
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn paths_links_and_sensitive_components_never_escape_the_selected_workspace() {
        let workspace = Fixture::new();
        let outside = Fixture::new();
        outside.write("untouched.txt", "outside-only");
        workspace.write("AGENTS.md", "Local fixture instructions.");
        workspace.write("日本語 ' quoted \" \\ \n.rs", "unicode-path-content\n");
        workspace.write(".git/config", "private-git-fixture");
        workspace.write(".ENV.local", "private-env-fixture");
        symlink(outside.file("untouched.txt"), workspace.file("escape")).unwrap();
        symlink(outside.path(), workspace.file("escape-dir")).unwrap();
        fs::hard_link(outside.file("untouched.txt"), workspace.file("hardlink")).unwrap();
        let tools = workspace.tools();
        assert!(WorkspaceTools::new(Path::new("/")).is_err());
        assert!(WorkspaceTools::new(&std::env::temp_dir()).is_err());
        assert!(WorkspaceTools::new(Path::new(".")).is_err());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(WorkspaceTools::new(Path::new(&home)).is_err());
        }
        for path in [
            "../untouched.txt",
            "/etc/passwd",
            ".git/config",
            ".ENV.local",
            "nested/credentials.json",
            "identity.PEM",
        ] {
            assert!(
                tools
                    .prepare(&call("read_file", json!({"path":path})))
                    .is_err(),
                "{path}"
            );
        }
        for path in ["escape", "hardlink"] {
            let read = tools
                .prepare(&call("read_file", json!({"path":path})))
                .unwrap();
            assert!(execute(read).await.is_err());
            assert!(
                tools
                    .prepare(&call(
                        "edit_file",
                        json!({"path":path,"old_text":"outside-only","new_text":"changed"})
                    ))
                    .is_err()
            );
        }
        assert!(
            tools
                .prepare(&call("list_directory", json!({"path":"escape-dir"})))
                .is_err()
        );
        assert!(
            tools
                .prepare(&call("run_command", json!({"command":"true"})))
                .is_err()
        );
        let read = tools
            .prepare(&call(
                "read_file",
                json!({"path":"日本語 ' quoted \" \\ \n.rs"}),
            ))
            .unwrap();
        let result: Value = serde_json::from_str(&execute(read).await.unwrap()).unwrap();
        assert_eq!(result["text"], "unicode-path-content\n");
        let instructions = tools.instructions().unwrap();
        assert_eq!(
            instructions,
            vec![(
                workspace.file("AGENTS.md").to_string_lossy().into_owned(),
                "Local fixture instructions.".into()
            )]
        );
        let search = tools
            .prepare(&call("search_files", json!({"query":"outside-only"})))
            .unwrap();
        let result: Value = serde_json::from_str(&execute(search).await.unwrap()).unwrap();
        assert_eq!(result["matches"], json!([]));
        let listing = tools.prepare(&call("list_directory", json!({}))).unwrap();
        let result = execute(listing).await.unwrap();
        assert!(!result.contains(".git"));
        assert!(!result.contains(".ENV"));
        assert!(!result.contains("escape"));
        assert!(!result.contains("hardlink"));
        assert_eq!(
            fs::read(outside.file("untouched.txt")).unwrap(),
            b"outside-only"
        );
    }

    #[tokio::test]
    async fn proposals_freeze_original_bytes_and_execution_refuses_changed_files_or_parents() {
        let workspace = Fixture::new();
        workspace.write("nested/edit.txt", "before target after\n");
        let tools = workspace.tools();
        let edit_call = call(
            "edit_file",
            json!({"path":"nested/edit.txt","old_text":"target","new_text":"置換"}),
        );
        let edit = tools.prepare(&edit_call).unwrap();
        let (_, proposal) = edit.proposal().unwrap();
        let proposal: Value = serde_json::from_str(&proposal).unwrap();
        assert_eq!(proposal["before"], "before target after\n");
        assert_eq!(proposal["after"], "before 置換 after\n");
        assert_eq!(
            fs::read_to_string(workspace.file("nested/edit.txt")).unwrap(),
            "before target after\n"
        );
        workspace.write("nested/edit.txt", "user changed it\n");
        assert!(
            execute(edit)
                .await
                .unwrap_err()
                .contains("original file changed")
        );
        assert_eq!(
            fs::read_to_string(workspace.file("nested/edit.txt")).unwrap(),
            "user changed it\n"
        );

        workspace.write("nested/edit.txt", "before target after\n");
        let edit = tools.prepare(&edit_call).unwrap();
        fs::rename(workspace.file("nested"), workspace.file("moved")).unwrap();
        workspace.write("nested/edit.txt", "before target after\n");
        assert!(
            execute(edit)
                .await
                .unwrap_err()
                .contains("parent directory changed")
        );
        assert_eq!(
            fs::read_to_string(workspace.file("nested/edit.txt")).unwrap(),
            "before target after\n"
        );

        let edit = tools.prepare(&edit_call).unwrap();
        execute(edit).await.unwrap();
        assert_eq!(
            fs::read_to_string(workspace.file("nested/edit.txt")).unwrap(),
            "before 置換 after\n"
        );
        assert_eq!(
            fs::metadata(workspace.file("nested/edit.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(workspace.file("nested")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn edit_preconditions_new_files_and_raced_links_fail_without_mutation() {
        let workspace = Fixture::new();
        let outside = Fixture::new();
        workspace.write("edit.txt", "one one");
        outside.write("outside.txt", "one one");
        let tools = workspace.tools();
        for args in [
            json!({"path":"edit.txt","old_text":"one","new_text":"two"}),
            json!({"path":"edit.txt","old_text":"","new_text":"two"}),
            json!({"path":"edit.txt","old_text":"missing","new_text":"two"}),
            json!({"path":"edit.txt","old_text":null,"new_text":"two"}),
            json!({"path":"missing.txt","old_text":"one","new_text":"two"}),
            json!({"path":"new.txt","new_text":"two"}),
        ] {
            assert!(tools.prepare(&call("edit_file", args)).is_err());
        }
        let edit = tools
            .prepare(&call(
                "edit_file",
                json!({"path":"edit.txt","old_text":"one one","new_text":"two"}),
            ))
            .unwrap();
        fs::remove_file(workspace.file("edit.txt")).unwrap();
        symlink(outside.file("outside.txt"), workspace.file("edit.txt")).unwrap();
        assert!(execute(edit).await.is_err());
        assert_eq!(fs::read(outside.file("outside.txt")).unwrap(), b"one one");
        assert!(
            fs::symlink_metadata(workspace.file("edit.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let create = || {
            call(
                "edit_file",
                json!({"path":"new.txt","old_text":null,"new_text":"new content"}),
            )
        };
        let edit = tools.prepare(&create()).unwrap();
        workspace.write("new.txt", "appeared while pending");
        assert!(execute(edit).await.is_err());
        assert_eq!(
            fs::read(workspace.file("new.txt")).unwrap(),
            b"appeared while pending"
        );
        fs::remove_file(workspace.file("new.txt")).unwrap();
        execute(tools.prepare(&create()).unwrap()).await.unwrap();
        assert_eq!(fs::read(workspace.file("new.txt")).unwrap(), b"new content");
        assert_eq!(fs::metadata(workspace.file("new.txt")).unwrap().nlink(), 1);
    }

    #[test]
    fn malformed_and_unbounded_calls_are_rejected_before_any_effect() {
        let workspace = Fixture::new();
        let tools = workspace.tools();
        for invalid in [
            call("unknown", json!({})),
            call("read_file", json!({"path":"x","extra":true})),
            call("read_file", json!({"path":"x","start_line":0})),
            call("read_file", json!({"path":"x","max_lines":1001})),
            call("list_directory", json!({"max_entries":0})),
            call("search_files", json!({"query":""})),
            call("search_files", json!({"query":"x","max_results":101})),
            call("run_command", json!({"command":"true","timeout_secs":1})),
            call("run_command", json!({"command":"true","timeout_ms":120001})),
            call("run_command", json!({"command":" \n"})),
            call(
                "edit_file",
                json!({"path":"x","old_text":null,"new_text":12}),
            ),
            call(
                "edit_file",
                json!({"path":"x","old_text":null,"new_text":"x","replace_all":true}),
            ),
            call("read_file", json!([])),
        ] {
            assert!(
                tools.prepare(&invalid).is_err(),
                "{}: {}",
                invalid.name,
                invalid.arguments
            );
        }
        assert_eq!(fs::read_dir(workspace.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn read_search_and_listing_limits_bound_serialized_results() {
        let workspace = Fixture::new();
        workspace.write("small.txt", "first\nneedle 日本語\nlast\n");
        workspace.write("large.txt", vec![b'x'; MAX_FILE_BYTES + 1]);
        workspace.write("long-line.txt", "\u{0001}".repeat(MAX_FILE_BYTES));
        workspace.write(".env", "needle must never be returned");
        let tools = workspace.tools();
        let read = tools
            .prepare(&call(
                "read_file",
                json!({"path":"small.txt","start_line":2,"max_lines":1}),
            ))
            .unwrap();
        let value: Value = serde_json::from_str(&execute(read).await.unwrap()).unwrap();
        assert_eq!(value["text"], "needle 日本語\n");
        assert_eq!(value["truncated"], true);
        let read = tools
            .prepare(&call("read_file", json!({"path":"large.txt"})))
            .unwrap();
        assert!(execute(read).await.is_err());
        let read = tools
            .prepare(&call("read_file", json!({"path":"long-line.txt"})))
            .unwrap();
        let response = execute(read).await.unwrap();
        assert!(response.len() <= MAX_RESULT_BYTES);
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["truncated"], true);
        assert!(!value["text"].as_str().unwrap().is_empty());
        let search = tools
            .prepare(&call("search_files", json!({"query":"needle"})))
            .unwrap();
        let value: Value = serde_json::from_str(&execute(search).await.unwrap()).unwrap();
        assert_eq!(
            value["matches"],
            json!([{"path":"small.txt","line":2,"text":"needle 日本語"}])
        );
        assert_eq!(value["truncated"], true);
        let listing = tools
            .prepare(&call("list_directory", json!({"max_entries":1})))
            .unwrap();
        let value: Value = serde_json::from_str(&execute(listing).await.unwrap()).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 1);
        assert_eq!(value["truncated"], true);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn real_sandbox_allows_workspace_and_denies_network_private_paths_and_outside_writes() {
        let workspace = Fixture::new();
        let outside = Fixture::new();
        let secret = "__private_fixture_content_must_not_escape__";
        workspace.write(".git/config", secret);
        workspace.write(".ENV.local", secret);
        workspace.write("identity.pem", secret);
        outside.write("outside.txt", secret);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let command = format!(
            "printf workspace-ok > allowed.txt; printf '%s' \"$HOME\" > temporary.path; /usr/bin/env; \
             /bin/cat {} .git/config .ENV.local identity.pem; \
             printf changed > {}; printf 'outside_status=%s\\n' \"$?\"; \
             /bin/ln {} forbidden-hardlink; printf 'link_status=%s\\n' \"$?\"; \
             /usr/bin/curl --silent --show-error --max-time 1 http://127.0.0.1:{port}/; printf 'network_status=%s\\n' \"$?\"; \
             printf changed > .git/config; printf 'git_status=%s\\n' \"$?\"",
            shell_quote(&outside.file("outside.txt")),
            shell_quote(&outside.file("outside.txt")),
            shell_quote(&outside.file("outside.txt"))
        );
        let prepared = workspace
            .tools()
            .prepare(&call(
                "run_command",
                json!({"command":command,"timeout_ms":5000}),
            ))
            .unwrap();
        let (_, proposal) = prepared.proposal().unwrap();
        let proposal: Value = serde_json::from_str(&proposal).unwrap();
        assert_eq!(proposal["command"], command);
        let response = execute(prepared)
            .await
            .expect("macOS sandbox must apply for command runtime acceptance");
        let response: Value = serde_json::from_str(&response).unwrap();
        let output = response["output"].as_str().unwrap();
        assert!(!output.contains(secret));
        assert!(!output.contains("outside_status=0"), "{output}");
        assert!(!output.contains("link_status=0"), "{output}");
        assert!(!output.contains("network_status=0"), "{output}");
        assert!(!output.contains("git_status=0"), "{output}");
        assert!(!output.contains("SSH_AUTH_SOCK="));
        assert!(!output.contains("OPENAI_API_KEY="));
        assert!(!output.contains("XAI_API_KEY="));
        assert!(!output.contains("DYLD_INSERT_LIBRARIES="));
        assert_eq!(
            fs::read(workspace.file("allowed.txt")).unwrap(),
            b"workspace-ok"
        );
        assert_eq!(
            fs::read(outside.file("outside.txt")).unwrap(),
            secret.as_bytes()
        );
        assert_eq!(
            fs::read(workspace.file(".git/config")).unwrap(),
            secret.as_bytes()
        );
        assert!(!workspace.file("forbidden-hardlink").exists());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        let temporary = fs::read_to_string(workspace.file("temporary.path")).unwrap();
        assert!(
            !Path::new(&temporary).exists(),
            "command temporary directory was not removed"
        );
        assert_eq!(
            fs::metadata(workspace.file("allowed.txt")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn real_sandbox_drains_saturated_stdout_and_stderr_with_bounded_output() {
        let workspace = Fixture::new();
        let prepared = workspace.tools().prepare(&call("run_command", json!({
            "command":"(/usr/bin/yes stdout | /usr/bin/head -c 400000) & (/usr/bin/yes stderr | /usr/bin/head -c 400000 >&2) & wait",
            "timeout_ms":5000
        }))).unwrap();
        let response = execute(prepared)
            .await
            .expect("sandbox output draining must complete");
        assert!(response.len() <= MAX_RESULT_BYTES);
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["truncated"], true, "{response}");
        assert_eq!(response["success"], true, "{response}");
        assert!(!response["output"].as_str().unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    async fn wait_for_pid(
        workspace: &Fixture,
        task: &tokio::task::JoinHandle<Result<String, String>>,
    ) -> i32 {
        for _ in 0..100 {
            if let Ok(text) = fs::read_to_string(workspace.file("child.pid"))
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            if task.is_finished() {
                panic!("Sandbox ended before starting the controlled child process.");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("Sandbox did not start the controlled child process within two seconds.");
    }

    #[cfg(target_os = "macos")]
    async fn assert_process_gone(pid: i32) {
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("Controlled child process {pid} remained after command termination.");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn real_sandbox_cancel_timeout_and_future_drop_terminate_the_process_group() {
        for mode in ["cancel", "timeout", "drop"] {
            let workspace = Fixture::new();
            let prepared = workspace.tools().prepare(&call("run_command", json!({
                "command":"/bin/sh -c 'while :; do printf tick >> heartbeat; /bin/sleep 0.05; done' & printf '%s' \"$!\" > child.pid; printf '%s' \"$TMPDIR\" > temporary.path; wait",
                "timeout_ms":if mode == "timeout" { 700 } else { 5000 }
            }))).unwrap();
            let (sender, receiver) = watch::channel(false);
            let task = tokio::spawn(prepared.execute(receiver));
            let child = wait_for_pid(&workspace, &task).await;
            let temporary = fs::read_to_string(workspace.file("temporary.path")).unwrap();
            match mode {
                "cancel" => {
                    sender.send(true).unwrap();
                    let error = task.await.unwrap().unwrap_err();
                    assert!(error.contains("cancelled"), "{error}");
                }
                "timeout" => {
                    let error = task.await.unwrap().unwrap_err();
                    assert!(error.contains("timed out"), "{error}");
                }
                "drop" => {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                }
                _ => unreachable!(),
            }
            assert_process_gone(child).await;
            let bytes = fs::read(workspace.file("heartbeat")).unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                fs::read(workspace.file("heartbeat")).unwrap_or_default(),
                bytes
            );
            assert!(
                !Path::new(&temporary).exists(),
                "temporary directory remained after {mode}"
            );
        }
    }
}
