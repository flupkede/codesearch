use super::{active_config, ModelType};
use anyhow::{bail, Result};
use std::path::Path;

/// Local models retain historical routing; remote uses only the active strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EmbeddingModel {
    Local(ModelType),
    ActiveRemote,
}

impl From<ModelType> for EmbeddingModel {
    fn from(value: ModelType) -> Self {
        Self::Local(value)
    }
}
impl Default for EmbeddingModel {
    fn default() -> Self {
        if active_config().remote() {
            Self::ActiveRemote
        } else {
            Self::Local(ModelType::default())
        }
    }
}
impl EmbeddingModel {
    pub fn parse(name: &str) -> Option<Self> {
        if active_config().remote() {
            (name == active_config().model).then_some(Self::ActiveRemote)
        } else {
            ModelType::parse(name).map(Self::Local)
        }
    }
    pub fn valid_short_names() -> String {
        if active_config().remote() {
            active_config().model.clone()
        } else {
            ModelType::valid_short_names()
        }
    }
    pub fn short_name(&self) -> &str {
        match self {
            Self::Local(model) => model.short_name(),
            Self::ActiveRemote => &active_config().model,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Local(model) => model.name(),
            Self::ActiveRemote => &active_config().model,
        }
    }
    pub fn try_dimensions(&self) -> Result<usize> {
        match self {
            Self::Local(model) => Ok(model.dimensions()),
            Self::ActiveRemote => super::openai::active_dimensions(),
        }
    }
    pub fn dimensions(&self) -> usize {
        match self {
            Self::Local(model) => model.dimensions(),
            // configure() resolves this once and fails before executing commands.
            Self::ActiveRemote => {
                super::openai::known_dimensions().expect("active remote strategy initialized")
            }
        }
    }
    pub fn is_heavier_than_default(&self) -> bool {
        match self {
            Self::Local(model) => model.is_heavier_than_default(),
            Self::ActiveRemote => false,
        }
    }
    pub fn cache_identity(&self) -> Result<String> {
        match self {
            Self::Local(model) => Ok(model.short_name().to_owned()),
            Self::ActiveRemote => Ok(format!(
                "{}-{}",
                active_config().fingerprint(),
                self.try_dimensions()?
            )),
        }
    }
    pub fn write_metadata_fields(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        match self {
            Self::Local(model) => {
                model.write_metadata_fields(obj);
                obj.insert("embedding_provider".into(), "fastembed".into());
                obj.remove("embedding_fingerprint");
            }
            Self::ActiveRemote => {
                obj.insert("embedding_provider".into(), "openai".into());
                obj.insert(
                    "model_short_name".into(),
                    active_config().model.clone().into(),
                );
                obj.insert("model_name".into(), active_config().model.clone().into());
                obj.insert("dimensions".into(), self.dimensions().into());
                obj.insert(
                    "embedding_fingerprint".into(),
                    active_config().fingerprint().into(),
                );
            }
        }
    }
    pub fn from_index_metadata(path: &Path) -> Option<Self> {
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("metadata.json")).ok()?).ok()?;
        if data.get("embedding_provider").and_then(|v| v.as_str()) == Some("openai") {
            return Some(Self::ActiveRemote);
        }
        ModelType::parse(data.get("model_short_name")?.as_str()?).map(Self::Local)
    }
    pub fn validate_strategy(path: &Path) -> Result<()> {
        if !path.join("metadata.json").exists() && !active_config().remote() {
            return Ok(());
        }
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("metadata.json"))?)?;
        let provider = data
            .get("embedding_provider")
            .and_then(|value| value.as_str())
            .unwrap_or("fastembed");
        if provider == "fastembed" && !active_config().remote() {
            return Ok(());
        }
        Self::resolve_index(path).map(|_| ())
    }

    pub fn resolve_index(path: &Path) -> Result<Self> {
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("metadata.json"))?)?;
        let provider = data
            .get("embedding_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("fastembed");
        let mismatch = || {
            anyhow::anyhow!("Embedding strategy mismatch; rebuild this index with --force using the active provider, model and input configuration")
        };
        match provider {
            "openai" => {
                if !active_config().remote()
                    || data.get("model_short_name").and_then(|v| v.as_str())
                        != Some(&active_config().model)
                    || data.get("embedding_fingerprint").and_then(|v| v.as_str())
                        != Some(&active_config().fingerprint())
                {
                    return Err(mismatch());
                }
                let dims = data.get("dimensions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if dims == 0 || dims != Self::ActiveRemote.try_dimensions()? {
                    return Err(mismatch());
                }
                Ok(Self::ActiveRemote)
            }
            "fastembed" => {
                if active_config().remote() {
                    return Err(mismatch());
                }
                let name = data
                    .get("model_short_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("minilm-l6-q");
                let model = ModelType::parse(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Unknown embedding model '{name}' in index metadata; rebuild with --force"
                    )
                })?;
                let dims = data
                    .get("dimensions")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(model.dimensions() as u64);
                if dims as usize != model.dimensions() {
                    bail!("Metadata dimension inconsistency; rebuild with --force");
                }
                Ok(Self::Local(model))
            }
            _ => bail!("Unknown embedding provider in index metadata; rebuild with --force"),
        }
    }
    pub fn batch_size(&self) -> usize {
        match self {
            Self::Local(model) => super::FastEmbedder::effective_batch_size(*model).max(1),
            Self::ActiveRemote => active_config().embed_batch_size,
        }
    }
}
