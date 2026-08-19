//! pixpull — super-robust HTTP/HTTPS picture downloader.
//!
//! Bulk image download with retries/backoff, resume, concurrency, segmented
//! downloads, anti-bot headers, and magic-byte integrity validation.

mod args;
mod client;
mod config;
mod cookies;
mod download;
mod throttle;
mod validate;

use args::Args;
use clap::{CommandFactory, Parser};
use download::Shared;
use futures::stream::{self, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use throttle::Throttle;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();

    if let Some(cfg_path) = &args.config {
        let cfg = config::load(cfg_path)?;
        config::apply_to_args(&mut args, &cfg);
    }

    // Guard: cap concurrency x split at --max-total-connections.
    if args.max_total_connections > 0 {
        let product = args.concurrency.max(1) * args.split.max(1);
        if product > args.max_total_connections {
            let new_split = (args.max_total_connections / args.concurrency.max(1)).max(1);
            if new_split < args.split {
                if !args.quiet {
                    eprintln!(
                        "note: --max-total-connections {} capped --split {} -> {}",
                        args.max_total_connections, args.split, new_split
                    );
                }
                args.split = new_split;
            }
        }
    }

    let inputs = args.all_inputs();
    if inputs.is_empty() {
        let mut cmd = Args::command();
        cmd.print_help()?;
        eprintln!("\n\nerror: no URLs given (pass URLs or use --input FILE)");
        std::process::exit(2);
    }

    let factory = client::ClientFactory::new(Arc::new(args.clone()))?;
    let shared = Arc::new(args.clone());
    let delay = args.delay_ms;

    // Shared run state: early-stop flag + global speed cap.
    let run_shared = Shared {
        stop: Arc::new(AtomicBool::new(false)),
        global_throttle: if args.max_overall_download_limit > 0 {
            Some(Throttle::new(args.max_overall_download_limit as f64 * 1024.0))
        } else {
            None
        },
    };

    // Optional persistent run log.
    let mut log = match &args.log {
        Some(path) => {
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await?;
            Some(f)
        }
        None => None,
    };

    let start = Instant::now();
    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut total_bytes: u64 = 0;
    let mut consecutive_404 = 0u32;

    if !args.quiet {
        println!(
            "pixpull: {} URL(s), concurrency {}, retries {}, split {}",
            inputs.len(),
            args.concurrency,
            args.retries,
            args.split
        );
    }

    let jobs = stream::iter(inputs.into_iter().enumerate().map(|(i, input)| {
        let factory = factory.clone();
        let shared = shared.clone();
        let run_shared = run_shared.clone();
        async move {
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            download::run_job(
                &factory,
                &shared,
                &run_shared,
                &input.url,
                i as u32 + shared.start_index,
                input.out,
            )
            .await
        }
    }))
    .buffer_unordered(args.concurrency.max(1));

    futures::pin_mut!(jobs);
    while let Some(result) = jobs.next().await {
        // --max-file-not-found: stop after N consecutive 404s.
        if result.not_found {
            consecutive_404 += 1;
            if args.max_file_not_found > 0 && consecutive_404 >= args.max_file_not_found {
                eprintln!(
                    "stopping after {} consecutive 404s (--max-file-not-found)",
                    consecutive_404
                );
                run_shared.stop.store(true, Ordering::Relaxed);
            }
        } else {
            consecutive_404 = 0;
        }

        match result.status {
            download::JobStatus::Downloaded => {
                done += 1;
                total_bytes += result.bytes;
                if !args.quiet {
                    let name = result
                        .file
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    println!("  ok   {name} ({} bytes)", result.bytes);
                }
            }
            download::JobStatus::Skipped => {
                skipped += 1;
                if !args.quiet {
                    let name = result
                        .file
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    println!("  skip {name} ({})", result.detail);
                }
            }
            download::JobStatus::Failed => {
                failed += 1;
                eprintln!("  FAIL {}: {}", result.url, result.detail);
            }
        }

        // Log every result (machine-readable).
        let ts = chrono_like_timestamp();
        let name = result
            .file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let status = match result.status {
            download::JobStatus::Downloaded => "OK",
            download::JobStatus::Skipped => "SKIP",
            download::JobStatus::Failed => "FAIL",
        };
        let line = format!("{ts} {status} {name} {} {}", result.bytes, result.url);
        write_log(&mut log, &line).await;

        if run_shared.should_stop() {
            break;
        }
    }

    let elapsed = start.elapsed();
    let rate = if elapsed.as_secs_f32() > 0.0 {
        total_bytes as f64 / 1024.0 / 1024.0 / f64::from(elapsed.as_secs_f32())
    } else {
        0.0
    };
    let summary = format!(
        "\nfinished in {:.1}s — {} ok ({} bytes, {:.1} MiB/s), {} skipped, {} failed",
        elapsed.as_secs_f32(),
        done,
        total_bytes,
        rate,
        skipped,
        failed
    );
    println!("{summary}");
    write_log(&mut log, summary.trim()).await;

    std::process::exit(if failed > 0 { 1 } else { 0 });
}

/// Append one line to the optional run log.
async fn write_log(log: &mut Option<tokio::fs::File>, line: &str) {
    if let Some(f) = log {
        use tokio::io::AsyncWriteExt;
        let _ = f.write_all(line.as_bytes()).await;
        let _ = f.write_all(b"\n").await;
    }
}

/// UTC timestamp for the run log, e.g. 2026-08-19T23:00:00.
fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (y, mo, d) = civil_from_days(secs / 86_400);
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
}

/// Convert days since the Unix epoch to (year, month, day) in the proleptic
/// Gregorian calendar (Howard Hinnant's civil-from-days algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
