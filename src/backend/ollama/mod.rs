mod api;
mod capability;
mod client;
mod endpoint;
mod lifecycle;
mod memory;
mod router;
mod stream;
mod web_tools;

pub use client::{OllamaAgentWeb, OllamaClient, OllamaModelManager};
pub use endpoint::{
    DEFAULT_OLLAMA_BASE_URL, OllamaEndpoint, OllamaEndpointError, OllamaEndpointMode,
    OllamaEndpointSource,
};
