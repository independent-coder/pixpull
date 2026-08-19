//! Core download engine: per-file fetch with resume, retries/backoff,
//! size limits, and magic-byte integrity validation.

use crate::args::Args;
use crate::client::ClientFactory;
use crate::throttle::Throttle;
use crate::validate::{extension_matches, validate};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use reqwest::header::{CONTENT_LENGTH, RANGE, RETRY_AFTER};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;

pub const PART_SUFFIX: &str = ".part";

/// Files at least this large benefit from segmented download.
pub const MIN_SEGMENT_SIZE: u64 = 1_048_576; // 1 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Downloaded,
    Skipped,
    Failed,
}

pub struct JobResult {
    pub url: String,
    pub file: Option<PathBuf>,
    pub bytes: u64,
    pub status: JobStatus,
    pub detail: String,
    /// True when this job failed with HTTP 404 (used by --max-file-not-found).
    pub not_found: bool,
}

/// State shared across all concurrent jobs.
#[derive(Clone, Default)]
pub struct Shared {
    /// Set when the run should stop early (e.g. too many consecutive 404s).
    pub stop: Arc<AtomicBool>,
    /// Global download speed cap shared by every job (None = unlimited).
    pub global_throttle: Option<Throttle>,
}

impl Shared {
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// A file-level outcome that we may retry.
enum Attempt {
    Done(PathBuf, u64),
    /// Retry. `keep_part` = keep the partial file so the next attempt can
    /// resume it (stream interruptions); when false the part is deleted.
    /// `retry_after` = seconds the server asked us to wait (Retry-After).
    Retry {
        reason: String,
        keep_part: bool,
        retry_after: Option<u64>,
    },
    /// Give up: permanent error. `not_found` marks HTTP 404 for
    /// --max-file-not-found.
    GiveUp {
        reason: String,
        not_found: bool,
    },
}

/// Download a single image URL. `out` is an optional per-URL filename from
/// an aria2c-style `out=` line in the input file (may contain `{n}`/`{ext}`
/// placeholders). Never panics; every failure is reported in the returned
/// `JobResult`.
pub async fn run_job(
    factory: &ClientFactory,
    args: &Args,
    shared: &Shared,
    url: &str,
    index: u32,
    out: Option<String>,
) -> JobResult {
    if shared.should_stop() {
        return JobResult {
            url: url.to_string(),
            file: None,
            bytes: 0,
            status: JobStatus::Failed,
            detail: "run stopped (--max-file-not-found)".to_string(),
            not_found: false,
        };
    }

    let parsed = match url::Url::parse(url) {
        Ok(u) if u.host_str().is_some() => u,
        Ok(_) => {
            return JobResult {
                url: url.to_string(),
                file: None,
                bytes: 0,
                status: JobStatus::Failed,
                detail: "URL has no host".to_string(),
                not_found: false,
            }
        }
        Err(e) => {
            return JobResult {
                url: url.to_string(),
                file: None,
                bytes: 0,
                status: JobStatus::Failed,
                detail: format!("invalid URL: {e}"),
                not_found: false,
            }
        }
    };

    let host = parsed.host_str().unwrap_or("").to_string();
    let output = args.output.clone();
    if let Err(e) = tokio::fs::create_dir_all(&output).await {
        return JobResult {
            url: url.to_string(),
            file: None,
            bytes: 0,
            status: JobStatus::Failed,
            detail: format!("cannot create output dir: {e}"),
            not_found: false,
        };
    }

    // Per-file speed cap shared across this file's segments (None = unlimited).
    let file_throttle = if args.max_download_limit > 0 {
        Some(Throttle::new(args.max_download_limit as f64 * 1024.0))
    } else {
        None
    };

    // --- Filename planning -------------------------------------------------
    // Stable .part name for cross-run resume: hash of the URL.
    let part_path = output.join(format!(".{:016x}{PART_SUFFIX}", stable_hash(url)));

    // Filename plan precedence: out= (per-URL) > --filename template > URL
    // basename. `out=` and templates may contain {n}/{ext} placeholders.
    let plan = if let Some(out) = out {
        NamePlan::Template(out)
    } else if let Some(tmpl) = &args.filename {
        NamePlan::Template(tmpl.clone())
    } else {
        match url_basename(&parsed) {
            Some(base) => NamePlan::Fixed(base),
            None => NamePlan::Probe,
        }
    };

    // Early skip: final name is already known (no placeholders), so we can
    // avoid a network request entirely for already-downloaded files.
    if let Some(name) = plan.known_name() {
        if let Some(outcome) = check_existing(&output.join(name), args, url) {
            cleanup_part(&part_path).await;
            return outcome;
        }
    }

    // --- Retry loop ---------------------------------------------------------
    let mut attempt: u32 = 0;
    loop {
        match attempt_once(
            factory,
            args,
            shared,
            url,
            &host,
            &part_path,
            &file_throttle,
        )
        .await
        {
            Ok(Attempt::Done(part_path_done, bytes)) => {
                debug_assert_eq!(part_path_done, part_path);
                // Integrity validation (magic bytes) + extension fix-up.
                let det = match validate_file(&part_path).await {
                    Ok(d) => d,
                    Err(e) => {
                        let msg = format!("validation failed: {e}");
                        if attempt < args.retries {
                            // Corrupt data: delete the part, redownload fresh.
                            let _ = tokio::fs::remove_file(&part_path).await;
                            attempt += 1;
                            tokio::time::sleep(backoff(attempt, None)).await;
                            continue;
                        }
                        return JobResult {
                            url: url.to_string(),
                            file: None,
                            bytes,
                            status: JobStatus::Failed,
                            detail: msg,
                            not_found: false,
                        };
                    }
                };

                // Resolve the final path now that we know the format.
                let mut final_path = match &plan {
                    NamePlan::Template(_) | NamePlan::Probe => {
                        output.join(plan.resolve(det, index))
                    }
                    NamePlan::Fixed(name) => output.join(name),
                };
                if !extension_matches(&final_path.to_string_lossy(), det) {
                    final_path = fix_extension(&final_path, det);
                }

                // If the resolved name collides with an existing file, honour
                // the skip rule (we already spent the bandwidth, but keeping
                // data is safer than clobbering).
                if !args.overwrite && final_path.exists() {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    return JobResult {
                        url: url.to_string(),
                        file: Some(final_path),
                        bytes,
                        status: JobStatus::Skipped,
                        detail: "target already exists".to_string(),
                        not_found: false,
                    };
                }

                if let Err(e) = tokio::fs::rename(&part_path, &final_path).await {
                    return JobResult {
                        url: url.to_string(),
                        file: None,
                        bytes,
                        status: JobStatus::Failed,
                        detail: format!("rename to final name failed: {e}"),
                        not_found: false,
                    };
                }
                return JobResult {
                    url: url.to_string(),
                    file: Some(final_path),
                    bytes,
                    status: JobStatus::Downloaded,
                    detail: String::new(),
                    not_found: false,
                };
            }
            Ok(Attempt::Retry {
                reason,
                keep_part,
                retry_after,
            }) => {
                if !keep_part {
                    let _ = tokio::fs::remove_file(&part_path).await;
                }
                if attempt < args.retries {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt, retry_after)).await;
                    continue;
                }
                return JobResult {
                    url: url.to_string(),
                    file: None,
                    bytes: 0,
                    status: JobStatus::Failed,
                    detail: reason,
                    not_found: false,
                };
            }
            Ok(Attempt::GiveUp { reason, not_found }) => {
                let _ = tokio::fs::remove_file(&part_path).await;
                return JobResult {
                    url: url.to_string(),
                    file: None,
                    bytes: 0,
                    status: JobStatus::Failed,
                    detail: reason,
                    not_found,
                };
            }
            Err(e) => {
                let msg = format!("internal error: {e}");
                if attempt < args.retries {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                return JobResult {
                    url: url.to_string(),
                    file: None,
                    bytes: 0,
                    status: JobStatus::Failed,
                    detail: msg,
                    not_found: false,
                };
            }
        }
    }
}

/// One full-file attempt. Handles resume, streaming, and response codes.
async fn attempt_once(
    factory: &ClientFactory,
    args: &Args,
    shared: &Shared,
    url: &str,
    host: &str,
    part_path: &Path,
    file_throttle: &Option<Throttle>,
) -> Result<Attempt> {
    let (client, headers, cookie) = factory.prepare(host)?;

    // --- Segmented download (--split N > 1) --------------------------------
    // Probe with a 1-byte Range request to learn total size + range support.
    // If the file is big enough and the server honours ranges, fan out into
    // parallel per-segment downloads; otherwise fall through to the single
    // stream path below.
    if args.split > 1 {
        if let Some((total, range_ok)) =
            probe_range(&client, url, &headers, cookie.as_deref()).await
        {
            if range_ok && total >= MIN_SEGMENT_SIZE {
                if let Some(att) = download_segmented(
                    &client,
                    args,
                    shared,
                    url,
                    &headers,
                    cookie.as_deref(),
                    part_path,
                    total,
                    file_throttle,
                )
                .await?
                {
                    return Ok(att);
                }
                // None => server ignored a segment's range mid-flight; fall
                // through to the single stream path.
            }
        }
    }

    let resume = !args.no_resume;

    let existing = if resume {
        tokio::fs::metadata(part_path).await.ok().map(|m| m.len())
    } else {
        None
    };

    let mut req = client.get(url).headers(headers);
    if let Some(c) = &cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    if let Some(start) = existing.filter(|s| *s > 0) {
        req = req.header(RANGE, format!("bytes={start}-"));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Attempt::Retry {
                reason: format!("request failed: {e}"),
                keep_part: true,
                retry_after: None,
            })
        }
    };

    let status = resp.status();
    let retry_after = parse_retry_after(resp.headers());

    // --- Response classification ------------------------------------------
    if status.is_redirection() {
        // reqwest follows redirects automatically; reaching here means the
        // policy gave up (too many hops).
        return Ok(Attempt::GiveUp {
            reason: format!("too many redirects ({status})"),
            not_found: false,
        });
    }
    if status == StatusCode::NOT_MODIFIED {
        return Ok(Attempt::GiveUp {
            reason: "server returned 304 Not Modified".to_string(),
            not_found: false,
        });
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // Our resume offset is at/after the end of the file: treat the
        // existing .part as complete.
        return validate_existing_part(part_path).await;
    }
    if status == StatusCode::PARTIAL_CONTENT && existing.unwrap_or(0) > 0 {
        // Good: server accepted resume.
    } else if status == StatusCode::OK {
        // Full content. If we previously had a partial file, the server does
        // not support Range — start over.
        if existing.unwrap_or(0) > 0 {
            let _ = tokio::fs::remove_file(part_path).await;
        }
    } else if status.is_client_error() {
        let retryable = matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        ) || (args.retry_on_http_errors
            && matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND));
        if retryable {
            return Ok(Attempt::Retry {
                reason: format!("HTTP {status}"),
                keep_part: true,
                retry_after,
            });
        }
        return Ok(Attempt::GiveUp {
            reason: format!("HTTP {status}"),
            not_found: status == StatusCode::NOT_FOUND,
        });
    } else if status.is_server_error() {
        return Ok(Attempt::Retry {
            reason: format!("HTTP {status}"),
            keep_part: true,
            retry_after,
        });
    } else {
        return Ok(Attempt::GiveUp {
            reason: format!("unexpected HTTP {status}"),
            not_found: false,
        });
    }

    // --- Size limits -------------------------------------------------------
    let declared: Option<u64> = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    if args.max_size > 0 {
        if let Some(len) = declared {
            if len > args.max_size {
                return Ok(Attempt::GiveUp {
                    reason: format!("size {len} exceeds --max-size {}", args.max_size),
                    not_found: false,
                });
            }
        }
    }

    // --- Streaming ---------------------------------------------------------
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part_path)
        .await
        .map_err(|e| anyhow!("cannot open {part_path:?}: {e}"))?;

    let mut total = existing.unwrap_or(0);
    if status == StatusCode::OK {
        total = 0;
    }

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = file.shutdown().await;
                return Ok(Attempt::Retry {
                    reason: format!("connection interrupted: {e}"),
                    keep_part: true,
                    retry_after: None,
                });
            }
        };
        if args.max_size > 0 && total + chunk.len() as u64 > args.max_size {
            return Ok(Attempt::GiveUp {
                reason: format!("download exceeds --max-size {}", args.max_size),
                not_found: false,
            });
        }
        // Apply speed caps before writing this chunk.
        if let Some(t) = file_throttle {
            t.acquire(chunk.len() as u64).await;
        }
        if let Some(t) = &shared.global_throttle {
            t.acquire(chunk.len() as u64).await;
        }
        if let Err(e) = file.write_all(&chunk).await {
            return Ok(Attempt::Retry {
                reason: format!("disk write failed: {e}"),
                keep_part: false,
                retry_after: None,
            });
        }
        total += chunk.len() as u64;
    }
    file.flush()
        .await
        .map_err(|e| anyhow!("flush failed: {e}"))?;

    if total == 0 {
        return Ok(Attempt::GiveUp {
            reason: "server returned an empty body".to_string(),
            not_found: false,
        });
    }

    Ok(Attempt::Done(part_path.to_path_buf(), total))
}

/// Parse the `Retry-After` header (integer seconds or HTTP-date). Returns
/// `None` when absent or unparseable.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs);
    }
    let when = httpdate::parse_http_date(v).ok()?;
    let now = SystemTime::now();
    when.duration_since(now).ok().map(|d| d.as_secs())
}

/// Validate the .part file content and return the detected format. Only the
/// first few KB are read — magic bytes live at the start of the file, so we
/// avoid re-reading the whole download from disk.
async fn validate_file(part_path: &Path) -> Result<crate::validate::Format> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(part_path)
        .await
        .map_err(|e| anyhow!("cannot open downloaded file: {e}"))?;
    let mut head = vec![0u8; 4096];
    let n = f
        .read(&mut head)
        .await
        .map_err(|e| anyhow!("read failed: {e}"))?;
    head.truncate(n);
    validate(&head).map_err(anyhow::Error::msg)
}

/// Probe a URL with `Range: bytes=0-0`. Returns `(total_size, range_ok)`.
/// `None` means the probe request itself failed (caller falls back to the
/// single-stream path, which has its own retry logic).
async fn probe_range(
    client: &reqwest::Client,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    cookie: Option<&str>,
) -> Option<(u64, bool)> {
    let mut req = client.get(url).headers(headers.clone());
    if let Some(c) = cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    req = req.header(RANGE, "bytes=0-0");
    let resp = req.send().await.ok()?;
    if resp.status() == StatusCode::PARTIAL_CONTENT {
        let total = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Some((total, total > 0))
    } else {
        // Server ignored the Range header (no range support).
        Some((0, false))
    }
}

/// Download a file in parallel segments. Returns:
/// - `Ok(Some(Attempt))` when the segmented path finished (Done or GiveUp),
/// - `Ok(None)` when a server ignored a range mid-flight (fall back),
/// - `Err` on internal IO errors.
#[allow(clippy::too_many_arguments)]
async fn download_segmented(
    client: &reqwest::Client,
    args: &Args,
    shared: &Shared,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    cookie: Option<&str>,
    part_path: &Path,
    total: u64,
    file_throttle: &Option<Throttle>,
) -> Result<Option<Attempt>> {
    // Drop any stale single-stream part file from a previous config.
    let _ = tokio::fs::remove_file(part_path).await;

    let n = (args.split as u64).min(total / MIN_SEGMENT_SIZE + 1).max(1) as usize;
    let seg_size = total.div_ceil(n as u64);

    let mut seg_paths = Vec::with_capacity(n);
    let mut ranges = Vec::with_capacity(n);
    for i in 0..n {
        let start = i as u64 * seg_size;
        let end = ((i as u64 + 1) * seg_size - 1).min(total - 1);
        ranges.push((start, end));
        seg_paths.push(part_path.with_extension(format!("part.seg{i}")));
    }

    let mut handles = Vec::with_capacity(n);
    for ((s, e), p) in ranges.iter().zip(seg_paths.iter()) {
        handles.push(download_segment(
            client,
            args,
            shared,
            url,
            headers,
            cookie,
            *s,
            *e,
            p,
            file_throttle,
        ));
    }
    let results = futures::future::join_all(handles).await;
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(true) => {}
            Ok(false) => {
                // Range ignored mid-flight: clean up and fall back.
                for p in &seg_paths {
                    let _ = tokio::fs::remove_file(p).await;
                }
                return Ok(None);
            }
            Err(e) => {
                for p in &seg_paths {
                    let _ = tokio::fs::remove_file(p).await;
                }
                return Ok(Some(Attempt::GiveUp {
                    reason: format!("segment {i} failed: {e}"),
                    not_found: false,
                }));
            }
        }
    }

    // Concatenate segments in order into the part file.
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(part_path)
        .await
        .map_err(|e| anyhow!("cannot create {part_path:?}: {e}"))?;
    for p in &seg_paths {
        let mut seg = tokio::fs::File::open(p)
            .await
            .map_err(|e| anyhow!("cannot open segment {p:?}: {e}"))?;
        tokio::io::copy(&mut seg, &mut out)
            .await
            .map_err(|e| anyhow!("concat failed: {e}"))?;
        let _ = tokio::fs::remove_file(p).await;
    }
    out.flush()
        .await
        .map_err(|e| anyhow!("flush failed: {e}"))?;

    Ok(Some(Attempt::Done(part_path.to_path_buf(), total)))
}

/// Download one byte range `[start, end]` into `path` with its own retries.
/// Returns `Ok(true)` when the segment is complete, `Ok(false)` when the
/// server ignored the Range header (whole file must fall back to one stream),
/// and `Err` when the segment failed after exhausting retries.
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    client: &reqwest::Client,
    args: &Args,
    shared: &Shared,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    cookie: Option<&str>,
    start: u64,
    end: u64,
    path: &Path,
    file_throttle: &Option<Throttle>,
) -> Result<bool> {
    let expected = end - start + 1;

    // Resume: if a previous run already completed this segment, skip it.
    if let Ok(m) = tokio::fs::metadata(path).await {
        if m.len() == expected {
            return Ok(true);
        }
    }

    let mut attempt: u32 = 0;
    loop {
        let mut req = client.get(url).headers(headers.clone());
        if let Some(c) = cookie {
            req = req.header(reqwest::header::COOKIE, c);
        }
        req = req.header(RANGE, format!("bytes={start}-{end}"));

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if attempt < args.retries {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                return Err(anyhow!("segment request failed: {e}"));
            }
        };

        let status = resp.status();
        let retry_after = parse_retry_after(resp.headers());
        if status == StatusCode::PARTIAL_CONTENT {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
                .await
                .map_err(|e| anyhow!("cannot open segment {path:?}: {e}"))?;
            let mut stream = resp.bytes_stream();
            let mut got: u64 = 0;
            let mut interrupted = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        got += c.len() as u64;
                        // Apply speed caps before writing this chunk.
                        if let Some(t) = file_throttle {
                            t.acquire(c.len() as u64).await;
                        }
                        if let Some(t) = &shared.global_throttle {
                            t.acquire(c.len() as u64).await;
                        }
                        if let Err(e) = file.write_all(&c).await {
                            return Err(anyhow!("segment write failed: {e}"));
                        }
                    }
                    Err(_) => {
                        interrupted = true;
                        break;
                    }
                }
            }
            file.flush()
                .await
                .map_err(|e| anyhow!("flush failed: {e}"))?;
            if !interrupted && got == expected {
                return Ok(true);
            }
            // Incomplete: retry this segment from scratch.
            let _ = tokio::fs::remove_file(path).await;
            if attempt < args.retries {
                attempt += 1;
                tokio::time::sleep(backoff(attempt, None)).await;
                continue;
            }
            return Err(anyhow!(
                "segment {start}-{end} incomplete ({got}/{expected} bytes)"
            ));
        } else if status == StatusCode::OK {
            // Server ignored the range and sent the whole body.
            return Ok(false);
        } else if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            if attempt < args.retries {
                attempt += 1;
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }
            return Err(anyhow!("segment HTTP {status}"));
        } else {
            return Err(anyhow!("segment HTTP {status}"));
        }
    }
}

/// For the 416 case: the .part file is already complete, validate it.
async fn validate_existing_part(part_path: &Path) -> Result<Attempt> {
    let meta = match tokio::fs::metadata(part_path).await {
        Ok(m) => m,
        Err(_) => {
            return Ok(Attempt::GiveUp {
                reason: "server rejected range but no partial file exists".to_string(),
                not_found: false,
            })
        }
    };
    if meta.len() == 0 {
        return Ok(Attempt::GiveUp {
            reason: "server rejected range on empty file".to_string(),
            not_found: false,
        });
    }
    let bytes = meta.len();
    Ok(Attempt::Done(part_path.to_path_buf(), bytes))
}

/// Skip decision when the final file already exists.
fn check_existing(final_path: &Path, args: &Args, url: &str) -> Option<JobResult> {
    if args.overwrite {
        return None;
    }
    if final_path.exists() {
        Some(JobResult {
            url: url.to_string(),
            file: Some(final_path.to_path_buf()),
            bytes: 0,
            status: JobStatus::Skipped,
            detail: "already exists".to_string(),
            not_found: false,
        })
    } else {
        None
    }
}

async fn cleanup_part(part_path: &Path) {
    let _ = tokio::fs::remove_file(part_path).await;
}

/// Exponential backoff with jitter; honours `retry_after` seconds if given.
fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    let base = Duration::from_secs(1).saturating_mul(1u32 << attempt.min(6));
    let jitter = Duration::from_millis(rand::random::<u64>() % 500);
    let d = base + jitter;
    match retry_after {
        Some(secs) if secs > 0 => d.max(Duration::from_secs(secs)),
        _ => d,
    }
}

/// A stable, content-addressed-ish hash for .part naming.
fn stable_hash(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Extract a sane basename from a URL path (percent-decoded).
fn url_basename(url: &url::Url) -> Option<String> {
    let seg = url.path_segments()?.next_back().filter(|s| !s.is_empty())?;
    let base = sanitize(seg);
    if base.is_empty() {
        None
    } else {
        Some(base)
    }
}

/// Remove characters that are illegal or dangerous in filenames.
fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.starts_with('.') || out.starts_with(' ') {
        out.remove(0);
    }
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.len() > 200 {
        let cut = out
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(out.len());
        out.truncate(cut);
    }
    out
}

/// Replace the extension of `path` with the canonical one for `format`.
fn fix_extension(path: &Path, format: crate::validate::Format) -> PathBuf {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let new_name = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => format!("{stem}.{}", format.ext()),
        _ => format!("{name}.{}", format.ext()),
    };
    path.with_file_name(new_name)
}

/// How the final filename is decided.
enum NamePlan {
    /// User template, rendered once the format is known.
    Template(String),
    /// Fixed name from the URL basename.
    Fixed(String),
    /// No basename available: probe Content-Disposition (not implemented in
    /// this pass — falls back to a numbered name).
    Probe,
}

impl NamePlan {
    /// The final name when it can be known without a request (no `{...}`
    /// placeholders). `None` means we must wait for the format to be detected.
    fn known_name(&self) -> Option<String> {
        match self {
            NamePlan::Fixed(name) => Some(name.clone()),
            NamePlan::Template(t) if !t.contains('{') => Some(sanitize(t)),
            _ => None,
        }
    }

    fn resolve(&self, format: crate::validate::Format, index: u32) -> String {
        match self {
            NamePlan::Template(tmpl) => render_template(tmpl, format, index),
            NamePlan::Fixed(name) => name.clone(),
            NamePlan::Probe => format!("image_{index:04}.{}", format.ext()),
        }
    }
}

/// Render a filename template: {n}, {n:04} (any width), {ext}.
fn render_template(tmpl: &str, format: crate::validate::Format, index: u32) -> String {
    let mut out = String::with_capacity(tmpl.len());
    let chars: Vec<char> = tmpl.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == '}') {
                let expr: String = chars[i + 1..i + 1 + close].iter().collect();
                match expr.as_str() {
                    "ext" => out.push_str(format.ext()),
                    "n" => out.push_str(&index.to_string()),
                    e if e.starts_with("n:") => {
                        let width: usize = e[2..].parse().unwrap_or(0);
                        out.push_str(&format!("{index:0width$}"));
                    }
                    _ => {
                        out.push('{');
                        out.push_str(&expr);
                        out.push('}');
                    }
                }
                i += close + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    sanitize(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_rendering() {
        assert_eq!(
            render_template("img_{n}.{ext}", crate::validate::Format::Png, 3),
            "img_3.png"
        );
        // {ext} is only substituted when the template asks for it; otherwise
        // fix_extension() appends the detected extension at the caller.
        assert_eq!(
            render_template("img_{n:04}", crate::validate::Format::Jpeg, 7),
            "img_0007"
        );
        assert_eq!(
            render_template("set/{n}", crate::validate::Format::Jpeg, 1),
            "set_1"
        );
    }

    #[test]
    fn sanitization() {
        assert_eq!(sanitize("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize("..hidden"), "hidden");
        assert_eq!(sanitize("trailing. "), "trailing");
    }

    #[test]
    fn extension_fixing() {
        let p = Path::new("/tmp/photo.gif");
        assert_eq!(
            fix_extension(p, crate::validate::Format::Jpeg),
            Path::new("/tmp/photo.jpg")
        );
        let p = Path::new("/tmp/photoless");
        assert_eq!(
            fix_extension(p, crate::validate::Format::Jpeg),
            Path::new("/tmp/photoless.jpg")
        );
    }

    #[test]
    fn backoff_grows_and_jitters() {
        // attempt N => base 2^N seconds + up to 500ms jitter.
        let b1 = backoff(1, None).as_millis();
        let b2 = backoff(2, None).as_millis();
        assert!((2000..=2500).contains(&b1));
        assert!((4000..=4500).contains(&b2));
        let capped = backoff(9, None).as_millis();
        assert!(capped <= 64000 + 500);
        // Retry-After wins when it is longer than the computed backoff.
        assert_eq!(backoff(1, Some(30)).as_secs(), 30);
    }
}
