use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "yaasm", version, about)]
pub struct Cli {
    #[arg(long, global = true, requires = "data_dir")]
    pub config_dir: Option<PathBuf>,

    #[arg(long, global = true, requires = "config_dir")]
    pub data_dir: Option<PathBuf>,

    #[arg(long, global = true, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..))]
    pub jobs: u16,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Deploy {
        #[arg(long, conflicts_with = "bundle")]
        locked: bool,
        /// Deploy strictly offline from a previously built remote bundle.
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    /// Update remote sources and the lockfile without deploying skills.
    Update { sources: Vec<String> },
    /// Build a portable remote bundle from the current lockfile.
    Bundle {
        /// Publish to a new directory. The lockfile is never changed.
        #[arg(long)]
        output: PathBuf,
    },
}
