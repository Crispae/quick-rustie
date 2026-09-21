//! Smoke-check Quickwit storage against local MinIO (`rustie-minio` on :9010).
//!
//! ```bash
//! cargo run --example minio_ping
//! ```

use quick_rustie::{ping_minio, MinioConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = MinioConfig::default();
    println!(
        "pinging MinIO endpoint={} bucket={} prefix={}",
        config.endpoint, config.bucket, config.prefix
    );

    let body = ping_minio(&config).await?;
    println!(
        "ok — wrote/read {} bytes: {}",
        body.len(),
        String::from_utf8_lossy(&body).trim()
    );
    Ok(())
}
