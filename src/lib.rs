//! pixpull — super-robust HTTP/HTTPS picture downloader.
//!
//! This crate ships as both a command-line tool and a reusable library for
//! bulk image download: retries with backoff, resume, concurrency, segmented
//! downloads, anti-bot headers, and magic-byte integrity validation.
//!
//! # Using the library
//!
//! The core building blocks are:
//!
//! - [`Args`] — configuration. Parse it from the CLI, or build a value with
//!   `Args::parse_from(...)` and tweak the fields.
//! - [`ClientFactory`] — a pooled HTTP client (anti-bot headers, cookies,
//!   proxy). Build once, then clone it across tasks.
//! - [`Shared`] — run-wide state (early-stop flag, global speed cap).
//! - [`run_job`] — downloads one URL with the full robustness pipeline and
//!   returns a [`JobResult`].
//!
//! Drive jobs with your own concurrency. See `src/main.rs` for the reference
//! orchestration (a `futures::stream` + `buffer_unordered` worker pool).

pub mod args;
pub mod client;
pub mod config;
pub mod cookies;
pub mod download;
pub mod throttle;
pub mod validate;

pub use args::{parse_input_file, Args, InputUrl, BROWSER_UAS};
pub use client::ClientFactory;
pub use config::{Config, DownloadSection, HttpSection};
pub use cookies::Cookie;
pub use download::{run_job, JobResult, JobStatus, Shared, MIN_SEGMENT_SIZE, PART_SUFFIX};
pub use throttle::Throttle;
pub use validate::Format;
