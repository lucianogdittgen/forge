//! A minimal HTTP/1.1 server, sized for exactly one job.
//!
//! Forge's MCP endpoint is loopback-only, single-client, and speaks one content
//! type. A general HTTP stack would be several hundred kilobytes of dependency
//! to carry a few JSON objects across a socket that never leaves the machine,
//! so the handful of features that endpoint actually needs are implemented
//! here: request line, headers, `Content-Length` and chunked bodies, and
//! keep-alive. Anything else is answered with a status code rather than
//! guessed at.

use std::collections::HashMap;

use anyhow::{bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Refuse absurd requests rather than allocating for them. The only client is
/// the agent process, and its largest message is a tool call.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body,
        }
    }

    /// A JSON-RPC notification has no reply. 202 says "accepted, nothing to
    /// say" — returning an empty 200 would make some clients try to parse it.
    pub fn accepted() -> Self {
        Self {
            status: 202,
            content_type: "text/plain",
            body: Vec::new(),
        }
    }

    pub fn status(code: u16, msg: &str) -> Self {
        Self {
            status: code,
            content_type: "text/plain",
            body: msg.as_bytes().to_vec(),
        }
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            _ => "Internal Server Error",
        }
    }

    pub async fn write<W: tokio::io::AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            self.status,
            self.reason(),
            self.content_type,
            self.body.len()
        );
        w.write_all(head.as_bytes()).await?;
        w.write_all(&self.body).await?;
        w.flush().await?;
        Ok(())
    }
}

/// Read one request. `Ok(None)` means the peer closed cleanly between requests.
pub async fn read_request(
    r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<Request>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    // Tolerate leading blank lines, which some clients send between requests.
    while line.trim().is_empty() {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        bail!("malformed request line");
    }

    let mut headers = HashMap::new();
    let mut header_bytes = line.len();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 {
            bail!("connection closed inside headers");
        }
        header_bytes += h.len();
        if header_bytes > MAX_HEADER_BYTES {
            bail!("headers too large");
        }
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

    let body = if chunked {
        read_chunked(r).await?
    } else {
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if len > MAX_BODY_BYTES {
            bail!("body too large");
        }
        let mut b = vec![0u8; len];
        r.read_exact(&mut b).await?;
        b
    };

    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

/// Chunked bodies are not hypothetical here: Node's HTTP client uses them for
/// streamed request bodies, so a `Content-Length`-only reader would hang.
async fn read_chunked(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if r.read_line(&mut size_line).await? == 0 {
            bail!("connection closed inside chunked body");
        }
        let size_str = size_line.trim();
        // Chunk extensions after ';' are legal and ignorable.
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let n = usize::from_str_radix(size_str, 16)
            .map_err(|_| anyhow::anyhow!("bad chunk size {size_str:?}"))?;
        if body.len() + n > MAX_BODY_BYTES {
            bail!("body too large");
        }
        if n == 0 {
            // Consume the trailer section.
            loop {
                let mut t = String::new();
                if r.read_line(&mut t).await? == 0 || t.trim().is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + n, 0);
        r.read_exact(&mut body[start..]).await?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).await?;
    }
}
