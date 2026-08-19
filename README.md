# pixpull

[![crates.io](https://img.shields.io/crates/v/pixpull.svg)](https://crates.io/crates/pixpull)
[![downloads](https://img.shields.io/crates/d/pixpull.svg)](https://crates.io/crates/pixpull)
[![CI](https://github.com/independent-coder/pixpull/actions/workflows/ci.yml/badge.svg)](https://github.com/independent-coder/pixpull/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE.MD)

Super-robust HTTP/HTTPS **picture** downloader, written from scratch in Rust.
The download engine for your scrapers — no more shelling out to `aria2c`.

## Install

```bash
cargo install pixpull
```

Prebuilt binaries for Windows, macOS, and Linux are attached to each
[GitHub release](https://github.com/independent-coder/pixpull/releases/latest).

## What it does differently

| Feature | Behavior |
|---|---|
| **Retries + backoff** | Exponential backoff with jitter on timeouts, connection drops, 408/429 and all 5xx. Honors the server's `Retry-After` header on 429/503. Optional `--retry-on-http-errors` for flaky CDNs that 403/404 intermittently. |
| **Speed limits** | `--max-download-limit KIB/S` (per file) and `--max-overall-download-limit KIB/S` (global) — token-bucket pacing for politeness/anti-ban, same semantics as aria2c. |
| **Early-stop guards** | `--max-file-not-found N` stops the run after N consecutive 404s (sequential galleries); `--max-total-connections N` caps `concurrency × split` so a huge config can't open hundreds of sockets. |
| **TLS + logging** | `--insecure` skips cert verification for self-signed hosts; `--log FILE` appends a machine-readable run log (timestamp, status, file, bytes, URL). |
| **Resume** | Downloads go to a hidden `.part` file (named by URL hash). On retry or on a later run, it sends `Range: bytes=N-` and appends. Handles `206`/`200`/`416` correctly. Servers without range support just restart cleanly. |
| **Integrity validation** | Magic-byte sniffing for JPEG/PNG/GIF/WebP/BMP/TIFF/AVIF/HEIC/SVG/ICO. Corrupt bodies are **deleted and redownloaded**; a URL's wrong/missing extension is corrected (`wrongext.gif` served as PNG → `wrongext.png`). |
| **Concurrency + rate limiting** | Async worker pool (`--concurrency`) with optional `--delay` between request starts. One shared connection-pooled client (keep-alive + HTTP/2) — a batch of 20 files uses ~9 connections, not 20. |
| **Segmented downloads** | `--split N` fans a large file out into N parallel `Range` requests (aria2c's `-x`). Files under 1 MiB stay single-stream. Per-segment retry + resume. |
| **Anti-bot** | Custom UA, `--ua-rotate` across 6 browser fingerprints, referer, extra headers, raw `--cookie`, Netscape cookie jars, proxy support. TLS via rustls (no OpenSSL dependency). |
| **Safety** | Skip-existing (rerun-safe), `--max-size` guard, filename sanitization, `--overwrite` opt-out, non-zero exit code when anything failed. |

## Build

```bash
cargo build --release        # binary at target/release/pixpull
```

Only the Rust toolchain is needed — no Python, no aria2c, no OpenSSL.

## Usage

```bash
# Basic
pixpull -o pics https://site.com/img/1.jpg https://site.com/img/2.jpg

# URL list file (one per line, # comments; aria2c-compatible)
pixpull -i urls.txt -o pics

# Scraper-style: numbered files, 8 parallel, 250ms spacing, 10 retries
pixpull -i urls.txt -o pics -c 8 --delay 250 --retries 10 \
      --filename "img_{n:04}.{ext}"

# Anti-bot setup
pixpull -i urls.txt --ua-rotate -H "X-Requested-With: XMLHttpRequest" \
      --referer https://site.com --cookie-jar cookies.txt \
      --proxy http://127.0.0.1:8080

# Max speed on a connection-capped CDN (like aria2c -x 16):
# 4 files in parallel, each split across 16 range requests
pixpull -i urls.txt -c 4 --split 16

# Polite / anti-ban scraping: cap global throughput, honor Retry-After,
# stop if the gallery just ends, and keep a run log
pixpull -i urls.txt -c 4 --split 16 --max-overall-download-limit 2048 \
      --max-file-not-found 10 --log run.log

# Self-signed / broken certs
pixpull -i urls.txt --insecure

# Resume a half-finished run (skips done files, resumes .part files)
pixpull -i urls.txt -o pics
```

Filename templates: `{n}` index, `{n:04}` zero-padded, `{ext}` detected
extension. Default is the (sanitized) URL basename.

## `out=` per-URL filenames (aria2c input format)

pixpull reads the same input-file format your aria2c lists use — an `out=` line
under a URL sets that URL's output name (indented or not):

```
https://cdn.example/img/abc123.jpg
  out=img_0001.jpg
https://cdn.example/img/def456.jpg
  out=img_0002.jpg
```

`out=` supports `{n}`/`{ext}` placeholders too, and the magic-byte detector
still corrects a wrong/missing extension (`out=img_0001.jpg` served as PNG
becomes `img_0001.png`). Per-URL `out=` takes precedence over `--filename`.
On reruns, files with a known `out=` name are skipped without a network
request.

## Config file

Everything can live in a TOML config; CLI flags always win:

```toml
[download]
output = "pics"
concurrency = 6
delay_ms = 200
retries = 8
timeout_secs = 60
filename = "img_{n:04}.{ext}"
max_size = 20971520          # skip anything over 20 MB
validate = true
split = 16
max_download_limit = 1024    # KiB/s per file
max_overall_download_limit = 4096  # KiB/s global
max_file_not_found = 10
max_total_connections = 128
log = "run.log"

[http]
user_agent = "Mozilla/5.0 ..."
ua_rotate = true
http1 = true
insecure = false
referer = "https://site.com"
proxy = "http://127.0.0.1:8080"
headers = ["X-Requested-With: XMLHttpRequest"]
cookie = "session=abc; theme=dark"
cookie_jar = "cookies.txt"
```

```bash
pixpull --config pixpull.toml -i urls.txt
```

Cookie jars use the Netscape format (same as `curl -b` / `aria2c --load-cookies`).

## Exit codes

`0` = everything downloaded or skipped; `1` = at least one URL failed.

## Testing

```bash
cargo test                            # unit tests
python e2e_server.py                  # fixture server for manual e2e runs
```

The fixture server simulates real-world failure modes: dropped connections
(resume), transient 500s (retry), and garbage bodies (validation).

## Performance vs aria2c

Measured on a real 58-image batch (same host, ~99 MB), all runs fresh:

| run | time | throughput | validates |
|---|---|---|---|
| pixpull, no connection pooling | 37.4s | 2.5 MiB/s | ✅ |
| pixpull, pooled client | 32.5s | 2.9 MiB/s | ✅ |
| aria2c `-j 4` (1 conn/file) | 24.3s | 4.1 MiB/s | ❌ |
| aria2c `-j 4 -s 16 -x 16` | 14.1s | 6.7 MiB/s | ❌ |
| pixpull `-c 4 --split 16` | 14.1s | 6.7 MiB/s | ✅ |
| **pixpull `-c 4 --split 16 --http1`** | **12.0s** | **7.9 MiB/s** | ✅ |

How to get there:
1. **Connection pooling** — one shared client so keep-alive reuse applies
   across files (20 files ≈ 9 connections instead of 20).
2. **Segmented downloads** (`--split 16`) — parallel `Range` requests per
   file, exactly what aria2c's `-x 16` does. This is the lever for CDNs that
   cap per-connection throughput.
3. **`--http1`** — the CDN above throttles HTTP/2; forcing HTTP/1.1 gained
   another ~17%. If your CDN behaves the same, put `http1 = true` in your
   config file.

The advantage grows with file size — on a 78-file batch of ~6 MB images
(481 MB total) pixpull hit **19.6 MiB/s (23.5s)** vs aria2c's **15.7 MiB/s
(30.7s)**, ~25% faster, because per-request overhead amortizes and the CDN
rewards the extra parallel connections. pixpull also magic-byte-validates every
file, which aria2c does not.

## Notes / limitations

- Content-Disposition filenames are not parsed; URLs without any path
  basename fall back to `image_{n}.{ext}`.
- Validation is magic-byte based (fast, covers all common formats); it does
  not fully decode every pixel.
