use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path, pin::Pin, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, timeout},
};

#[derive(Debug)]
pub(super) enum WireMessage {
    Response { id: Value, result: Result<Value, String> },
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

fn decode(line: &str) -> Result<WireMessage, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| "Codex App Server returned invalid JSON.".to_owned())?;
    let object = value.as_object().ok_or("Codex App Server returned a non-object message.")?;
    let id = object.get("id").cloned();
    if let Some(id) = &id {
        if !id.is_string() && !id.is_number() {
            return Err("Codex App Server returned an invalid request ID.".into());
        }
    }
    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        return Ok(match id {
            Some(id) => WireMessage::Request { id, method: method.into(), params },
            None => WireMessage::Notification { method: method.into(), params },
        });
    }
    let id = id.ok_or("Codex App Server response has no request ID.")?;
    let result = if let Some(error) = object.get("error") {
        Err(error.get("message").and_then(Value::as_str)
            .unwrap_or("Codex App Server rejected the request.").to_owned())
    } else {
        Ok(object.get("result").cloned().ok_or("Codex App Server response has no result.")?)
    };
    Ok(WireMessage::Response { id, result })
}

/// Owns one process group. Dropping an abandoned login/run never leaves its
/// callback server or tool processes behind, and never targets another Codex.
struct OwnedProcess {
    child: Child,
    group: Option<i32>,
}

impl OwnedProcess {
    fn kill(&mut self) {
        if let Some(group) = self.group.take() {
            // The child created this process group with process_group(0).
            unsafe { libc::kill(-group, libc::SIGKILL); }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) { self.kill(); }
}

pub(super) struct Connection {
    process: Option<OwnedProcess>,
    writer: Pin<Box<dyn AsyncWrite + Send>>,
    incoming: mpsc::UnboundedReceiver<Result<WireMessage, String>>,
    reader: JoinHandle<()>,
    deferred: VecDeque<WireMessage>,
    sequence: u64,
}

impl Connection {
    pub async fn spawn(executable: &Path, home: &Path, cwd: &Path, chat: bool) -> Result<Self, String> {
        let mut command = Command::new(executable);
        // Do not inherit API credentials, proxy/provider routing, remote host
        // settings, or another Codex client's private execution environment.
        command.env_clear();
        for name in ["HOME", "PATH", "TMPDIR", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE"] {
            if let Some(value) = std::env::var_os(name) { command.env(name, value); }
        }
        command.arg("app-server").arg("--listen").arg("stdio://")
            .env("CODEX_HOME", home)
            .current_dir(cwd)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .kill_on_drop(true);
        // These are process-local overrides, never writes to the user's Codex config.
        for setting in [
            "cli_auth_credentials_store=\"keyring\"", "forced_login_method=\"chatgpt\"",
            "model_provider=\"openai\"", "web_search=\"disabled\"", "allow_login_shell=false",
            "features.hooks=false", "features.plugin_hooks=false", "features.apps=false",
            "features.plugins=false", "features.remote_plugin=false",
            "features.shell_snapshot=false", "features.shell_snapshot_v2=false",
        ] { command.arg("-c").arg(setting); }
        if chat {
            for setting in ["features.shell_tool=false", "features.multi_agent=false", "features.multi_agent_v2=false"] {
                command.arg("-c").arg(setting);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().map_err(|error| format!("Could not start Codex App Server: {error}"))?;
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        let input = child.stdin.take().ok_or("Codex App Server stdin is unavailable.")?;
        let output = child.stdout.take().ok_or("Codex App Server stdout is unavailable.")?;
        let mut connection = Self::from_io(output, input);
        connection.process = Some(OwnedProcess { child, group });
        Ok(connection)
    }

    pub(super) fn from_io<R, W>(read: R, write: W) -> Self
    where R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Send + 'static {
        let (sender, incoming) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut read = BufReader::new(read);
            let mut line = String::new();
            loop {
                line.clear();
                match read.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) if line.trim().is_empty() => continue,
                    Ok(_) => {
                        let message = decode(&line);
                        let failed = message.is_err();
                        if sender.send(message).is_err() || failed { break; }
                    }
                    Err(error) => { let _ = sender.send(Err(format!("Codex connection failed: {error}"))); break; }
                }
            }
        });
        Self { process: None, writer: Box::pin(write), incoming, reader, deferred: VecDeque::new(), sequence: 0 }
    }

    async fn write(&mut self, value: Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        timeout(Duration::from_secs(3), async {
            self.writer.write_all(&bytes).await.map_err(|error| format!("Could not write to Codex: {error}"))?;
            self.writer.flush().await.map_err(|error| format!("Could not flush Codex request: {error}"))
        }).await.map_err(|_| "Writing to Codex timed out; the request's effect is unknown and was not retried.".to_owned())?
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.sequence += 1;
        let id = json!(format!("taceta-{}", self.sequence));
        self.write(json!({"id":id,"method":method,"params":params})).await?;
        Ok(id)
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({"method":method,"params":params})).await
    }

    pub async fn respond(&mut self, id: Value, result: Value) -> Result<(), String> {
        self.write(json!({"id":id,"result":result})).await
    }

    pub async fn reject(&mut self, id: Value, message: &str) -> Result<(), String> {
        self.write(json!({"id":id,"error":{"code":-32601,"message":message}})).await
    }

    async fn receive(&mut self) -> Result<WireMessage, String> {
        self.incoming.recv().await.ok_or_else(|| "Codex App Server closed the connection before completion.".to_owned())?
    }

    pub async fn next(&mut self) -> Result<WireMessage, String> {
        if let Some(message) = self.deferred.pop_front() { return Ok(message); }
        self.receive().await
    }

    /// Notifications arriving before an RPC response are retained in wire order.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.request(method, params).await?;
        timeout(Duration::from_secs(30), async {
            loop {
                match self.receive().await? {
                    WireMessage::Response { id: response_id, result } if response_id == id => return result,
                    message @ WireMessage::Notification { .. } => self.deferred.push_back(message),
                    WireMessage::Request { id, method, .. } => {
                        self.reject(id, "Taceta cannot handle this request during connection setup.").await?;
                        return Err(format!("Unexpected Codex request during setup: {method}"));
                    }
                    WireMessage::Response { .. } => return Err("Codex returned a response for an unknown request.".into()),
                }
            }
        }).await.map_err(|_| format!("Codex {method} timed out."))?
    }

    pub async fn initialize(&mut self) -> Result<(), String> {
        self.call("initialize", json!({
            "clientInfo":{"name":"taceta","title":"Taceta","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}
        })).await?;
        self.notify("initialized", json!({})).await
    }

    pub async fn shutdown(&mut self) {
        if let Some(process) = &mut self.process {
            process.kill();
            let _ = timeout(Duration::from_secs(2), process.child.wait()).await;
        }
        self.reader.abort();
    }
}

impl Drop for Connection {
    fn drop(&mut self) { self.reader.abort(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};

    #[tokio::test]
    async fn gpt_jsonl_correlates_rpc_and_retains_early_notifications() {
        let (client, server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::from_io(read, write);
        let peer = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            write.write_all(b"{\"method\":\"item/agentMessage/delta\",\"params\":{\"delta\":\"").await.unwrap();
            write.write_all("日本語\"}}\n".as_bytes()).await.unwrap();
            write.write_all(format!("{}\n", json!({"id":request["id"],"result":{"ok":true}})).as_bytes()).await.unwrap();
        });
        assert_eq!(connection.call("model/list", json!({})).await.unwrap(), json!({"ok":true}));
        assert!(matches!(connection.next().await.unwrap(), WireMessage::Notification { params, .. } if params["delta"] == "日本語"));
        assert!(connection.next().await.is_err());
        peer.await.unwrap();
    }

    #[test]
    fn gpt_wire_ids_and_protocol_errors_are_not_silently_coerced() {
        assert!(matches!(decode(r#"{"id":"server-1","method":"approve","params":{}}"#).unwrap(), WireMessage::Request { id, .. } if id == "server-1"));
        assert!(matches!(decode(r#"{"id":7,"error":{"message":"denied"}}"#).unwrap(), WireMessage::Response { result: Err(message), .. } if message == "denied"));
        assert!(decode(r#"{"id":{},"result":{}}"#).is_err());
        assert!(decode("not json").is_err());
    }
}
