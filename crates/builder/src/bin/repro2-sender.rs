use anyhow::Result;
use builder::queue::{Job, Queue};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Durable local Nix post-build queue and resident publisher")]
struct Cli {
    #[arg(
        long,
        env = "REPRO2_SPOOL",
        default_value = "/var/lib/repro2-sender",
        global = true
    )]
    spool: PathBuf,
    #[arg(
        long,
        env = "REPRO2_GC_ROOTS",
        default_value = "/nix/var/nix/gcroots/repro2",
        global = true
    )]
    gc_roots: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Called by Nix with DRV_PATH and space-separated OUT_PATHS; no Nix/network calls.
    Hook,
    /// Strict local enqueue for debugging; errors return nonzero.
    Enqueue {
        drv_path: String,
        #[arg(required = true)]
        outputs: Vec<String>,
    },
    /// Automatically retry persisted jobs until interrupted.
    Run(builder::sender::Sender),
}

fn enqueue(queue: &Queue, drv_path: String, outputs: Vec<String>) -> Result<()> {
    let id = queue.enqueue(Job::new(drv_path, outputs)?)?;
    eprintln!("repro2-sender queued {id}");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let result = (|| {
        let queue = Queue::open(&cli.spool, &cli.gc_roots)?;
        match &cli.command {
            Action::Hook => enqueue(
                &queue,
                std::env::var("DRV_PATH")?,
                std::env::var("OUT_PATHS")?
                    .split_ascii_whitespace()
                    .map(str::to_owned)
                    .collect(),
            ),
            Action::Enqueue { drv_path, outputs } => {
                enqueue(&queue, drv_path.clone(), outputs.clone())
            }
            Action::Run(sender) => sender.run(&queue),
        }
    })();
    if matches!(cli.command, Action::Hook) {
        if let Err(error) = result {
            eprintln!(
                "CRITICAL repro2-sender LOCAL QUEUE FAILURE: {error:#}; build is not failed, publication/GC retention is NOT guaranteed"
            );
        }
        Ok(())
    } else {
        result
    }
}
