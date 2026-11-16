#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("ilar-file-worker {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ilar::tools::file_worker::worker_main().await
}
