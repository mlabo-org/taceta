mod ollama;

use crate::domain::{ChatRequest, GenerationEvent, ModelDescriptor};
use std::{future::Future, pin::Pin};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid Ollama response: {0}")]
    Protocol(String),
    #[error("generation cancelled")]
    Cancelled,
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

pub use ollama::OllamaClient;
