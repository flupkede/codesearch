use clap::Parser;
use codesearch_core::embed::{EmbedConfig, ModelType};
use codesearch_server::{router, Embedding, Store};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(about = "Unauthenticated loopback PostgreSQL search foundation (#384 stage 2)")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:39726")]
    listen: SocketAddr,
    #[arg(long, default_value = "minilm-l6-q")]
    model: String,
    #[command(flatten)]
    embedding: EmbedConfig,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("codesearch_server=info")
        .init();
    let args = Args::parse();
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "stage 2 has no authentication; only loopback listeners are supported"
    );
    let mut config = args.embedding;
    config.model = args.model.clone();
    let local = if config.remote() {
        ModelType::default()
    } else {
        ModelType::parse(&args.model).ok_or_else(|| {
            anyhow::anyhow!("unknown local model; use a supported core model short name")
        })?
    };
    let embedding = tokio::task::spawn_blocking(move || Embedding::new(config, local)).await??;
    let url = std::env::var("CODESEARCH_DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("set CODESEARCH_DATABASE_URL"))?;
    let store = Store::connect(&url, embedding).await?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!(
        "codesearch-server stage 2: unauthenticated loopback only, listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, router(store))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
