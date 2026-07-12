use loomterm::config::{AppPaths, Settings};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("loomd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("loomterm=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = async {
        let paths = AppPaths::discover()?;
        let settings = Settings::load(&paths)?;
        loomterm::daemon::run(paths, settings).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("loomd: {error}");
        std::process::exit(1);
    }
}
