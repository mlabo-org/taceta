mod api;
mod capability;
mod client;
mod endpoint;
mod lifecycle;
mod router;
mod stream;

pub use client::{OllamaClient, OllamaModelManager};
pub use endpoint::{
    DEFAULT_OLLAMA_BASE_URL, OllamaEndpoint, OllamaEndpointError, OllamaEndpointMode,
    OllamaEndpointSource,
};
