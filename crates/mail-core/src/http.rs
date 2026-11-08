//! Minimal HTTP/1.1 client over the same rustls transport as the mail
//! protocols. Enough for CalDAV, Microsoft Graph and OAuth token endpoints,
//! without pulling in an HTTP stack.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{Error, Result};
use crate::net::{self, Conn, MailStream};

const MAX_BODY: usize = 32 << 20;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with '/'.
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url> {
        let bad = || Error::Other(format!("invalid URL: {url}"));
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(bad());
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(bad()); // credentials in URLs are never used
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || p.chars().all(|c| c.is_ascii_digit()) => (h, p.parse().map_err(|_| bad())?),
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad());
        }
        // Plain HTTP only for the local OAuth redirect and tests.
        if !https && host != "127.0.0.1" && host != "localhost" {
            return Err(Error::InsecureRefused);
        }
        Ok(Url { https, host: host.to_string(), port, path: path.to_string() })
    }

    /// Resolves a Location header or an href against this URL.
    pub fn join(&self, reference: &str) -> Result<Url> {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Url::parse(reference);
        }
        let mut u = self.clone();
        u.path = if reference.starts_with('/') {
            reference.to_string()
        } else {
            let base = self.path.split('?').next().unwrap_or("/");
            format!("{}{}", &base[..base.rfind('/').map_or(0, |i| i + 1)], reference)
        };
        Ok(u)
    }

    pub fn origin(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        let scheme = if self.https { "https" } else { "http" };
        if self.port == default {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    pub fn to_string_full(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Largest response body accepted.
    pub max_body: usize,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Request {
        Request { method: method.into(), url: url.into(), headers: Vec::new(), body: Vec::new(), max_body: MAX_BODY }
    }

    /// Raises (or lowers) the response size limit, e.g. for app updates.
    pub fn max_body(mut self, bytes: usize) -> Request {
        self.max_body = bytes;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Request {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    pub fn basic(self, user: &str, pass: &str) -> Request {
        let t = crate::b64::encode(format!("{user}:{pass}"));
        self.header("Authorization", &format!("Basic {t}"))
    }

    pub fn body(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Request {
        self.body = body.into();
        self.header("Content-Type", content_type)
    }

    pub fn form(self, fields: &[(&str, &str)]) -> Request {
        let body = fields.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&");
        self.body("application/x-www-form-urlencoded", body)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Final URL after redirects.
    pub url: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Err for non-2xx, with the start of the body for context.
    pub fn ok(self) -> Result<Response> {
        if self.is_success() {
            return Ok(self);
        }
        let snippet: String = self.text().chars().take(300).collect();
        if self.status == 401 || self.status == 403 {
            return Err(Error::Auth(format!("HTTP {}: {snippet}", self.status)));
        }
        Err(Error::Server(format!("HTTP {}: {snippet}", self.status)))
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// application/x-www-form-urlencoded / query component encoding.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sends a request, following redirects (same method; CalDAV needs that).
pub async fn send(req: Request) -> Result<Response> {
    let mut url = Url::parse(&req.url)?;
    for _ in 0..=MAX_REDIRECTS {
        let tcp = net::connect_tcp(&url.host, url.port).await?;
        let stream = if url.https {
            MailStream::Tls(Box::new(net::tls_wrap(tcp, &url.host).await?))
        } else {
            MailStream::Plain(tcp)
        };
        let mut resp = send_on(stream, &url, &req).await?;
        resp.url = url.to_string_full();
        if matches!(resp.status, 301 | 302 | 307 | 308) {
            if let Some(loc) = resp.header("Location") {
                let next = url.join(loc)?;
                // Never follow a redirect from HTTPS down to plain HTTP.
                if url.https && !next.https {
                    return Err(Error::InsecureRefused);
                }
                // Credentials only go to the origin they were meant for.
                if next.origin() != url.origin() && req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Authorization")) {
                    return Ok(resp);
                }
                url = next;
                continue;
            }
        }
        return Ok(resp);
    }
    Err(Error::Other("too many redirects".into()))
}

/// One request/response exchange on an already-connected stream.
pub async fn send_on<S: AsyncRead + AsyncWrite + Unpin>(stream: S, url: &Url, req: &Request) -> Result<Response> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Carteiro/{}\r\nAccept-Encoding: identity\r\nConnection: close\r\nContent-Length: {}\r\n",
        req.method,
        url.path,
        if url.port == 443 || url.port == 80 { url.host.clone() } else { format!("{}:{}", url.host, url.port) },
        env!("CARGO_PKG_VERSION"),
        req.body.len()
    );
    for (k, v) in &req.headers {
        net::check_line(k)?;
        net::check_line(v)?;
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut conn = Conn::new(stream);
    let mut out = head.into_bytes();
    out.extend_from_slice(&req.body);
    conn.write_all(&out).await?;

    let status_line = String::from_utf8_lossy(&conn.read_line().await?).into_owned();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Protocol(format!("bad HTTP status line: {status_line}")))?;
    let mut headers = Vec::new();
    loop {
        let line = conn.read_line().await?;
        if line.is_empty() {
            break;
        }
        let line = String::from_utf8_lossy(&line).into_owned();
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
    let body = if req.method == "HEAD" || status == 204 || status == 304 {
        Vec::new()
    } else if get("Transfer-Encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        let mut body = Vec::new();
        loop {
            let size_line = String::from_utf8_lossy(&conn.read_line().await?).into_owned();
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| Error::Protocol("bad chunk size".into()))?;
            if size == 0 {
                // Trailers until an empty line.
                while !conn.read_line().await?.is_empty() {}
                break;
            }
            if body.len() + size > req.max_body {
                return Err(Error::Protocol("HTTP response too large".into()));
            }
            body.extend(conn.read_exact_n(size).await?);
            conn.read_line().await?; // CRLF after each chunk
        }
        body
    } else if let Some(len) = get("Content-Length").and_then(|v| v.parse::<usize>().ok()) {
        if len > req.max_body {
            return Err(Error::Protocol("HTTP response too large".into()));
        }
        conn.read_exact_n(len).await?
    } else {
        conn.read_to_end(req.max_body).await?
    };
    Ok(Response { status, headers, body, url: url.to_string_full() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn parses_urls() {
        let u = Url::parse("https://caldav.example.com/dav/cal/?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("caldav.example.com", 443, "/dav/cal/?x=1"));
        assert_eq!(Url::parse("https://h:8443").unwrap().port, 8443);
        assert!(matches!(Url::parse("http://evil.example/"), Err(Error::InsecureRefused)));
        assert!(Url::parse("http://127.0.0.1:5000/cb").is_ok());
        assert!(Url::parse("https://user:pw@h/").is_err());
        assert_eq!(u.join("/other/").unwrap().path, "/other/");
        assert_eq!(u.join("event.ics").unwrap().path, "/dav/cal/event.ics");
        assert_eq!(u.join("https://x.org/a").unwrap().host, "x.org");
    }

    #[test]
    fn form_encoding() {
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(decode("a%20b+c%26"), "a b c&");
    }

    async fn roundtrip(reply: &'static [u8]) -> Response {
        let (client, mut server) = tokio::io::duplex(1 << 16);
        let srv = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = server.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            server.write_all(reply).await.unwrap();
            req
        });
        let url = Url::parse("https://example.com/x").unwrap();
        let resp = send_on(client, &url, &Request::new("PROPFIND", "https://example.com/x").header("Depth", "0").body("text/xml", "<a/>")).await.unwrap();
        let req = srv.await.unwrap();
        assert!(req.starts_with("PROPFIND /x HTTP/1.1\r\nHost: example.com\r\n"));
        assert!(req.contains("Depth: 0\r\n") && req.ends_with("\r\n\r\n<a/>"));
        resp
    }

    #[tokio::test]
    async fn content_length_body() {
        let r = roundtrip(b"HTTP/1.1 207 Multi-Status\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello").await;
        assert_eq!((r.status, r.text().as_str(), r.header("x-a")), (207, "hello", Some("b")));
    }

    #[tokio::test]
    async fn chunked_body() {
        let r = roundtrip(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n").await;
        assert_eq!(r.text(), "hello world");
    }

    #[tokio::test]
    async fn rejects_header_injection() {
        let (client, _server) = tokio::io::duplex(1024);
        let url = Url::parse("https://example.com/").unwrap();
        let req = Request::new("GET", "https://example.com/").header("X", "a\r\nEvil: 1");
        assert!(send_on(client, &url, &req).await.is_err());
    }
}
