//! Inference model-ID admission copied from MIT grok-codex-bridge src/catalog.rs.
//! Persistent bridge catalog/cache and Native routing are not Taceta responsibilities.
use std::collections::HashSet;
use serde::Serialize;
use thiserror::Error;

pub(crate) fn validate_model_ids<I, S>(models: I) -> Result<Vec<String>, CatalogError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Ok(validate_models(models)?
        .into_iter()
        .map(|model| model.id)
        .collect())
}

fn validate_models<I, S>(models: I) -> Result<Vec<ModelObject>, CatalogError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut seen = HashSet::new();
    let mut admitted = Vec::new();

    for model in models {
        let id = model.into();
        if !(6..=128).contains(&id.len())
            || !id.starts_with("grok-")
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(CatalogError::InvalidModelId);
        }
        if !seen.insert(id.clone()) {
            return Err(CatalogError::DuplicateModelId);
        }
        admitted.push(ModelObject {
            id,
            object: "model",
            owned_by: "xai",
        });
    }

    if admitted.is_empty() {
        return Err(CatalogError::EmptyCatalog);
    }
    Ok(admitted)
}

#[derive(Clone, Debug, Serialize)]
struct ModelObject {
    id: String,
    object: &'static str,
    owned_by: &'static str,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("model catalog must contain at least one admitted model")]
    EmptyCatalog,
    #[error("model catalog contains an invalid Grok model identifier")]
    InvalidModelId,
    #[error("model catalog contains a duplicate model identifier")]
    DuplicateModelId,
}
