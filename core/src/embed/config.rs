use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Nonsecret active strategy. Credentials are read only when constructing HTTP requests.
#[derive(Clone, Debug, Args, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
    #[arg(long, global = true, env = "CODESEARCH_EMBED_PROVIDER", value_parser = ["fastembed", "openai"])]
    pub embed_provider: Option<String>,
    #[arg(long, global = true, env = "CODESEARCH_EMBED_BASE_URL")]
    pub embed_base_url: Option<String>,
    #[arg(long, global = true, env = "CODESEARCH_EMBED_DIMENSIONS")]
    pub embed_dimensions: Option<usize>,
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_BATCH_SIZE",
        default_value_t = 32
    )]
    pub embed_batch_size: usize,
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_TIMEOUT_SECS",
        default_value_t = 30
    )]
    pub embed_timeout_secs: u64,
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_CONCURRENCY",
        default_value_t = 2
    )]
    pub embed_concurrency: usize,
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_QUERY_PREFIX",
        default_value = ""
    )]
    pub embed_query_prefix: String,
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_DOCUMENT_PREFIX",
        default_value = ""
    )]
    pub embed_document_prefix: String,
    /// Maximum UTF-8 input bytes, including prefix (remote only).
    #[arg(
        long,
        global = true,
        env = "CODESEARCH_EMBED_MAX_INPUT_BYTES",
        default_value_t = 24000
    )]
    pub embed_max_input_bytes: usize,
    #[arg(skip)]
    pub model: String,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            embed_provider: None,
            embed_base_url: None,
            embed_dimensions: None,
            embed_batch_size: 32,
            embed_timeout_secs: 30,
            embed_concurrency: 2,
            embed_query_prefix: String::new(),
            embed_document_prefix: String::new(),
            embed_max_input_bytes: 24000,
            model: String::new(),
        }
    }
}

static ACTIVE: OnceLock<EmbedConfig> = OnceLock::new();
pub fn active_config() -> &'static EmbedConfig {
    ACTIVE.get_or_init(EmbedConfig::default)
}

impl EmbedConfig {
    pub fn remote(&self) -> bool {
        self.embed_provider.as_deref() == Some("openai")
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(
            self.embed_provider.as_deref(),
            None | Some("fastembed") | Some("openai")
        ) {
            bail!("Unknown embedding provider; use fastembed or openai");
        }
        if !self.remote() {
            return Ok(());
        }
        self.endpoint()?;
        if self.model.is_empty() {
            bail!("OpenAI embeddings require an explicit --model");
        }
        if self.embed_dimensions == Some(0)
            || self.embed_batch_size == 0
            || self.embed_timeout_secs == 0
            || self.embed_concurrency == 0
            || self.embed_max_input_bytes == 0
        {
            bail!("Embedding dimensions, batch size, timeout, concurrency and input limit must be positive");
        }
        Ok(())
    }

    pub fn endpoint(&self) -> Result<reqwest::Url> {
        let base = self.embed_base_url.as_deref().context(
            "OpenAI embeddings require --embed-base-url (no default network destination)",
        )?;
        let mut url =
            reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("Invalid embedding base URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("Embedding base URL must be HTTP(S), without credentials, query or fragment");
        }
        let path = format!("{}/embeddings", url.path().trim_end_matches('/'));
        url.set_path(&path);
        Ok(url)
    }

    pub fn fingerprint(&self) -> String {
        // Endpoint is deliberately not persisted per index. A changed endpoint serving
        // the same model is allowed; model/dimension/input transformations must match.
        let identity = serde_json::json!([
            "openai",
            self.model,
            self.embed_dimensions,
            self.embed_query_prefix,
            self.embed_document_prefix,
            self.embed_max_input_bytes
        ]);
        let hex = Sha256::digest(identity.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("openai-{hex}")
    }

    pub fn transform(&self, text: &str, query: bool) -> String {
        let prefix = if query {
            &self.embed_query_prefix
        } else {
            &self.embed_document_prefix
        };
        let mut input = format!("{prefix}{text}");
        let mut end = input.len().min(self.embed_max_input_bytes);
        while !input.is_char_boundary(end) {
            end -= 1;
        }
        input.truncate(end);
        input
    }
}

pub fn configure(config: EmbedConfig, model: Option<&str>, explicit: &[String]) -> Result<()> {
    let mut merged = config.clone();
    if let Ok(path) = crate::constants::codesearch_home() {
        if let Ok(data) = std::fs::read(path.join("embedding-defaults.json")) {
            let saved: EmbedConfig =
                serde_json::from_slice(&data).context("Invalid embedding defaults; rerun setup")?;
            let mut saved = serde_json::to_value(saved)?;
            let supplied = serde_json::to_value(&config)?;
            for key in explicit {
                if let Some(value) = supplied.get(key) {
                    saved[key] = value.clone();
                }
            }
            merged = serde_json::from_value(saved)?;
        }
    }
    if let Some(model) = model {
        merged.model = model.to_owned();
    }
    merged.validate()?;
    ACTIVE
        .set(merged)
        .map_err(|_| anyhow::anyhow!("Embedding strategy already configured"))
}

pub fn save_defaults() -> Result<()> {
    let config = active_config();
    config.validate()?;
    let home = crate::constants::codesearch_home()?;
    std::fs::create_dir_all(&home)?;
    let path = home.join("embedding-defaults.json");
    let tmp = home.join("embedding-defaults.json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(config)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
