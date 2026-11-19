use super::config::EmbedConfig;
use super::provider::Embedder;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, SystemTime};

static NOTICE: Once = Once::new();
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static DIMENSIONS: OnceLock<usize> = OnceLock::new();
static PROBE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub fn known_dimensions() -> Option<usize> {
    DIMENSIONS.get().copied()
}

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("embedding HTTP runtime")
    })
}

pub fn active_dimensions() -> Result<usize> {
    if let Some(dims) = DIMENSIONS.get() {
        return Ok(*dims);
    }
    let _probe = PROBE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Embedding probe mutex poisoned"))?;
    if let Some(dims) = DIMENSIONS.get() {
        return Ok(*dims);
    }
    let embedder = OpenAiEmbedder::new(super::active_config().clone())?;
    let _ = DIMENSIONS.set(embedder.dimensions);
    Ok(embedder.dimensions)
}

pub struct OpenAiEmbedder {
    config: EmbedConfig,
    client: reqwest::Client,
    dimensions: usize,
    semaphore: Arc<tokio::sync::Semaphore>,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Datum>,
}
#[derive(Deserialize)]
struct Datum {
    index: usize,
    embedding: Vec<f32>,
}

impl OpenAiEmbedder {
    pub fn new(config: EmbedConfig) -> Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot initialize embedding HTTP client"))?;
        let mut embedder = Self {
            dimensions: config.embed_dimensions.unwrap_or(0),
            semaphore: Arc::new(tokio::sync::Semaphore::new(config.embed_concurrency)),
            config,
            client,
        };
        if embedder.dimensions == 0 {
            let result = embedder.run(vec!["dimension probe".to_owned()], true)?;
            embedder.dimensions = result[0].len();
        }
        Ok(embedder)
    }

    pub fn with_known_dimensions(config: EmbedConfig, dimensions: usize) -> Result<Self> {
        let mut initial = config.clone();
        initial.embed_dimensions = Some(dimensions);
        let mut embedder = Self::new(initial)?;
        embedder.config = config;
        Ok(embedder)
    }

    fn run(&self, texts: Vec<String>, query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let config = self.config.clone();
        let client = self.client.clone();
        let semaphore = self.semaphore.clone();
        let dimensions = self.dimensions;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let timeout = Duration::from_secs(config.embed_timeout_secs);
        // All HTTP work runs on a dedicated runtime. Sync callers may themselves
        // be inside tokio; nested block_on / blocking clients would panic there.
        runtime().spawn(async move {
            let work = async {
                let mut tasks = tokio::task::JoinSet::new();
                for (batch, inputs) in texts.chunks(config.embed_batch_size).enumerate() {
                    let inputs = inputs.iter().map(|s| config.transform(s, query)).collect();
                    let config = config.clone();
                    let client = client.clone();
                    let permit = semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| anyhow::anyhow!("Embedding queue closed"))?;
                    tasks.spawn(async move {
                        let _permit = permit;
                        request(&client, &config, inputs, dimensions)
                            .await
                            .map(|vectors| (batch, vectors))
                    });
                }
                let mut batches = Vec::new();
                while let Some(result) = tasks.join_next().await {
                    batches.push(
                        result.map_err(|_| anyhow::anyhow!("Embedding request task failed"))??,
                    );
                }
                batches.sort_by_key(|(index, _)| *index);
                Ok(batches
                    .into_iter()
                    .flat_map(|(_, vectors)| vectors)
                    .collect())
            };
            let result = tokio::time::timeout(timeout, work)
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "Remote embedding deadline exceeded (including queue and retries)"
                    ))
                });
            let _ = tx.send(result);
        });
        rx.recv_timeout(timeout + Duration::from_secs(1))
            .map_err(|_| anyhow::anyhow!("Remote embedding deadline exceeded"))?
    }
}

async fn request(
    client: &reqwest::Client,
    config: &EmbedConfig,
    inputs: Vec<String>,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>> {
    let endpoint = config.endpoint()?;
    NOTICE.call_once(|| {
        eprintln!(
            "Remote embeddings active: endpoint={} model={}\n\
             Source chunks and queries will be sent to this endpoint.",
            endpoint, config.model
        );
    });
    let mut body =
        serde_json::json!({ "model": config.model, "input": inputs, "encoding_format": "float" });
    if let Some(dimensions) = config.embed_dimensions {
        body["dimensions"] = dimensions.into();
    }
    for attempt in 0..4u32 {
        let mut request = client
            .post(endpoint.clone())
            .json(&body)
            .timeout(Duration::from_secs(config.embed_timeout_secs));
        if let Ok(key) = std::env::var("CODESEARCH_EMBED_API_KEY") {
            if !key.is_empty() {
                request = request.bearer_auth(key);
            }
        }
        let response = request.send().await.map_err(|_| {
            anyhow::anyhow!("Remote embedding transport failure (URL and credentials redacted)")
        })?;
        let status = response.status();
        if status.is_success() {
            // Never attach the response body or a deserializer error to errors:
            // compatible servers may echo submitted secrets or source text.
            let response: Response = response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("Invalid embedding response JSON"))?;
            return validate_response(response, inputs.len(), dimensions);
        }
        if !(status.as_u16() == 429 || status.is_server_error()) || attempt == 3 {
            let hint = match status.as_u16() {
                401 | 403 => "check CODESEARCH_EMBED_API_KEY and endpoint permissions",
                400 | 404 | 422 => "check embedding base URL, model and dimensions",
                _ => "endpoint unavailable or retry budget exhausted",
            };
            bail!(
                "Remote embedding request failed (HTTP {}): {hint}",
                status.as_u16()
            );
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after_delay);
        let jitter = rand::random::<u8>() as u64;
        let backoff = Duration::from_millis(200 * (1u64 << attempt) + jitter);
        tokio::time::sleep(retry_after.unwrap_or(backoff).max(backoff)).await;
    }
    unreachable!()
}

fn retry_after_delay(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|time| time.duration_since(SystemTime::now()).unwrap_or_default())
        })
}

fn validate_response(response: Response, count: usize, dimensions: usize) -> Result<Vec<Vec<f32>>> {
    if response.data.len() != count {
        bail!("Embedding response count mismatch");
    }
    let mut ordered = vec![None; count];
    let mut expected = dimensions;
    for datum in response.data {
        if datum.index >= count || ordered[datum.index].is_some() {
            bail!("Invalid or duplicate embedding response index");
        }
        if expected == 0 {
            expected = datum.embedding.len();
        }
        if expected == 0 || datum.embedding.len() != expected {
            bail!("Embedding response dimension mismatch");
        }
        let mut vector = datum.embedding;
        if vector.iter().any(|v| !v.is_finite()) {
            bail!("Nonfinite embedding response");
        }
        let norm = vector
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 || !norm.is_finite() {
            bail!("Invalid embedding response norm");
        }
        for value in &mut vector {
            *value = (f64::from(*value) / norm) as f32;
        }
        ordered[datum.index] = Some(vector);
    }
    ordered
        .into_iter()
        .map(|v| v.ok_or_else(|| anyhow::anyhow!("Missing embedding response index")))
        .collect()
}

impl Embedder for OpenAiEmbedder {
    fn embed_documents(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.run(texts, false)
    }
    fn embed_queries(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.run(texts, true)
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    fn model_name(&self) -> &str {
        &self.config.model
    }
    fn local_model(&self) -> Option<super::ModelType> {
        None
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
