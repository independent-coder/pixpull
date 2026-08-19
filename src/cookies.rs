//! Cookie support: raw `Cookie: ...` strings and Netscape-format cookie jars
//! (the same format curl/aria2c use with `-b`/`--load-cookies`).

use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;

/// One parsed cookie from a Netscape jar.
#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// May be unused in the future (path-based cookie scoping); kept as
    /// parsed metadata.
    #[allow(dead_code)]
    pub path: Option<String>,
    pub domain: Option<String>,
}

impl Cookie {
    /// Does this cookie apply to the given URL's host?
    pub fn matches_host(&self, host: &str) -> bool {
        match &self.domain {
            None => true,
            Some(d) => {
                let d = d.trim_start_matches('.');
                host == d || host.ends_with(&format!(".{d}"))
            }
        }
    }
}

/// Parse a raw `Cookie:` header value (e.g. `session=abc; theme=dark`).
pub fn parse_cookie_string(s: &str) -> Result<Vec<Cookie>> {
    let mut out = Vec::new();
    for part in s.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("malformed cookie pair: {part:?}"))?;
        out.push(Cookie {
            name: name.trim().to_string(),
            value: value.trim().to_string(),
            domain: None,
            path: None,
        });
    }
    Ok(out)
}

/// Parse a Netscape-format cookie jar file. Lines starting with `#` are
/// comments (except `#HttpOnly_`, which is a real entry marker). Column
/// layout: domain, include_subdomains, path, secure, expiry_epoch, name, value.
pub fn parse_netscape_jar(path: &Path) -> Result<Vec<Cookie>> {
    let content = fs::read_to_string(path)
        .map_err(|e| anyhow!("failed to read cookie jar {}: {e}", path.display()))?;

    let mut out = Vec::new();
    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') && !line.starts_with("#HttpOnly_") {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            // Some jars separate with spaces; try that before giving up.
            let spaced: Vec<&str> = line.split_whitespace().collect();
            if spaced.len() < 7 {
                return Err(anyhow!(
                    "cookie jar {} line {}: expected 7 tab-separated columns, got {}",
                    path.display(),
                    lineno + 1,
                    cols.len()
                ));
            }
            parse_netscape_cols(spaced, &mut out);
        } else {
            parse_netscape_cols(cols, &mut out);
        }
    }
    Ok(out)
}

fn parse_netscape_cols(cols: Vec<&str>, out: &mut Vec<Cookie>) {
    let domain = cols[0].trim_start_matches('#');
    let path = cols[2];
    let expiry: i64 = cols[4].parse().unwrap_or(0);

    // Drop expired cookies (expiry == 0 means session cookie, keep it).
    if expiry != 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if expiry < now {
            return;
        }
    }

    out.push(Cookie {
        name: cols[5].to_string(),
        value: cols[6].to_string(),
        domain: Some(domain.to_string()),
        path: Some(path.to_string()),
    });
}

/// Build a `Cookie:` header value from raw string cookies plus jar cookies
/// that match the target host.
pub fn build_cookie_header(raw: &[Cookie], jar: &[Cookie], host: &str) -> Option<String> {
    let mut parts: Vec<String> = raw
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    for c in jar {
        if c.matches_host(host) {
            parts.push(format!("{}={}", c.name, c.value));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_cookie_string() {
        let cs = parse_cookie_string("session=abc123; theme=dark").unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name, "session");
        assert_eq!(cs[1].value, "dark");
    }

    #[test]
    fn parses_netscape_jar() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixpull_test_cookies.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "# Netscape HTTP Cookie File").unwrap();
        writeln!(f, ".example.com\tTRUE\t/\tFALSE\t9999999999\tsid\txyz").unwrap();
        writeln!(f, ".example.com\tTRUE\t/\tFALSE\t1\texpired\tgone").unwrap();
        writeln!(f, "#HttpOnly_.other.com\tTRUE\t/\tTRUE\t9999999999\ttok\tabc").unwrap();
        f.sync_all().unwrap();

        let cookies = parse_netscape_jar(&path).unwrap();
        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().any(|c| c.name == "sid" && c.value == "xyz"));
        assert!(cookies.iter().all(|c| c.name != "expired"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn host_matching() {
        let jar = parse_cookie_string("a=1").unwrap();
        let header = build_cookie_header(&jar, &[], "img.example.com");
        assert_eq!(header.as_deref(), Some("a=1"));

        let jar = parse_netscape_jar_from_str(
            ".example.com\tTRUE\t/\tFALSE\t9999999999\tsid\txyz\n",
        );
        let header = build_cookie_header(&[], &jar, "cdn.example.com").unwrap();
        assert!(header.contains("sid=xyz"));
        let header = build_cookie_header(&[], &jar, "other.org");
        assert!(header.is_none());
    }

    fn parse_netscape_jar_from_str(s: &str) -> Vec<Cookie> {
        let dir = std::env::temp_dir();
        let path = dir.join("pixpull_test_cookies_str.txt");
        fs::write(&path, s).unwrap();
        let r = parse_netscape_jar(&path).unwrap();
        let _ = fs::remove_file(&path);
        r
    }
}
