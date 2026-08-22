//! Minimal HTTPS client, implemented over `curl`.
//!
//! Why a subprocess rather than a Rust HTTP crate: this project already drives external
//! tools for the things they do well (MakeMKV, redumper, ffprobe), and curl is the same
//! kind of dependency — universally present, and responsible for TLS, redirects and
//! timeouts. It also keeps the dependency tree small, which matters on the older
//! toolchains this crate supports.
//!
//! **Secrets never appear in the command line.** Request options, including the
//! `Authorization` header, are written to curl's stdin via `--config -`. Passing a
//! bearer token as `-H` would expose it in `ps` output to every user on the machine,
//! and in any process accounting the system keeps.

use std::io::Write;
use std::process::{Command, Stdio};

const TOOL: &str = "curl";

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("curl is not installed or not on PATH")]
    NotInstalled,

    #[error("request to {url} failed: {detail}")]
    Failed { url: String, detail: String },

    #[error("{url} returned HTTP {status}")]
    Status { url: String, status: u16 },

    #[error("i/o error talking to curl: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, HttpError>;

/// Escape a value for curl's config-file syntax, which uses double-quoted strings.
fn quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Perform a GET and return the response body.
///
/// `headers` are sent as-is. The whole request description goes to curl over stdin, so
/// nothing sensitive is visible in the process table.
pub fn get(url: &str, headers: &[(&str, String)], timeout_secs: u32) -> Result<String> {
    let mut child = Command::new(TOOL)
        // Read every other option from stdin. Only this flag is visible in `ps`.
        .arg("--config")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HttpError::NotInstalled
            } else {
                HttpError::Io(e)
            }
        })?;

    {
        let stdin = child.stdin.as_mut().expect("piped");
        let mut cfg = String::new();
        cfg.push_str(&format!("url = {}\n", quote(url)));
        for (k, v) in headers {
            cfg.push_str(&format!("header = {}\n", quote(&format!("{k}: {v}"))));
        }
        cfg.push_str("silent\n");
        cfg.push_str("show-error\n");
        // Report the HTTP status rather than printing an error page as if it were data.
        cfg.push_str("write-out = \"\\n%{http_code}\"\n");
        cfg.push_str(&format!("max-time = {timeout_secs}\n"));
        cfg.push_str("location\n");
        cfg.push_str("retry = 2\n");
        stdin.write_all(cfg.as_bytes())?;
    }

    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(HttpError::Failed {
            url: url.to_string(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout).to_string();
    // The status code was appended on its own final line by write-out.
    let (payload, status) = match body.rsplit_once('\n') {
        Some((p, s)) => (p.to_string(), s.trim().parse::<u16>().unwrap_or(0)),
        None => (body.clone(), 0),
    };

    if !(200..300).contains(&status) {
        return Err(HttpError::Status {
            url: url.to_string(),
            status,
        });
    }
    Ok(payload)
}

/// Percent-encode a query-string value.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_escapes_config_values() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote(r#"has "quotes""#), r#""has \"quotes\"""#);
        assert_eq!(quote(r"back\slash"), r#""back\\slash""#);
    }

    /// A header value containing a newline must not be able to inject extra curl
    /// options: the value is quoted, so a newline stays inside the string.
    #[test]
    fn newlines_in_values_stay_contained() {
        let q = quote("token\nproxy = http://evil");
        assert!(q.starts_with('"') && q.ends_with('"'));
    }

    #[test]
    fn urlencodes_reserved_characters() {
        assert_eq!(urlencode("30 for 30"), "30%20for%2030");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("safe-_.~"), "safe-_.~");
        assert_eq!(urlencode("café"), "caf%C3%A9");
    }
}
