//! CLI argument definitions (clap derive) plus defaults.

use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "picdl",
    version,
    about = "Super-robust HTTP/HTTPS picture downloader",
    long_about = "Bulk-download images with retries/backoff, resume support, \
concurrency, anti-bot headers, and magic-byte integrity validation."
)]
pub struct Args {
    /// Image URLs to download (one or more)
    #[arg(value_name = "URL")]
    pub urls: Vec<String>,

    /// Read URLs from a file (one per line, '#' starts a comment)
    #[arg(short, long, value_name = "FILE")]
    pub input: Option<PathBuf>,

    /// Output directory (created if missing)
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    pub output: PathBuf,

    /// Number of parallel downloads
    #[arg(short, long, value_name = "N", default_value_t = 4)]
    pub concurrency: usize,

    /// Segmented download: parallel Range requests per file (like aria2c -x).
    /// Files below the 1 MiB split threshold are downloaded in one stream.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub split: usize,

    /// Force HTTP/1.1 instead of negotiating HTTP/2 (some CDNs throttle h2)
    #[arg(long)]
    pub http1: bool,

    /// Skip TLS certificate verification (self-signed / broken certs)
    #[arg(long)]
    pub insecure: bool,

    /// Per-file download speed cap in KiB/s (0 = unlimited, aria2c semantics)
    #[arg(long, value_name = "KIB/S", default_value_t = 0)]
    pub max_download_limit: u64,

    /// Global download speed cap across all files in KiB/s (0 = unlimited)
    #[arg(long, value_name = "KIB/S", default_value_t = 0)]
    pub max_overall_download_limit: u64,

    /// Stop the whole run after N consecutive 404s (0 = never stop)
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub max_file_not_found: u32,

    /// Cap on total concurrent connections (concurrency x split). 0 = no cap.
    /// Split is reduced as needed to respect the cap.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub max_total_connections: usize,

    /// Append a machine-readable run log to FILE
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// TOML config file (CLI flags override it)
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,


    /// Minimum delay in ms between request starts (rate limiting)
    #[arg(long = "delay", visible_alias = "delay-ms", value_name = "MS", default_value_t = 0)]
    pub delay_ms: u64,

    /// Max retry attempts per file (retries on 408/429/5xx/timeouts/network errors)
    #[arg(long, value_name = "N", default_value_t = 5)]
    pub retries: u32,

    /// Per-request timeout in seconds
    #[arg(long, value_name = "SECS", default_value_t = 60)]
    pub timeout_secs: u64,

    /// Connection timeout in seconds
    #[arg(long, value_name = "SECS", default_value_t = 15)]
    pub connect_timeout_secs: u64,

    /// User-Agent header value
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,

    /// Rotate through a built-in list of browser User-Agents
    #[arg(long)]
    pub ua_rotate: bool,

    /// Extra header, format "Name: value" (repeatable)
    #[arg(short = 'H', long, value_name = "K:V")]
    pub header: Vec<String>,

    /// Referer header value
    #[arg(long, value_name = "URL")]
    pub referer: Option<String>,

    /// Raw Cookie header value, e.g. "session=abc; theme=dark"
    #[arg(long, value_name = "COOKIES")]
    pub cookie: Option<String>,

    /// Netscape-format cookie jar file (same format as curl -b)
    #[arg(long, value_name = "FILE")]
    pub cookie_jar: Option<PathBuf>,

    /// Proxy URL, e.g. http://127.0.0.1:8080
    #[arg(long, value_name = "URL")]
    pub proxy: Option<String>,

    /// Disable resume of partially downloaded files (.part files)
    #[arg(long)]
    pub no_resume: bool,

    /// Re-download files that already exist (default: skip them)
    #[arg(long)]
    pub overwrite: bool,

    /// Retry on HTTP 403/404 too (some CDNs reject intermittently)
    #[arg(long)]
    pub retry_on_http_errors: bool,

    /// Skip files larger than this many bytes (0 = no limit)
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub max_size: u64,

    /// Filename template: {n} = index, {n:04} = zero-padded, {ext} = detected
    /// extension. Default: URL basename (sanitized).
    #[arg(long, value_name = "TMPL")]
    pub filename: Option<String>,

    /// Starting index for {n} in the filename template
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub start_index: u32,

    /// Disable magic-byte image validation (default: on)
    #[arg(long)]
    pub no_validate: bool,

    /// Only print failures and the final summary
    #[arg(long)]
    pub quiet: bool,
}

/// One URL from the command line or input file, with an optional `out=`
/// filename (aria2c input-file format).
#[derive(Debug, Clone)]
pub struct InputUrl {
    pub url: String,
    pub out: Option<String>,
}

impl Args {
    /// All URLs from positional args plus the input file (if any), in order,
    /// with per-URL `out=` filenames applied. Matches aria2c's input format:
    /// an `out=...` line (indented or not) following a URL sets that URL's
    /// output filename. `{n}`/`{ext}` placeholders are resolved later.
    pub fn all_inputs(&self) -> Vec<InputUrl> {
        let mut out = Vec::new();
        for url in &self.urls {
            out.push(InputUrl {
                url: url.clone(),
                out: None,
            });
        }
        if let Some(path) = &self.input {
            out.extend(parse_input_file(path));
        }
        out
    }
}

/// Parse an aria2c-style input file into URLs with their `out=` filenames.
/// Returns an empty vec if the file cannot be read.
pub fn parse_input_file(path: &Path) -> Vec<InputUrl> {
    let mut out = Vec::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("out=") {
            if let Some(last) = out.last_mut() {
                last.out = Some(name.trim().to_string());
            }
            continue;
        }
        out.push(InputUrl {
            url: line.to_string(),
            out: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_out_lines_from_input_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("picdl_test_input.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "https://a.example/1.jpg").unwrap();
        writeln!(f, "  out=img_0001.jpg").unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "https://a.example/2.jpg").unwrap();
        writeln!(f, "out=img_0002.jpg").unwrap();
        writeln!(f, "https://a.example/3.jpg").unwrap();
        f.sync_all().unwrap();

        let inputs = parse_input_file(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].url, "https://a.example/1.jpg");
        assert_eq!(inputs[0].out.as_deref(), Some("img_0001.jpg"));
        assert_eq!(inputs[1].url, "https://a.example/2.jpg");
        assert_eq!(inputs[1].out.as_deref(), Some("img_0002.jpg"));
        assert_eq!(inputs[2].url, "https://a.example/3.jpg");
        assert_eq!(inputs[2].out, None);
    }
}

/// Default browser User-Agents used for --ua-rotate.
pub const BROWSER_UAS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:137.0) Gecko/20100101 Firefox/137.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.4 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
];
