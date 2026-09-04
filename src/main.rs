use anyhow::Result;
use clap::Parser;

use yaasm::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
