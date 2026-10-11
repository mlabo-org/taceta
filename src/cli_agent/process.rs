//! A CLI child process that speaks one JSON value per line on stdio.

use serde_json::Value;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
};

/// Finds `name` on PATH and then in `fallback_dirs`. An app started from
/// Finder gets a minimal PATH, so the places these CLIs install themselves
/// are searched explicitly.
pub fn find_executable(name: &str, fallback_dirs: &[PathBuf]) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .iter()
        .chain(fallback_dirs)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Common install locations under the user's home directory and Homebrew.
pub fn usual_install_dirs() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        directories.push(home.join(".local/bin"));
        directories.push(home.join(".grok/bin"));
        directories.push(home.join(".claude/local"));
    }
    directories.push(PathBuf::from("/opt/homebrew/bin"));
    directories.push(PathBuf::from("/usr/local/bin"));
    directories
}

/// Owns the child's process group. Stopping or dropping it ends the CLI and
/// every tool process it started, and never touches another process.
pub struct CliProcess {
    child: Child,
    group: Option<i32>,
    stdin: Option<ChildStdin>,
    lines: mpsc::UnboundedReceiver<Result<Value, String>>,
    reader: JoinHandle<()>,
}

impl CliProcess {
    /// Starts `executable` in `cwd` with the user's own environment, so the
    /// CLI finds its normal login and settings.
    pub fn spawn(executable: &Path, args: &[OsString], cwd: &Path) -> Result<Self, String> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start {}: {error}", executable.display()))?;
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        let stdin = child.stdin.take().ok_or("The CLI's input is unavailable.")?;
        let stdout = child.stdout.take().ok_or("The CLI's output is unavailable.")?;
        let (sender, lines) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut output = BufReader::new(stdout).lines();
            loop {
                let message = match output.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => serde_json::from_str::<Value>(&line)
                        .map_err(|_| "The CLI wrote a line that is not JSON.".to_owned()),
                    Ok(None) => break,
                    Err(error) => Err(format!("Reading the CLI's output failed: {error}")),
                };
                let failed = message.is_err();
                if sender.send(message).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self { child, group, stdin: Some(stdin), lines, reader })
    }

    pub async fn send(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("The CLI's input was already closed.")?;
        let mut line = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|error| format!("Writing to the CLI failed: {error}"))?;
        stdin.flush().await.map_err(|error| format!("Writing to the CLI failed: {error}"))
    }

    /// The next message, or `None` once the CLI has closed its output.
    pub async fn next(&mut self) -> Option<Result<Value, String>> {
        self.lines.recv().await
    }

    /// Closes the CLI's input, which is how these CLIs are told to finish.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    fn kill(&mut self) {
        if let Some(group) = self.group.take() {
            // The child created this group itself with process_group(0).
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    /// Ends the CLI and its tool processes, and waits for it to be gone.
    pub async fn shutdown(mut self) {
        self.close_input();
        self.kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        self.reader.abort();
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn script(root: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("fake-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[tokio::test]
    async fn lines_are_exchanged_as_json_and_a_closed_output_ends_the_stream() {
        let root = tempfile::tempdir().unwrap();
        let cli = script(root.path(), r#"read line; printf '%s\n\n' "$line"; printf '{"done":true}\n'"#);
        let mut process = CliProcess::spawn(&cli, &[], root.path()).unwrap();
        process.send(&json!({"hello": "日本語"})).await.unwrap();
        assert_eq!(process.next().await, Some(Ok(json!({"hello": "日本語"}))));
        assert_eq!(process.next().await, Some(Ok(json!({"done": true}))));
        assert_eq!(process.next().await, None);
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_an_error_not_a_skipped_message() {
        let root = tempfile::tempdir().unwrap();
        let cli = script(root.path(), "echo 'not json'; echo '{}'");
        let mut process = CliProcess::spawn(&cli, &[], root.path()).unwrap();
        assert!(process.next().await.unwrap().is_err());
        assert_eq!(process.next().await, None);
    }

    #[tokio::test]
    async fn shutdown_ends_the_cli_and_the_processes_it_started() {
        let root = tempfile::tempdir().unwrap();
        let cli = script(root.path(), r#"sleep 300 & echo "{\"child\":$!}"; wait"#);
        let mut process = CliProcess::spawn(&cli, &[], root.path()).unwrap();
        let child = process.next().await.unwrap().unwrap()["child"].as_i64().unwrap() as i32;
        assert_eq!(unsafe { libc::kill(child, 0) }, 0);
        process.shutdown().await;
        for _ in 0..50 {
            if unsafe { libc::kill(child, 0) } != 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the CLI's child process outlived shutdown");
    }

    #[test]
    fn an_executable_is_found_in_a_fallback_directory_and_plain_files_are_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let name = format!("taceta-test-cli-{}", std::process::id());
        let path = root.path().join(&name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(find_executable(&name, &[root.path().to_path_buf()]), None);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(find_executable(&name, &[root.path().to_path_buf()]), Some(path));
    }
}
