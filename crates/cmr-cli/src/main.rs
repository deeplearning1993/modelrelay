//! Executable entry point for the Codex Model Router CLI.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cmr_cli::main_entry().await
}
