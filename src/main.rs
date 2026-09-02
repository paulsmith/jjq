// ABOUTME: jjq - A local merge queue for jj (Jujutsu VCS).
// ABOUTME: Implements the jjq specification for queuing and processing merge candidates.

mod commands;
mod config;
mod exit_codes;
mod history;
mod jj;
mod lock;
mod meta;
mod paths;
mod queue;
mod runlog;
mod runner;
mod tail;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "jjq", about = "Local merge queue for jj", version)]
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
        /// Exempt a multi-revision stack from the single-revision policy
        #[arg(long)]
        allow_stack: bool,
    },
    /// Process the next item(s) in the queue
    Run {
        /// Process all queued items until empty or failure
        #[arg(long)]
        all: bool,
        /// Stop processing on first failure (only with --all)
        #[arg(long)]
        stop_on_failure: bool,
        /// Maximum concurrent speculative checks (requires --all)
        #[arg(long, default_value_t = 1, requires = "all", value_parser = parse_jobs)]
        jobs: usize,
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
    /// Re-push a failed item back onto the queue
    Requeue {
        /// Sequence ID of the failed item
        id: String,
    },
    /// Reject an item: mark it failed without running, with a reason
    Reject {
        /// Sequence ID of the queued or failed item
        id: String,
        /// Why the item is rejected (shown to the submitter via status/wait)
        #[arg(long)]
        reason: String,
    },
    /// Wait for a candidate to reach a terminal verdict (landed or rejected)
    Wait {
        /// Revset or change ID of the pushed candidate
        revset: String,
        /// Poll period in seconds
        #[arg(long, default_value_t = 10)]
        interval: u64,
        /// Give up after this many seconds (default: wait forever)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Remove an item from queue or failed list
    Delete {
        /// Sequence ID of the item
        id: String,
    },
    /// Cooperatively cancel a queued or running item
    Cancel {
        /// Sequence ID of the item
        id: String,
    },
    /// Display durable lifecycle history for a queue submission
    History {
        /// Sequence ID of a submission
        id: Option<String>,
        /// Find every submission for a candidate change ID
        #[arg(long, conflicts_with = "id")]
        change: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove jjq workspaces
    Clean,
    /// Validate configuration and environment
    Doctor {
        /// Repair a conflicted jjq metadata bookmark before running checks
        #[arg(long)]
        fix: bool,
    },
    /// Get or set configuration
    Config {
        /// Configuration key
        key: Option<String>,
        /// Value to set
        value: Option<String>,
    },
    /// View check command output
    Tail {
        /// Sequence ID of a failed item whose archived output to view
        id: Option<String>,
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

fn parse_jobs(input: &str) -> Result<usize, String> {
    let jobs: usize = input
        .parse()
        .map_err(|_| "jobs must be a positive integer".to_string())?;
    if jobs == 0 {
        return Err("jobs must be at least 1".to_string());
    }
    Ok(jobs)
}

fn main() {
    if let Err(e) = run() {
        if let Some(exit_err) = e.downcast_ref::<exit_codes::ExitError>() {
            // code_only errors were already reported in full by the command
            if !exit_err.message.is_empty() {
                eprintln!("jjq: {}", exit_err.message);
            }
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

    // Resolve the shared .jj directory up front so the cached value reflects
    // the workspace jjq was invoked from.
    jj::shared_jj_dir()?;

    match cli.command {
        Commands::Init { trunk, check, strategy } => {
            commands::init(trunk.as_deref(), check.as_deref(), &strategy)
        }
        Commands::Push {
            revset,
            allow_stack,
        } => commands::push(&revset, allow_stack),
        Commands::Run {
            all,
            stop_on_failure,
            jobs,
        } => commands::run(all, stop_on_failure, jobs),
        Commands::Check { rev, verbose } => commands::check(&rev, verbose),
        Commands::Status { id, json, resolve } => {
            commands::status(id.as_deref(), json, resolve.as_deref())
        }
        Commands::Requeue { id } => commands::requeue(&id),
        Commands::Reject { id, reason } => commands::reject(&id, &reason),
        Commands::Wait {
            revset,
            interval,
            timeout,
        } => commands::wait(&revset, interval, timeout),
        Commands::Delete { id } => commands::delete(&id),
        Commands::Cancel { id } => commands::cancel(&id),
        Commands::History { id, change, json } => {
            commands::history(id.as_deref(), change.as_deref(), json)
        }
        Commands::Clean => commands::clean(),
        Commands::Doctor { fix } => commands::doctor(fix),
        Commands::Config { key, value } => commands::config(key.as_deref(), value.as_deref()),
        Commands::Tail {
            id,
            all,
            no_follow,
        } => tail::tail(id.as_deref(), all, !no_follow),
        Commands::Quickstart => unreachable!(),
    }
}
