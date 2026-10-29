//! HTTPS for the web build, through the relay. The Microsoft, Xbox and
//! Minecraft services a sign-in and an online-mode join call let no other
//! site's page call them, so the requests go through the relay's TCP tunnel
//! (`?target=host:443`, which horizon-web-relay opens to those services)
//! with TLS spoken here, end to end with the service: the relay carries
//! ciphertext, so a relay run by someone else never sees a token. One
//! HTTP/1.1 request per connection. The sockets are the page's, so this
//! runs on the page's thread.
use super::socket::Socket;
use crate::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore,
    pki_types::{ServerName, UnixTime},
};
use std::{
    io::{self, Read, Write},
    sync::{Arc, OnceLock},
};

/// A response: its status and its body (chunked transfer decoded).
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}
impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// rustls checks certificates against the time, which std can't tell in a
/// browser: the page's clock.
#[derive(Debug)]
struct PageClock;
impl rustls::time_provider::TimeProvider for PageClock {
    fn current_time(&self) -> Option<UnixTime> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(UnixTime::since_unix_epoch)
    }
}

/// RustCrypto's algorithms and Mozilla's roots (webpki-roots).
fn config() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(config.clone());
    }
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_details(
        Arc::new(rustls_rustcrypto::provider()),
        Arc::new(PageClock),
    )
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(CONFIG.get_or_init(|| Arc::new(config)).clone())
}

/// `https://host[:port]/path?query` as (host, port, path and query).
fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("not an https address: {url}"))?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_owned()
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse()
                .map_err(|_| format!("not an https address: {url}"))?,
        ),
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(format!("not an https address: {url}"));
    }
    Ok((host.to_ascii_lowercase(), port, path))
}

/// `method url` with `headers` and `body` (sent with its length), through
/// the relay at `relay` (`ws://` or `wss://`), within `timeout`. Errors say
/// what failed without the request, whose query or body may hold secrets.
pub async fn request(
    relay: &str,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let (host, port, path) = split_url(url)?;
    let deadline = Instant::now() + timeout;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}");
    if port != 443 {
        head.push_str(&format!(":{port}"));
    }
    head.push_str("\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
    {
        head.push_str("User-Agent: Horizon\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || method != "GET" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);

    let relay = relay.trim();
    let join = if relay.contains('?') { '&' } else { '?' };
    let socket = Socket::connect(&format!("{relay}{join}target={host}:{port}"), timeout)
        .await
        .map_err(|e| format!("The relay can't reach {host}: {e}"))?;
    let result = exchange(&socket, &host, &bytes, deadline).await;
    socket.close();
    result
}

/// The TLS session over `socket`: send `request`, read the response.
async fn exchange(
    socket: &Socket,
    host: &str,
    request: &[u8],
    deadline: Instant,
) -> Result<Response, String> {
    let tls_error = |e: rustls::Error| format!("TLS with {host}: {e}");
    let name = ServerName::try_from(host.to_owned()).map_err(|_| format!("Bad host {host}"))?;
    let mut tls = ClientConnection::new(config()?, name).map_err(tls_error)?;
    // Kept until the handshake is done, then sent.
    tls.writer()
        .write_all(request)
        .map_err(|e| format!("TLS with {host}: {e}"))?;
    let mut plain = Vec::new();
    let mut buf = vec![0u8; 16 << 10];
    loop {
        while tls.wants_write() {
            let mut out = Vec::new();
            tls.write_tls(&mut out)
                .map_err(|e| format!("TLS with {host}: {e}"))?;
            socket
                .write(&out)
                .map_err(|e| format!("Lost {host}: {e}"))?;
        }
        let mut ended = false;
        loop {
            match tls.reader().read(&mut buf) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => plain.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // Closed without close_notify: what came is all there is.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    ended = true;
                    break;
                }
                Err(e) => return Err(format!("TLS with {host}: {e}")),
            }
        }
        if let Some(response) = parse(&plain, ended)? {
            return Ok(response);
        }
        if ended {
            return Err(format!("{host} closed the connection before it answered"));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !socket.readable_within(left).await {
            return Err(format!("{host} did not answer in time"));
        }
        match socket.read(&mut buf) {
            Ok(0) => {
                tls.read_tls(&mut io::empty())
                    .map_err(|e| format!("TLS with {host}: {e}"))?;
                tls.process_new_packets().map_err(tls_error)?;
            }
            Ok(n) => {
                let mut arrived = &buf[..n];
                while !arrived.is_empty() {
                    tls.read_tls(&mut arrived)
                        .map_err(|e| format!("TLS with {host}: {e}"))?;
                    tls.process_new_packets().map_err(tls_error)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("Lost {host}: {e}")),
        }
    }
}

fn find(data: &[u8], what: &[u8]) -> Option<usize> {
    data.windows(what.len()).position(|w| w == what)
}

/// The response in `data`, once it is all there (`ended`: nothing more
/// comes); None while more is needed.
fn parse(data: &[u8], ended: bool) -> Result<Option<Response>, String> {
    let Some(end) = find(data, b"\r\n\r\n") else {
        return if ended {
            Err("The answer was cut off".into())
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&data[..end]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("Not an HTTP answer")?;
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    let rest = &data[end + 4..];
    let body = if chunked {
        dechunk(rest)
    } else if let Some(length) = length {
        (rest.len() >= length).then(|| rest[..length].to_vec())
    } else if ended || status == 204 || status == 304 {
        Some(rest.to_vec())
    } else {
        None
    };
    match body {
        Some(body) => Ok(Some(Response { status, body })),
        None if ended => Err("The answer was cut off".into()),
        None => Ok(None),
    }
}

/// A chunked body, once its last chunk is in.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = find(data, b"\r\n")?;
        let size = std::str::from_utf8(&data[..line]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        data = &data[line + 2..];
        if size == 0 {
            return Some(out);
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_answers() {
        assert_eq!(
            split_url("https://Login.Example.com/a/b?c=d").unwrap(),
            ("login.example.com".into(), 443, "/a/b?c=d".into())
        );
        assert_eq!(
            split_url("https://h:8443?x").unwrap(),
            ("h".into(), 8443, "/?x".into())
        );
        assert!(split_url("http://h/").is_err());
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(parse(plain, false).unwrap().unwrap().body, b"hi");
        assert!(parse(&plain[..plain.len() - 1], false).unwrap().is_none());
        let chunked = b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n3;x\r\n!!!\r\n0\r\n\r\n";
        let r = parse(chunked, false).unwrap().unwrap();
        assert_eq!((r.status, r.body.as_slice()), (201, &b"hi!!!"[..]));
        let open = b"HTTP/1.1 200 OK\r\n\r\nall of it";
        assert!(parse(open, false).unwrap().is_none());
        assert_eq!(parse(open, true).unwrap().unwrap().body, b"all of it");
    }
}
