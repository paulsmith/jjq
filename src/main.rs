// ABOUTME: jjq - A local merge queue for jj (Jujutsu VCS).
// ABOUTME: Implements the jjq specification for queuing and processing merge candidates.

mod commands;
mod config;
mod exit_codes;
mod jj;
mod lock;
mod queue;
mod runlog;
mod runner;
mod tail;

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
    /// Initialize jjq in this repository
    Init {
        /// Trunk bookmark name
        #[arg(long)]
        trunk: Option<String>,
        /// Check command
        #[arg(long)]
        check: Option<String>,
        /// Merge strategy (rebase or merge)
        #[arg(long, default_value = "rebase")]
        strategy: String,
    },
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
        /// Stop processing on first failure (only with --all)
        #[arg(long)]
        stop_on_failure: bool,
    },
    /// Run check command against a revision without queue processing
    Check {
        /// Revset to check
        #[arg(long, default_value = "@")]
        rev: String,
        /// Show workspace path, shell, and environment before running
        #[arg(long, short)]
        verbose: bool,
    },
    /// Display current queue state
    Status {
        /// Sequence ID of a specific item to show
        id: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Look up item by candidate change ID
        #[arg(long, conflicts_with = "id")]
        resolve: Option<String>,
    },
    /// Remove an item from queue or failed list
    Delete {
        /// Sequence ID of the item
        id: String,
    },
    /// Remove jjq workspaces
    Clean,
    /// Validate configuration and environment
    Doctor,
    /// Get or set configuration
    Config {
        /// Configuration key
        key: Option<String>,
        /// Value to set
        value: Option<String>,
    },
    /// View check command output
    Tail {
        /// Show output from the beginning (default: last 20 lines)
        #[arg(long)]
        all: bool,
        /// Don't follow output, just dump and exit
        #[arg(long)]
        no_follow: bool,
    },
    /// Print quickstart guide for LLM agents
    Quickstart,
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

    // Quickstart doesn't need a repo
    if let Commands::Quickstart = cli.command {
        print!("{}", include_str!("quickstart.txt"));
        return Ok(());
    }

    // Verify we're in a jj repository
    jj::verify_repo()?;

    match cli.command {
        Commands::Init { trunk, check, strategy } => {
            commands::init(trunk.as_deref(), check.as_deref(), &strategy)
        }
        Commands::Push { revset } => commands::push(&revset),
        Commands::Run {
            all,
            stop_on_failure,
        } => commands::run(all, stop_on_failure),
        Commands::Check { rev, verbose } => commands::check(&rev, verbose),
        Commands::Status { id, json, resolve } => {
            commands::status(id.as_deref(), json, resolve.as_deref())
        }
        Commands::Delete { id } => commands::delete(&id),
        Commands::Clean => commands::clean(),
        Commands::Doctor => commands::doctor(),
        Commands::Config { key, value } => commands::config(key.as_deref(), value.as_deref()),
        Commands::Tail { all, no_follow } => tail::tail(all, !no_follow),
        Commands::Quickstart => unreachable!(),
    }
}
