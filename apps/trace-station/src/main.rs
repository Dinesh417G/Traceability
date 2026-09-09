//! ElectronIx Trace station agent.
//!
//! The headless half of an operator terminal: it owns the durable offline
//! spool, talks to the edge, and replays anything captured while the edge was
//! unreachable.
//!
//! The durable spool itself lives in the `trace_station` library so the Tauri 2
//! touch UI can reuse it. See `DECISIONS.md` D-006.

use anyhow::Context as _;
use clap::Parser;
use trace_station::{Spool, SpoolRecord};

/// Command line.
#[derive(Debug, Parser)]
#[command(name = "trace-station", about = "ElectronIx Trace station agent")]
struct Cli {
    /// Spool file. Must be on local storage that survives a power cut.
    #[arg(
        long,
        default_value = "/var/lib/electronix-trace/spool.jsonl",
        env = "TRACE_SPOOL"
    )]
    spool: std::path::PathBuf,

    /// Edge base URL.
    #[arg(
        long,
        default_value = "http://trace.plant.local",
        env = "TRACE_EDGE_URL"
    )]
    edge_url: String,

    /// Show what is queued and exit.
    #[arg(long)]
    status: bool,

    /// Append a test event to the spool and exit.
    #[arg(long)]
    enqueue: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,trace_station=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let spool = Spool::new(&cli.spool);

    if let Some(topic) = cli.enqueue {
        let record = SpoolRecord::new(topic, serde_json::json!({"source": "cli"}));
        spool
            .append(&record)
            .await
            .context("appending to the spool")?;
        println!("queued {} ({})", record.id, record.topic);
        return Ok(());
    }

    let queued = spool.load().await.context("reading the spool")?;

    println!("station agent");
    println!("  spool    {}", spool.path().display());
    println!("  edge     {}", cli.edge_url);
    println!("  queued   {} event(s)", queued.len());

    if let Some(oldest) = queued.first() {
        println!(
            "  oldest   {} at {}",
            oldest.topic,
            oldest.recorded_at.to_rfc3339()
        );
    }

    if cli.status {
        return Ok(());
    }

    // The replay loop belongs here. It is deliberately not written yet rather
    // than half written: it needs the edge ingest endpoint it will call, and a
    // replay path that has never round-tripped against a real edge is exactly
    // the code that silently drops a shift's data.
    println!();
    println!("Replay against the edge is not implemented in this release.");
    println!("The spool is durable and idempotent, so nothing captured is lost;");
    println!("events accumulate until the replay loop lands.");

    Ok(())
}
