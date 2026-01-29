// ABOUTME: jjq - A local merge queue for jj (Jujutsu VCS).
// ABOUTME: Implements the jjq specification for queuing and processing merge candidates.

mod commands;
mod config;
mod exit_codes;
mod jj;
mod lock;
mod queue;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "jjq", about = "Local merge queue for jj")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Queue a revision for merging to trunk
    Push {
        /// Revset expression resolving to exactly one revision
        revset: String,
    },
    /// Process the next item(s) in the queue
    Run {
        /// Process all queued items until empty or failure
        #[arg(long)]
        all: bool,
    },
    /// Display current queue state
    Status,
    /// Retry a failed merge attempt
    Retry {
        /// Sequence ID of the failed item
        id: String,
        /// Optional revset to use instead of original candidate
        revset: Option<String>,
    },
    /// Remove an item from queue or failed list
    Delete {
        /// Sequence ID of the item
        id: String,
    },
    /// Get or set configuration
    Config {
        /// Configuration key
        key: Option<String>,
        /// Value to set
        value: Option<String>,
    },
}

fn main() {
    if let Err(e) = run() {
        if let Some(exit_err) = e.downcast_ref::<exit_codes::ExitError>() {
            eprintln!("jjq: {}", exit_err.message);
            std::process::exit(exit_err.code);
        }
        eprintln!("jjq: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Verify we're in a jj repository
    jj::verify_repo()?;

    match cli.command {
        Commands::Push { revset } => commands::push(&revset),
        Commands::Run { all } => commands::run(all),
        Commands::Status => commands::status(),
        Commands::Retry { id, revset } => commands::retry(&id, revset.as_deref()),
        Commands::Delete { id } => commands::delete(&id),
        Commands::Config { key, value } => commands::config(key.as_deref(), value.as_deref()),
    }
}
