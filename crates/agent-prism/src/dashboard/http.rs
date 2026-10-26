//! A minimal HTTP/1.1 server side for the dashboard: one GET request per
//! connection, no keep-alive.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Result, bail};

/// The request head may not be longer than this.
const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Reads and parses the request head. The body (there should be none for
/// a GET) is not read.
pub fn read_request(r: &mut impl Read) -> Result<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let end = loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            bail!("connection closed before the request head ended");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too long");
        }
    };
    parse_head(&String::from_utf8_lossy(&buf[..end]))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn parse_head(head: &str) -> Result<Request> {
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        bail!("bad request line");
    };
    if !version.starts_with("HTTP/1.") {
        bail!("unsupported HTTP version");
    }
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let query = qs
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
        })
        .collect();
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query,
        headers,
    })
}

/// The Host header names this server: `127.0.0.1:<port>` or
/// `localhost:<port>`. Anything else may be a DNS-rebinding page.
pub fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else {
        return false;
    };
    let h = h.to_ascii_lowercase();
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }

    pub fn json(v: &impl serde::Serialize) -> Self {
        match serde_json::to_vec(v) {
            Ok(b) => Self::new(200, "application/json", b),
            Err(e) => Self::text(500, &format!("serialize: {e}")),
        }
    }

    pub fn text(status: u16, s: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", s.as_bytes())
    }

    pub fn write_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\n\
             Cross-Origin-Resource-Policy: same-origin\r\n\
             Connection: close\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        if self.content_type.starts_with("text/html") {
            head.push_str(
                "Content-Security-Policy: default-src 'none'; script-src 'self'; \
                 style-src 'self'; connect-src 'self'; img-src 'self' data:; \
                 base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
                 X-Frame-Options: DENY\r\n",
            );
        }
        if self.status == 405 {
            head.push_str("Allow: GET\r\n");
        }
        head.push_str("\r\n");
        w.write_all(head.as_bytes())?;
        w.write_all(&self.body)?;
        w.flush()
    }
}
