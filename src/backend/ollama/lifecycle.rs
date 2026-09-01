use crate::backend::BackendError;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{sleep, timeout},
};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";
const READY_DEADLINE: Duration = Duration::from_secs(10);
static START_LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();

pub(super) async fn ensure_ready(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<(), BackendError> {
    if is_reachable(http, base_url).await? {
        return Ok(());
    }
    if base_url != DEFAULT_BASE_URL {
        return Err(BackendError::OllamaUnavailable);
    }
    let lock = {
        let locks = START_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut locks = locks.lock().expect("Ollama start lock poisoned");
        locks
            .entry(base_url.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    if is_reachable(http, base_url).await? {
        return Ok(());
    }
    let binary = discover_ollama().ok_or(BackendError::OllamaBinaryMissing)?;
    Command::new(binary)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| BackendError::OllamaStartFailed(e.to_string()))?;
    match timeout(READY_DEADLINE, async {
        loop {
            if is_reachable(http, base_url).await? {
                return Ok::<(), BackendError>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(BackendError::OllamaReadinessTimeout),
    }
}

async fn is_reachable(http: &reqwest::Client, base_url: &str) -> Result<bool, BackendError> {
    match http.get(format!("{base_url}/api/version")).send().await {
        Ok(_) => Ok(true),
        Err(error) if error.is_connect() => Ok(false),
        Err(error) => Err(BackendError::Request(error)),
    }
}

pub(super) fn discover_ollama() -> Option<PathBuf> {
    discover_ollama_from(std::env::var_os("PATH").as_deref(), |path| path.is_file())
}
fn discover_ollama_from<F>(path_var: Option<&std::ffi::OsStr>, is_file: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let mut candidates = vec![
        PathBuf::from("/Applications/Ollama.app/Contents/Resources/ollama"),
        PathBuf::from("/usr/local/bin/ollama"),
        PathBuf::from("/opt/homebrew/bin/ollama"),
    ];
    if let Some(path_var) = path_var {
        candidates.extend(std::env::split_paths(path_var).map(|dir| dir.join("ollama")));
    }
    candidates.into_iter().find(|path| is_file(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_prefers_macos_locations_then_path() {
        let found = discover_ollama_from(Some(std::ffi::OsStr::new("/path/bin")), |path| {
            path == Path::new("/usr/local/bin/ollama") || path == Path::new("/path/bin/ollama")
        });
        assert_eq!(found, Some(PathBuf::from("/usr/local/bin/ollama")));
    }
    #[test]
    fn missing_binary_is_reported_by_pure_discovery() {
        assert_eq!(
            discover_ollama_from(Some(std::ffi::OsStr::new("/none")), |_| false),
            None
        );
    }
}
