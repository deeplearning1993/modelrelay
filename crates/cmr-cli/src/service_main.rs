//! Background-only executable entry point for Codex Model Router.
//!
//! Windows uses the GUI subsystem so Task Scheduler can start the router
//! without allocating or showing a console window. The regular `cmr` binary
//! remains a console application for interactive commands and diagnostics.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cmr_cli::main_entry().await
}
