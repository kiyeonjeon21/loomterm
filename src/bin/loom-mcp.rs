use loomterm::client::DaemonClient;
use loomterm::config::AppPaths;
use loomterm::mcp::LoomMcpServer;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("loomterm=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = async {
        let paths = AppPaths::discover()?;
        paths.ensure()?;
        let client = DaemonClient::connect_or_start(&paths).await?;
        let service = LoomMcpServer::new(client)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| loomterm::Error::Protocol(error.to_string()))?;
        service
            .waiting()
            .await
            .map_err(|error| loomterm::Error::Protocol(error.to_string()))?;
        Ok::<(), loomterm::Error>(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("loom-mcp: {error}");
        std::process::exit(1);
    }
}
