use anyhow::Result;

pub async fn run(model: Option<String>) -> Result<()> {
    let model_name = model.unwrap_or_else(|| "mxbai-embed-xsmall-v1".to_string());

    println!("📦 Downloading embedding model: {}", model_name);

    // TODO: Download model from HuggingFace Hub

    println!("✅ Setup complete!");
    Ok(())
}
