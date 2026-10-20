mod ollama;

use crate::domain::{
    ChatRequest, GenerationEvent, ModelCandidate, ModelDescriptor, ModelManagerEvent,
    ModelPullRequest,
};
use std::{future::Future, pin::Pin};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid Ollama response: {0}")]
    Protocol(String),
    /// Web Search, page fetch, or Taceta Link failed. The local inference
    /// connection is not implicated, and no answer may be kept for the turn.
    #[error("{0}")]
    Web(String),
    #[error("generation cancelled")]
    Cancelled,
    #[error("Ollama endpoint is unavailable")]
    OllamaUnavailable,
    #[error("Ollama executable was not found in the standard macOS locations or PATH")]
    OllamaBinaryMissing,
    #[error("failed to start Ollama: {0}")]
    OllamaStartFailed(String),
    #[error("Ollama did not become ready within 10 seconds")]
    OllamaReadinessTimeout,
    /// Ollama relayed the request to its cloud and the cloud refused it:
    /// 401 when the Mac is not signed in to Ollama, 403 when the model is
    /// not in the plan or cloud features are turned off. The local Ollama
    /// server itself answered and is reachable.
    #[error("Ollama's cloud refused the request ({status}): {message}")]
    Cloud { status: u16, message: String },
}

pub type BackendFuture<T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send>>;

pub trait InferenceBackend: Send + Sync {
    fn list_models(&self) -> BackendFuture<Vec<ModelDescriptor>>;
    fn stream_chat(
        &self,
        request: ChatRequest,
        events: UnboundedSender<GenerationEvent>,
    ) -> BackendFuture<()>;
}

/// Model lifecycle is deliberately separate from chat inference. Dropping the
/// returned future cancels an in-flight pull by closing its HTTP stream.
pub trait ModelManager: Send + Sync {
    fn list_installed(&self) -> BackendFuture<Vec<ModelDescriptor>>;
    fn list_available(&self, model: String) -> BackendFuture<Vec<ModelCandidate>>;
    fn pull(
        &self,
        request: ModelPullRequest,
        events: UnboundedSender<ModelManagerEvent>,
    ) -> BackendFuture<()>;
    fn delete(&self, model: String) -> BackendFuture<()>;
    /// Unload all currently loaded models without deleting files; return their count.
    fn unload_all(&self) -> BackendFuture<usize>;
}

pub use ollama::{
    DEFAULT_OLLAMA_BASE_URL, OllamaAgentWeb, OllamaClient, OllamaEndpoint, OllamaEndpointError, OllamaEndpointMode,
    OllamaEndpointSource, OllamaModelManager,
};
