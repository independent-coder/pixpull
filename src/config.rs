//! TOML config file support. CLI flags always win over config values.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "download")]
    pub download: DownloadSection,
    #[serde(rename = "http")]
    pub http: HttpSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DownloadSection {
    pub output: Option<String>,
    pub concurrency: Option<usize>,
    pub delay_ms: Option<u64>,
    pub retries: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    pub resume: Option<bool>,
    pub overwrite: Option<bool>,
    pub max_size: Option<u64>,
    pub validate: Option<bool>,
    pub filename: Option<String>,
    pub start_index: Option<u32>,
    pub retry_on_http_errors: Option<bool>,
    pub split: Option<usize>,
    pub max_download_limit: Option<u64>,
    pub max_overall_download_limit: Option<u64>,
    pub max_file_not_found: Option<u32>,
    pub max_total_connections: Option<usize>,
    pub log: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HttpSection {
    pub user_agent: Option<String>,
    pub ua_rotate: Option<bool>,
    pub http1: Option<bool>,
    pub insecure: Option<bool>,
    pub referer: Option<String>,
    pub proxy: Option<String>,
    pub cookie: Option<String>,
    pub cookie_jar: Option<String>,
    /// Extra headers as "Name: value" strings.
    pub headers: Vec<String>,
}

/// Load and parse a TOML config file.
pub fn load(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("invalid config file {}", path.display()))
}

/// Apply config values to the parsed CLI args (CLI wins: only fill in fields
/// that the user did not explicitly set).
pub fn apply_to_args(args: &mut crate::args::Args, cfg: &Config) {
    let d = &cfg.download;
    let h = &cfg.http;

    if args.output == Path::new(".") && d.output.is_some() {
        args.output = Path::new(d.output.as_ref().unwrap()).to_path_buf();
    }
    if args.concurrency == 4 && d.concurrency.is_some() {
        args.concurrency = d.concurrency.unwrap();
    }
    if args.delay_ms == 0 && d.delay_ms.is_some() {
        args.delay_ms = d.delay_ms.unwrap();
    }
    if args.retries == 5 && d.retries.is_some() {
        args.retries = d.retries.unwrap();
    }
    if args.timeout_secs == 60 && d.timeout_secs.is_some() {
        args.timeout_secs = d.timeout_secs.unwrap();
    }
    if args.connect_timeout_secs == 15 && d.connect_timeout_secs.is_some() {
        args.connect_timeout_secs = d.connect_timeout_secs.unwrap();
    }
    if !args.no_resume && d.resume == Some(false) {
        args.no_resume = true;
    }
    if !args.overwrite && d.overwrite == Some(true) {
        args.overwrite = true;
    }
    if args.max_size == 0 && d.max_size.is_some() {
        args.max_size = d.max_size.unwrap();
    }
    if !args.no_validate && d.validate == Some(false) {
        args.no_validate = true;
    }
    if args.filename.is_none() && d.filename.is_some() {
        args.filename = d.filename.clone();
    }
    if args.start_index == 1 && d.start_index.is_some() {
        args.start_index = d.start_index.unwrap();
    }
    if !args.retry_on_http_errors && d.retry_on_http_errors == Some(true) {
        args.retry_on_http_errors = true;
    }
    if args.split == 1 && d.split.is_some() {
        args.split = d.split.unwrap();
    }
    if args.max_download_limit == 0 && d.max_download_limit.is_some() {
        args.max_download_limit = d.max_download_limit.unwrap();
    }
    if args.max_overall_download_limit == 0 && d.max_overall_download_limit.is_some() {
        args.max_overall_download_limit = d.max_overall_download_limit.unwrap();
    }
    if args.max_file_not_found == 0 && d.max_file_not_found.is_some() {
        args.max_file_not_found = d.max_file_not_found.unwrap();
    }
    if args.max_total_connections == 0 && d.max_total_connections.is_some() {
        args.max_total_connections = d.max_total_connections.unwrap();
    }
    if args.log.is_none() && d.log.is_some() {
        args.log = Some(Path::new(d.log.as_ref().unwrap()).to_path_buf());
    }
    if !args.http1 && h.http1 == Some(true) {
        args.http1 = true;
    }
    if !args.insecure && h.insecure == Some(true) {
        args.insecure = true;
    }

    if args.user_agent.is_none() && h.user_agent.is_some() {
        args.user_agent = h.user_agent.clone();
    }
    if !args.ua_rotate && h.ua_rotate == Some(true) {
        args.ua_rotate = true;
    }
    if args.referer.is_none() && h.referer.is_some() {
        args.referer = h.referer.clone();
    }
    if args.proxy.is_none() && h.proxy.is_some() {
        args.proxy = h.proxy.clone();
    }
    if args.cookie.is_none() && h.cookie.is_some() {
        args.cookie = h.cookie.clone();
    }
    if args.cookie_jar.is_none() && h.cookie_jar.is_some() {
        args.cookie_jar = Some(Path::new(h.cookie_jar.as_ref().unwrap()).to_path_buf());
    }
    // Config headers are merged in (CLI headers take precedence by order).
    args.header.splice(0..0, h.headers.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_config() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixpull_test_config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[download]").unwrap();
        writeln!(f, "concurrency = 8").unwrap();
        writeln!(f, "delay_ms = 250").unwrap();
        writeln!(f, "[http]").unwrap();
        writeln!(f, "user_agent = \"TestAgent/1.0\"").unwrap();
        writeln!(f, "headers = [\"X-Requested-With: XMLHttpRequest\"]").unwrap();
        f.sync_all().unwrap();

        let cfg = load(&path).unwrap();
        assert_eq!(cfg.download.concurrency, Some(8));
        assert_eq!(cfg.download.delay_ms, Some(250));
        assert_eq!(cfg.http.user_agent.as_deref(), Some("TestAgent/1.0"));
        assert_eq!(cfg.http.headers.len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
