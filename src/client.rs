//! HTTP client construction: anti-bot headers, UA rotation, proxy, cookies.
//!
//! One shared `reqwest::Client` is built once and reused for every request so
//! connections are pooled (keep-alive, HTTP/2 multiplexing) instead of paying
//! a fresh TCP+TLS handshake per file. Per-request differences (User-Agent
//! rotation, per-host cookies) are applied via request headers.

use crate::args::{Args, BROWSER_UAS};
use crate::cookies::{build_cookie_header, parse_cookie_string, parse_netscape_jar, Cookie};
use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

/// Shared client state.
#[derive(Clone)]
pub struct ClientFactory {
    args: Arc<Args>,
    client: reqwest::Client,
    jar_cookies: Vec<Cookie>,
    raw_cookies: Vec<Cookie>,
    ua_index: Arc<AtomicUsize>,
}

impl ClientFactory {
    pub fn new(args: Arc<Args>) -> Result<Self> {
        let raw_cookies = match &args.cookie {
            Some(s) => parse_cookie_string(s)?,
            None => Vec::new(),
        };
        let jar_cookies = match &args.cookie_jar {
            Some(p) => parse_netscape_jar(p)?,
            None => Vec::new(),
        };

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(args.timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(args.connect_timeout_secs.max(1)))
            .redirect(reqwest::redirect::Policy::limited(10))
            .tcp_nodelay(true);

        if args.http1 {
            builder = builder.http1_only();
        }
        if args.insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(proxy) = &args.proxy {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|e| anyhow!("invalid proxy URL {proxy:?}: {e}"))?,
            );
        }

        let client = builder
            .build()
            .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;

        Ok(Self {
            args,
            client,
            jar_cookies,
            raw_cookies,
            ua_index: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Next User-Agent for this request (rotates if --ua-rotate).
    fn next_ua(&self) -> String {
        if self.args.ua_rotate {
            let i = self
                .ua_index
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            BROWSER_UAS[i % BROWSER_UAS.len()].to_string()
        } else {
            self.args
                .user_agent
                .clone()
                .unwrap_or_else(|| DEFAULT_UA.to_string())
        }
    }

    /// Build the headers for one request to `host`: static anti-bot headers,
    /// this request's User-Agent, and a per-host `Cookie` header.
    pub fn prepare(&self, host: &str) -> Result<(reqwest::Client, HeaderMap, Option<String>)> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.next_ua())
                .map_err(|e| anyhow!("invalid user agent: {e}"))?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            ),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        if let Some(referer) = &self.args.referer {
            headers.insert(
                reqwest::header::REFERER,
                HeaderValue::from_str(referer)
                    .map_err(|e| anyhow!("invalid referer {referer:?}: {e}"))?,
            );
        }
        for h in &self.args.header {
            let (name, value) = h
                .split_once(':')
                .ok_or_else(|| anyhow!("header must be \"Name: value\", got {h:?}"))?;
            let name = HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|e| anyhow!("invalid header name {name:?}: {e}"))?;
            let value = HeaderValue::from_str(value.trim())
                .map_err(|e| anyhow!("invalid header value for {name}: {e}"))?;
            headers.insert(name, value);
        }

        let cookie = build_cookie_header(&self.raw_cookies, &self.jar_cookies, host);

        Ok((self.client.clone(), headers, cookie))
    }
}
