mod grok;
mod ollama;

use crate::domain::{
    ChatRequest, GenerationEvent, ModelCandidate, ModelDescriptor, ModelManagerEvent,
    ModelPullRequest,
};
use std::{future::Future, pin::Pin};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Grok: {0}")]
    Grok(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid Ollama response: {0}")]
    Protocol(String),
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
}

pub use grok::{GrokClient, GrokLoginEvent};

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
    DEFAULT_OLLAMA_BASE_URL, OllamaClient, OllamaEndpoint, OllamaEndpointError, OllamaEndpointMode,
    OllamaEndpointSource, OllamaModelManager,
};
