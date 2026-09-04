use clap::Parser;

use yaasm::cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    tokio::select! {
        result = yaasm::app::run(cli) => {
            if let Err(error) = result {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("interrupted");
            std::process::exit(130);
        }
    }
}
