use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("loom-supervisor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("loomterm=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(error) = loomterm::supervisor::run().await {
        eprintln!("loom-supervisor: {error}");
        std::process::exit(1);
    }
}
