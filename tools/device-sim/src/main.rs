//! Virtual hardware, so a whole line can be exercised on a laptop.
//!
//! Two roles:
//!
//! * **instrument** — accepts TCP connections and emits ASCII readings, the way
//!   a torque wrench or a scale on an Ethernet converter does. It can be told
//!   to emit out-of-limit readings, so the quarantine and rework paths are
//!   testable without deliberately over-torquing anything.
//! * **printer** — listens on Zebra's raw port 9100 and prints the ZPL it
//!   receives to stdout, which is how a label is checked before a roll of
//!   media is committed to it.
//!
//! ```bash
//! device-sim instrument --port 4001 --nominal 12.0 --tolerance 0.4
//! device-sim instrument --port 4001 --bad-every 5      # every 5th part fails
//! device-sim printer --port 9100
//! ```

#![warn(missing_docs)]

use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Command line.
#[derive(Debug, Parser)]
#[command(
    name = "device-sim",
    about = "Virtual plant hardware for ElectronIx Trace"
)]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

/// What to pretend to be.
#[derive(Debug, Subcommand)]
enum Role {
    /// A measuring instrument emitting ASCII readings over TCP.
    Instrument {
        /// Port to listen on.
        #[arg(short, long, default_value_t = 4001)]
        port: u16,
        /// Target reading.
        #[arg(long, default_value_t = 12.0)]
        nominal: f64,
        /// Variation applied around the nominal.
        #[arg(long, default_value_t = 0.3)]
        tolerance: f64,
        /// Emit an out-of-limits reading every Nth request. 0 disables.
        ///
        /// This is how the quarantine and rework paths get exercised without
        /// deliberately damaging a real part.
        #[arg(long, default_value_t = 0)]
        bad_every: u64,
        /// Text around the number, so parser behaviour can be checked.
        #[arg(long, default_value = "T: {v} NM")]
        format: String,
    },

    /// A Zebra printer on the raw print port.
    Printer {
        /// Port to listen on. Zebra's raw port is 9100.
        #[arg(short, long, default_value_t = 9100)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().role {
        Role::Instrument {
            port,
            nominal,
            tolerance,
            bad_every,
            format,
        } => serve_instrument(port, nominal, tolerance, bad_every, format).await,
        Role::Printer { port } => serve_printer(port).await,
    }
}

/// Deterministic pseudo-variation around a nominal.
///
/// Deliberately not random: a simulator that produces a different reading every
/// run makes a failing test impossible to reproduce.
fn reading_for(seq: u64, nominal: f64, tolerance: f64) -> f64 {
    // A cheap repeating pattern in [-1.0, 1.0].
    let phase = ((seq % 8) as f64 / 4.0) - 1.0;
    nominal + phase * tolerance
}

/// Format a reading into the instrument's line protocol.
fn format_reading(template: &str, value: f64) -> String {
    format!("{}\r\n", template.replace("{v}", &format!("{value:.2}")))
}

async fn serve_instrument(
    port: u16,
    nominal: f64,
    tolerance: f64,
    bad_every: u64,
    format: String,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        port,
        nominal,
        tolerance,
        bad_every,
        "virtual instrument listening; connect a station to it"
    );

    let seq = Arc::new(AtomicU64::new(0));

    loop {
        let (socket, peer) = listener.accept().await?;
        let seq = Arc::clone(&seq);
        let format = format.clone();
        tokio::spawn(async move {
            tracing::info!(%peer, "station connected");
            if let Err(e) =
                instrument_session(socket, &seq, nominal, tolerance, bad_every, &format).await
            {
                tracing::info!(%peer, error = %e, "station disconnected");
            }
        });
    }
}

/// Emit a reading per request, and one immediately so streaming devices work.
async fn instrument_session(
    socket: TcpStream,
    seq: &AtomicU64,
    nominal: f64,
    tolerance: f64,
    bad_every: u64,
    format: &str,
) -> anyhow::Result<()> {
    let mut stream = BufReader::new(socket);

    loop {
        let n = seq.fetch_add(1, Ordering::SeqCst) + 1;

        // A "bad" part is far outside tolerance, so it fails any sane limit.
        let value = if bad_every > 0 && n.is_multiple_of(bad_every) {
            tracing::warn!(seq = n, "emitting a deliberately out-of-limits reading");
            nominal + tolerance * 20.0
        } else {
            reading_for(n, nominal, tolerance)
        };

        let line = format_reading(format, value);
        stream.get_mut().write_all(line.as_bytes()).await?;
        stream.get_mut().flush().await?;
        tracing::debug!(seq = n, value, "emitted");

        // Wait for the station to ask for the next one. A device that streams
        // without being asked would flood the station.
        let mut request = String::new();
        if stream.read_line(&mut request).await? == 0 {
            return Ok(());
        }
    }
}

async fn serve_printer(port: u16) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        port,
        "virtual Zebra listening; ZPL will be printed to stdout"
    );

    loop {
        let (mut socket, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = Vec::new();
            match tokio::io::AsyncReadExt::read_to_end(&mut socket, &mut buf).await {
                Ok(_) => {
                    let zpl = String::from_utf8_lossy(&buf);
                    tracing::info!(%peer, bytes = buf.len(), "label received");
                    println!("----- ZPL from {peer} -----");
                    println!("{zpl}");
                    println!("----- end -----");
                    summarise(&zpl);
                }
                Err(e) => tracing::warn!(%peer, error = %e, "printer read failed"),
            }
        });
    }
}

/// Point out the things that usually go wrong with a label.
fn summarise(zpl: &str) {
    let fields = zpl.matches("^FO").count();
    let separators = zpl.matches("^FS").count();

    if fields != separators {
        // A missing ^FS makes a real printer wait forever for the field to end.
        println!("WARNING: {fields} field origins but {separators} field separators");
    }
    if !zpl.contains("^XA") || !zpl.contains("^XZ") {
        println!("WARNING: label is not wrapped in ^XA ... ^XZ");
    }
    if zpl.contains("{{") {
        println!("WARNING: unresolved binding left in the label");
    }
    println!(
        "summary: {fields} fields, {} barcodes",
        zpl.matches("^B").count()
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn readings_are_deterministic_so_a_failure_can_be_reproduced() {
        assert_eq!(reading_for(3, 12.0, 0.4), reading_for(3, 12.0, 0.4));
        assert_ne!(reading_for(1, 12.0, 0.4), reading_for(3, 12.0, 0.4));
    }

    #[test]
    fn readings_stay_within_the_configured_tolerance() {
        for seq in 0..64 {
            let v = reading_for(seq, 12.0, 0.4);
            assert!((11.6..=12.4).contains(&v), "seq {seq} gave {v}");
        }
    }

    #[test]
    fn the_line_format_matches_what_instruments_emit() {
        assert_eq!(format_reading("T: {v} NM", 12.1), "T: 12.10 NM\r\n");
        assert_eq!(format_reading("{v}", 9.0), "9.00\r\n");
    }

    #[tokio::test]
    async fn a_station_can_read_from_the_virtual_instrument() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seq = Arc::new(AtomicU64::new(0));

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = instrument_session(socket, &seq, 12.0, 0.3, 0, "T: {v} NM").await;
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        assert!(line.starts_with("T: "), "got {line:?}");
        assert!(line.ends_with("NM\r\n"), "got {line:?}");
    }

    #[tokio::test]
    async fn the_bad_every_switch_produces_a_failing_reading() {
        // This is how the quarantine path gets exercised without over-torquing
        // a real part.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seq = Arc::new(AtomicU64::new(0));

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // Every reading is bad, so the first one is.
            let _ = instrument_session(socket, &seq, 12.0, 0.3, 1, "{v}").await;
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let value: f64 = line.trim().parse().unwrap();
        assert!(
            value > 14.0,
            "a bad reading must fail a 10..14 limit, got {value}"
        );
    }
}
