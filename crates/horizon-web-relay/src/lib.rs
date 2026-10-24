//! The web build's way to a Minecraft server (web/README.md): a browser can
//! open WebSockets but no TCP connections, so this takes a WebSocket and
//! opens the TCP connection for it, then copies bytes both ways. The
//! Minecraft protocol stays end to end, between the game in the browser and
//! the server: the relay doesn't read it.
//!
//! The server is the one `--target` names, as websockify does, or the one
//! the page names (`?target=host:port`) when `--allow` lets it. Nothing else
//! is reached: an open relay would let any page reach any port this machine
//! can.
//!
//! A server's name may redirect by its SRV record, as the game follows it
//! (`horizon_protocol::resolver`): the relay connects where it says.
//!
//! It also introduces browsers to each other ([`rooms`]): a world played in
//! one page is joined from another over WebRTC, the relay carrying only the
//! few messages that set the connection up. And it passes a server's
//! resource pack on to a page whose own fetch the pack's server refuses
//! (`?download=URL`, [`Config::downloads`]).
pub mod rooms;
use base64::Engine;
use sha1::{Digest, Sha1};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

/// A server the page may name: a host, or any public one (`*`), on a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allow {
    /// None: any host that resolves to public addresses only.
    pub host: Option<String>,
    pub port: u16,
}
impl Allow {
    /// `host:port`, `[v6]:port` or `*:port`.
    pub fn parse(text: &str) -> Option<Self> {
        let (host, port) = split_host_port(text)?;
        Some(Self {
            host: (host != "*").then(|| normal_host(&host)),
            port,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The one server every connection reaches (the page's `target` is ignored).
    pub target: Option<(String, u16)>,
    /// The servers a page may name with `?target=`.
    pub allow: Vec<Allow>,
    /// The pages that may connect (their `Origin`); empty: any.
    pub origins: Vec<String>,
    /// Connections open at once.
    pub max_connections: usize,
    /// How long the server has to accept the TCP connection.
    pub connect_timeout: Duration,
    /// Introduce browsers to each other (`?host`, `?join=CODE`).
    pub rooms: bool,
    pub max_rooms: usize,
    pub max_guests: usize,
    /// Pass server resource packs on to pages (`?download=URL`): from public
    /// addresses only, at most [`MAX_DOWNLOAD`] bytes.
    pub downloads: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            target: None,
            allow: Vec::new(),
            origins: Vec::new(),
            max_connections: 64,
            connect_timeout: Duration::from_secs(10),
            rooms: true,
            max_rooms: 256,
            max_guests: 16,
            downloads: false,
        }
    }
}

/// The largest download passed on: vanilla's for a server resource pack
/// (`DownloadedPackSource.MAX_PACK_SIZE_BYTES`).
pub const MAX_DOWNLOAD: u64 = 262_144_000;

/// The GUID RFC 6455 appends to the client's key.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The largest frame taken from a page: a Minecraft packet is at most 2 MiB.
const MAX_FRAME: u64 = 4 << 20;

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.trim().as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

/// Serve WebSockets on `listener` until the process ends.
pub fn serve(listener: TcpListener, config: Config) {
    let rooms = Arc::new(rooms::Rooms::new(config.max_rooms, config.max_guests));
    let config = Arc::new(config);
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if open.fetch_add(1, Ordering::Relaxed) >= config.max_connections {
            open.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let (config, count, rooms) = (config.clone(), open.clone(), rooms.clone());
        let spawned = std::thread::Builder::new()
            .name("web-relay".into())
            .spawn(move || {
                let peer = stream.peer_addr().ok();
                if let Err(e) = relay(stream, &config, &rooms)
                    && let Some(peer) = peer
                {
                    println!("{peer}: {e}");
                }
                count.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// What the page asked for in its upgrade request.
#[derive(Debug, Default)]
struct Request {
    path: String,
    upgrade: bool,
    key: Option<String>,
    version: Option<String>,
    origin: Option<String>,
    protocols: Vec<String>,
}

fn read_request(reader: &mut impl BufRead) -> io::Result<Request> {
    let mut request = Request::default();
    let mut line = Vec::new();
    let mut total = 0;
    for index in 0.. {
        line.clear();
        let n = reader.take(8192).read_until(b'\n', &mut line)?;
        total += n;
        if n == 0 || total > 16384 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request"));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if index == 0 {
            let mut parts = text.split(' ');
            if parts.next() != Some("GET") {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a GET"));
            }
            request.path = parts.next().unwrap_or("/").to_owned();
            continue;
        }
        if text.is_empty() {
            break;
        }
        let Some((name, value)) = text.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => request.upgrade = value.eq_ignore_ascii_case("websocket"),
            "sec-websocket-key" => request.key = Some(value.to_owned()),
            "sec-websocket-version" => request.version = Some(value.to_owned()),
            "origin" => request.origin = Some(value.to_owned()),
            "sec-websocket-protocol" => request
                .protocols
                .extend(value.split(',').map(|p| p.trim().to_owned())),
            _ => {}
        }
    }
    Ok(request)
}

/// The value of `name` in a request's query, percent-decoded: `""` for a
/// name without one.
fn query(path: &str, name: &str) -> Option<String> {
    query_all(path, name).into_iter().next()
}

/// Every value of `name` in a request's query, in order.
fn query_all(path: &str, name: &str) -> Vec<String> {
    let Some((_, query)) = path.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| match pair.split_once('=') {
            Some((n, value)) => (n == name).then_some(value),
            None => (pair == name).then_some(""),
        })
        .filter_map(decode)
        .collect()
}

/// A query value, percent-decoded.
fn decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `host:port`, `[v6]:port` or a bare host (port 25565).
fn split_host_port(text: &str) -> Option<(String, u16)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => 25565,
            None => return None,
        };
        return (!host.is_empty()).then(|| (host.to_owned(), port));
    }
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, port.parse().ok()?),
        Some(_) => return None,
        None => (text, 25565),
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

fn normal_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Addresses no page should reach through `*`: this machine and private networks.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Where a connection for `asked` (the page's target) goes, or why it can't.
pub fn resolve(config: &Config, asked: Option<&str>) -> Result<(String, Vec<SocketAddr>), String> {
    resolve_with(config, asked, horizon_protocol::resolver::srv_target)
}

/// [`resolve`], with `srv` for the SRV redirect a name has.
fn resolve_with(
    config: &Config,
    asked: Option<&str>,
    srv: impl FnOnce(&str, u16) -> Option<(String, u16)>,
) -> Result<(String, Vec<SocketAddr>), String> {
    let (host, port) = match (&config.target, asked) {
        (Some(target), _) => target.clone(),
        (None, None) => return Err("Name a server: ?target=host:port".to_owned()),
        (None, Some(asked)) => {
            split_host_port(asked).ok_or_else(|| format!("Not a server address: {asked}"))?
        }
    };
    let shown = format!("{host}:{port}");
    let named = normal_host(&host);
    let fixed = config.target.is_some();
    let exact = config
        .allow
        .iter()
        .any(|a| a.port == port && a.host.as_deref() == Some(named.as_str()));
    let any_public = config
        .allow
        .iter()
        .any(|a| a.port == port && a.host.is_none());
    if !fixed && !exact && !any_public {
        return Err(format!("This relay doesn't reach {shown}"));
    }
    // A name the relay was given (its target, or one it allows by name) goes
    // where its SRV record says, as in the game. Anyone can write a record,
    // so through `*` it leads only to a public address on an unprivileged port.
    let trusted = fixed || exact;
    let (host, port, shown) = match srv(&named, port) {
        Some((target, target_port)) => {
            let target = target.trim_end_matches('.').to_owned();
            let shown = format!("{shown} ({target}:{target_port})");
            if !trusted && target_port < 1024 {
                return Err(format!("This relay doesn't reach {shown}"));
            }
            (target, target_port, shown)
        }
        None => (host, port, shown),
    };
    let addrs: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| format!("Unknown host {host}"))?
        .filter(|a| trusted || is_public(a.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(format!("This relay doesn't reach {shown}"));
    }
    Ok((shown, addrs))
}

/// One frame to the page (servers don't mask).
fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// A close frame: the code and a reason of at most 123 bytes.
fn close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(&reason.as_bytes()[..end]);
    frame(0x8, &payload)
}

/// The next frame from the page: its opcode and unmasked payload.
fn read_frame(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head)?;
    let opcode = head[0] & 0x0f;
    if head[1] & 0x80 == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unmasked frame"));
    }
    let len = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b)?;
            u64::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => u64::from(n),
    };
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask)?;
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Ok((opcode, payload))
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    respond_with(stream, status, "", body)
}

/// [`respond`] with more header lines (each ending in CRLF).
fn respond_with(out: &mut impl Write, status: &str, headers: &str, body: &str) -> io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A page's `?download=URL`: a server resource pack, passed on with the
/// header that lets the page read it (`Access-Control-Allow-Origin`) for a
/// pack whose own server doesn't. `header=Name:Value` sets the game's
/// download headers (`X-Minecraft-*`, `User-Agent`).
fn download(client: &mut TcpStream, request: &Request, url: &str) -> io::Result<()> {
    let headers: Vec<(String, String)> = query_all(&request.path, "header")
        .into_iter()
        .filter_map(|header| {
            let (name, value) = header.split_once(':')?;
            let name = name.trim();
            let lower = name.to_ascii_lowercase();
            (lower.starts_with("x-minecraft-") || lower == "user-agent")
                .then(|| (name.to_owned(), value.trim().to_owned()))
        })
        .collect();
    let peer = client.peer_addr()?;
    println!("{peer}: downloading {url}");
    let origin = request.origin.as_deref().unwrap_or("*");
    pass_on(client, url, &headers, origin, true)
}

/// GET `url` and write the response to `out`: from http and https, public
/// addresses only (after redirects too) unless `public_only` is off, and
/// at most [`MAX_DOWNLOAD`] bytes (a longer one is cut, which the page sees
/// as a failed download).
fn pass_on(
    out: &mut impl Write,
    url: &str,
    headers: &[(String, String)],
    origin: &str,
    public_only: bool,
) -> io::Result<()> {
    let cors = format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n");
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return respond_with(
            out,
            "400 Bad Request",
            &cors,
            "Only http and https downloads\n",
        );
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .resolver(move |netloc: &str| -> io::Result<Vec<SocketAddr>> {
            let addrs: Vec<SocketAddr> = netloc
                .to_socket_addrs()?
                .filter(|a| !public_only || is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(io::Error::other(format!(
                    "this relay doesn't download from {netloc}"
                )));
            }
            Ok(addrs)
        })
        .build();
    let mut call = agent.get(url);
    for (name, value) in headers {
        call = call.set(name, value);
    }
    let response = match call.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            let body = format!("The pack's server answered {code}\n");
            return respond_with(out, "502 Bad Gateway", &cors, &body);
        }
        Err(e) => {
            let body = format!("Can't download the pack: {e}\n");
            return respond_with(out, "502 Bad Gateway", &cors, &body);
        }
    };
    let size = response
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok());
    if size.is_some_and(|size| size > MAX_DOWNLOAD) {
        return respond_with(
            out,
            "413 Content Too Large",
            &cors,
            "The pack is over 250 MiB\n",
        );
    }
    let length = match size {
        Some(size) => format!("Content-Length: {size}\r\n"),
        None => "Transfer-Encoding: chunked\r\n".to_owned(),
    };
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n{cors}{length}Connection: close\r\n\r\n"
    )?;
    let mut input = response
        .into_reader()
        .take(size.unwrap_or(MAX_DOWNLOAD + 1));
    let mut chunk = vec![0u8; 65536];
    let mut sent = 0;
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        sent += n as u64;
        if sent > MAX_DOWNLOAD {
            return Err(io::Error::other("the pack is over 250 MiB"));
        }
        if size.is_some() {
            out.write_all(&chunk[..n])?;
        } else {
            write!(out, "{n:x}\r\n")?;
            out.write_all(&chunk[..n])?;
            out.write_all(b"\r\n")?;
        }
    }
    if size.is_none() {
        out.write_all(b"0\r\n\r\n")?;
    }
    out.flush()
}

/// One page's connection, from its upgrade request to the end.
fn relay(mut client: TcpStream, config: &Config, rooms: &rooms::Rooms) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let request = read_request(&mut reader)?;
    let allowed_origin = config.origins.is_empty()
        || request
            .origin
            .as_ref()
            .is_some_and(|o| config.origins.iter().any(|a| a.eq_ignore_ascii_case(o)));
    if !request.upgrade
        && config.downloads
        && let Some(url) = query(&request.path, "download")
    {
        if !allowed_origin {
            return respond(
                &mut client,
                "403 Forbidden",
                "This page may not use the relay\n",
            );
        }
        client.set_read_timeout(None)?;
        return download(&mut client, &request, &url);
    }
    let (Some(key), true) = (&request.key, request.upgrade) else {
        return respond(
            &mut client,
            "426 Upgrade Required",
            "horizon-web-relay: connect with a WebSocket\n",
        );
    };
    if request.version.as_deref() != Some("13") {
        return respond(
            &mut client,
            "426 Upgrade Required",
            "WebSocket version 13\n",
        );
    }
    if !allowed_origin {
        return respond(
            &mut client,
            "403 Forbidden",
            "This page may not use the relay\n",
        );
    }
    // websockify's clients ask for the `binary` subprotocol.
    let protocol = if request.protocols.iter().any(|p| p == "binary") {
        "Sec-WebSocket-Protocol: binary\r\n"
    } else {
        ""
    };
    write!(
        client,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n{protocol}\r\n",
        accept_key(key)
    )?;
    client.set_read_timeout(None)?;
    if config.rooms {
        if query(&request.path, "host").is_some() {
            return rooms.host(reader, client);
        }
        if let Some(code) = query(&request.path, "join") {
            return rooms.join(&code, reader, client);
        }
    }
    let asked = query(&request.path, "target");
    // The page learns why through the close frame's reason: a browser shows
    // nothing of a refused upgrade.
    let (shown, addrs) = match resolve(config, asked.as_deref()) {
        Ok(found) => found,
        Err(reason) => {
            client.write_all(&close_frame(1008, &reason))?;
            return Err(io::Error::other(reason));
        }
    };
    let mut server = None;
    let mut failure = String::new();
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, config.connect_timeout) {
            Ok(s) => {
                server = Some(s);
                break;
            }
            Err(e) => failure = format!("Can't reach {shown}: {e}"),
        }
    }
    let Some(server) = server else {
        client.write_all(&close_frame(1011, &failure))?;
        return Err(io::Error::other(failure));
    };
    server.set_nodelay(true)?;
    client.set_nodelay(true)?;
    let peer = client.peer_addr()?;
    println!("{peer}: relaying to {shown}");
    pump(reader, client, server);
    println!("{peer}: closed");
    Ok(())
}

/// Copies both ways until either end closes.
fn pump(mut from_page: BufReader<TcpStream>, client: TcpStream, server: TcpStream) {
    let to_page = Arc::new(Mutex::new(client));
    let closed = Arc::new(AtomicBool::new(false));
    let send_close = {
        let (to_page, closed) = (to_page.clone(), closed.clone());
        move |code: u16, reason: &str| {
            if !closed.swap(true, Ordering::Relaxed) {
                let page = to_page.lock().unwrap();
                let _ = (&*page).write_all(&close_frame(code, reason));
                let _ = page.shutdown(Shutdown::Write);
            }
        }
    };
    let Ok(mut to_server) = server.try_clone() else {
        return;
    };
    let page_side = {
        let (to_page, send_close) = (to_page.clone(), send_close.clone());
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut from_page) {
                    // Binary, text and their continuations: the bytes go on.
                    Ok((0x0..=0x2, payload)) => {
                        if to_server.write_all(&payload).is_err() {
                            send_close(1011, "The server closed the connection");
                            break;
                        }
                    }
                    Ok((0x8, _)) => {
                        send_close(1000, "");
                        break;
                    }
                    Ok((0x9, payload)) => {
                        let page = to_page.lock().unwrap();
                        let _ = (&*page).write_all(&frame(0xA, &payload));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = to_server.shutdown(Shutdown::Both);
        })
    };
    let mut server = server;
    let mut chunk = vec![0u8; 65536];
    loop {
        match server.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let page = to_page.lock().unwrap();
                if (&*page).write_all(&frame(0x2, &chunk[..n])).is_err() {
                    break;
                }
            }
        }
    }
    send_close(1000, "");
    // The page has this long to answer the close.
    let _ = to_page
        .lock()
        .unwrap()
        .set_read_timeout(Some(Duration::from_secs(10)));
    let _ = page_side.join();
    let _ = to_page.lock().unwrap().shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn accept_key_matches_rfc_6455() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn targets_parse_and_decode() {
        assert_eq!(
            query("/?target=play.example.net%3A25566", "target"),
            Some("play.example.net:25566".to_owned())
        );
        assert_eq!(
            query("/relay?x=1&target=%5B::1%5D:25565", "target"),
            Some("[::1]:25565".to_owned())
        );
        assert_eq!(query("/", "target"), None);
        assert_eq!(query("/?host", "host"), Some(String::new()));
        assert_eq!(query("/?hosts=1", "host"), None);
        assert_eq!(
            split_host_port("[::1]:25570"),
            Some(("::1".to_owned(), 25570))
        );
        assert_eq!(
            split_host_port("Example.org"),
            Some(("Example.org".to_owned(), 25565))
        );
        assert_eq!(split_host_port("a:b:c"), None);
        assert_eq!(
            Allow::parse("*:25565"),
            Some(Allow {
                host: None,
                port: 25565
            })
        );
    }

    #[test]
    fn only_allowed_targets_are_reached() {
        let config = Config {
            allow: vec![Allow::parse("localhost:25565").unwrap()],
            ..Config::default()
        };
        assert!(resolve(&config, Some("LOCALHOST:25565")).is_ok());
        assert!(resolve(&config, Some("localhost:22")).is_err());
        assert!(resolve(&config, Some("127.0.0.1:25565")).is_err());
        assert!(resolve(&config, None).is_err());
        // `*` never reaches this machine or a private network.
        let any = Config {
            allow: vec![Allow::parse("*:25565").unwrap()],
            ..Config::default()
        };
        assert!(resolve(&any, Some("127.0.0.1:25565")).is_err());
        assert!(resolve(&any, Some("192.168.1.4:25565")).is_err());
        assert!(resolve(&any, Some("[::ffff:10.0.0.1]:25565")).is_err());
        assert!(resolve(&any, Some("1.1.1.1:25565")).is_ok());
        assert!(resolve(&any, Some("1.1.1.1:22")).is_err());
        // A fixed target ignores what the page names.
        let fixed = Config {
            target: Some(("127.0.0.1".to_owned(), 25565)),
            ..Config::default()
        };
        assert_eq!(
            resolve(&fixed, Some("1.1.1.1:22")).unwrap().0,
            "127.0.0.1:25565"
        );
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
    }

    #[test]
    fn srv_records_redirect_within_what_the_relay_reaches() {
        let any = Config {
            allow: vec![Allow::parse("*:25565").unwrap()],
            ..Config::default()
        };
        let to = |host: &'static str, port| move |_: &str, _| Some((host.to_owned(), port));
        // A public target on a Minecraft port is reached through `*`.
        let (shown, addrs) =
            resolve_with(&any, Some("play.example.net"), to("1.1.1.1.", 25600)).unwrap();
        assert_eq!(shown, "play.example.net:25565 (1.1.1.1:25600)");
        assert_eq!(addrs, vec!["1.1.1.1:25600".parse().unwrap()]);
        // Not a privileged port, nor a private address.
        assert!(resolve_with(&any, Some("play.example.net"), to("1.1.1.1", 25)).is_err());
        assert!(resolve_with(&any, Some("play.example.net"), to("10.0.0.2", 25600)).is_err());
        // A name allowed by name goes wherever its record says.
        let named = Config {
            allow: vec![Allow::parse("play.example.net:25565").unwrap()],
            ..Config::default()
        };
        let (_, addrs) = resolve_with(&named, Some("Play.Example.net."), |host, port| {
            assert_eq!((host, port), ("play.example.net", 25565));
            Some(("127.0.0.1".to_owned(), 22))
        })
        .unwrap();
        assert_eq!(addrs, vec!["127.0.0.1:22".parse().unwrap()]);
    }

    #[test]
    fn repeated_query_values_are_all_read() {
        assert_eq!(
            query_all("/?download=x&header=A%3A1&header=B:2", "header"),
            vec!["A:1".to_owned(), "B:2".to_owned()]
        );
        assert!(query_all("/", "header").is_empty());
    }

    /// A one-shot HTTP server on this machine answering with `body`, with or
    /// without a Content-Length; the request it got.
    fn http_once(body: &'static [u8], length: bool) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pack.zip", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            let length = if length {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n"
            )
            .unwrap();
            stream.write_all(body).unwrap();
            request
        });
        (url, handle)
    }

    #[test]
    fn downloads_pass_on_with_the_pages_origin_and_the_games_headers() {
        for length in [true, false] {
            let (url, server) = http_once(b"PK pack bytes", length);
            let mut out = Vec::new();
            let headers = [("X-Minecraft-Username".to_owned(), "Alex".to_owned())];
            pass_on(&mut out, &url, &headers, "https://play.example", false).unwrap();
            let out = String::from_utf8(out).unwrap();
            let (head, body) = out.split_once("\r\n\r\n").unwrap();
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert!(head.contains("Access-Control-Allow-Origin: https://play.example"));
            let body = if length {
                body.to_owned()
            } else {
                assert!(head.contains("Transfer-Encoding: chunked"));
                assert_eq!(body, "d\r\nPK pack bytes\r\n0\r\n\r\n");
                "PK pack bytes".to_owned()
            };
            assert_eq!(body, "PK pack bytes");
            assert!(
                server
                    .join()
                    .unwrap()
                    .contains("X-Minecraft-Username: Alex")
            );
        }
        // This machine is not a public address: a page can't reach it so.
        let (url, _server) = http_once(b"", true);
        let mut out = Vec::new();
        pass_on(&mut out, &url, &[], "*", true).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("HTTP/1.1 502"), "{out}");
        assert!(out.contains("doesn't download from"), "{out}");
        let mut out = Vec::new();
        pass_on(&mut out, "file:///etc/passwd", &[], "*", true).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn frames_round_trip() {
        for len in [0, 125, 126, 70000] {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut framed = frame(0x2, &payload);
            // As a page sends it: masked.
            let mask = [1, 2, 3, 4];
            framed[1] |= 0x80;
            let at = match len {
                n if n < 126 => 2,
                n if n <= 0xffff => 4,
                _ => 10,
            };
            let mut masked: Vec<u8> = framed[..at].to_vec();
            masked.extend_from_slice(&mask);
            masked.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            let (opcode, read) = read_frame(&mut masked.as_slice()).unwrap();
            assert_eq!((opcode, read), (0x2, payload));
        }
        let close = close_frame(1008, &"é".repeat(100));
        assert_eq!(close[1] as usize, close.len() - 2);
        assert!(close.len() - 2 <= 125);
    }

    /// A page's end of a WebSocket to the relay on `port`.
    struct Page {
        ws: TcpStream,
        reader: BufReader<TcpStream>,
    }
    impl Page {
        fn open(port: u16, query: &str) -> Self {
            let mut ws = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                ws,
                "GET /?{query} HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            )
            .unwrap();
            let mut reader = BufReader::new(ws.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("HTTP/1.1 101"), "{line}");
            while line != "\r\n" {
                line.clear();
                reader.read_line(&mut line).unwrap();
            }
            Self { ws, reader }
        }
        /// Send a message, masked as pages do.
        fn send(&mut self, data: &[u8]) {
            let mask = [9, 8, 7, 6];
            let mut message = vec![0x82, 0x80 | data.len() as u8];
            message.extend_from_slice(&mask);
            message.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.ws.write_all(&message).unwrap();
        }
        /// The next frame: opcode and payload (short ones only).
        fn next(&mut self) -> (u8, Vec<u8>) {
            let mut head = [0u8; 2];
            self.reader.read_exact(&mut head).unwrap();
            let mut payload = vec![0u8; usize::from(head[1] & 0x7f)];
            self.reader.read_exact(&mut payload).unwrap();
            (head[0] & 0x0f, payload)
        }
        fn text(&mut self) -> String {
            let (opcode, payload) = self.next();
            assert_eq!(opcode, 0x1);
            String::from_utf8(payload).unwrap()
        }
        /// The close frame's code and reason.
        fn closed(&mut self) -> (u16, String) {
            let (opcode, payload) = self.next();
            assert_eq!(opcode, 0x8);
            (
                u16::from_be_bytes([payload[0], payload[1]]),
                String::from_utf8(payload[2..].to_vec()).unwrap(),
            )
        }
    }

    fn relay_on(config: Config) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || serve(listener, config));
        port
    }

    /// A page's bytes reach the server and the server's come back, and
    /// what the relay refuses is said in the close frame.
    #[test]
    fn relays_bytes_and_explains_refusals() {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = echo.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap();
            s.write_all(&buf[..n]).unwrap();
        });
        let port = relay_on(Config {
            allow: vec![Allow::parse(&format!("127.0.0.1:{echo_port}")).unwrap()],
            ..Config::default()
        });

        let mut page = Page::open(port, &format!("target=127.0.0.1:{echo_port}"));
        page.send(b"hello");
        assert_eq!(page.next(), (0x2, b"hello".to_vec()));
        // The server hung up: the page is told the connection closed.
        assert_eq!(page.closed().0, 1000);

        let mut page = Page::open(port, "target=127.0.0.1:1");
        assert_eq!(
            page.closed(),
            (1008, "This relay doesn't reach 127.0.0.1:1".to_owned())
        );
    }

    /// A host's room carries messages between it and its guests, by id.
    #[test]
    fn rooms_introduce_pages() {
        let port = relay_on(Config::default());
        let mut host = Page::open(port, "host");
        let room = host.text();
        let code = room.strip_prefix("room ").unwrap().to_owned();
        assert_eq!(code.len(), 9);

        let mut guest = Page::open(port, &format!("join={}", code.to_lowercase()));
        guest.send(b"offer");
        assert_eq!(host.text(), "1 offer");
        host.send(b"1 answer");
        assert_eq!(guest.text(), "answer");
        // Another guest's messages are its own.
        let mut second = Page::open(port, &format!("join={}", code.replace('-', "")));
        second.send(b"another");
        assert_eq!(host.text(), "2 another");
        drop(guest);
        assert_eq!(host.text(), "left 1");

        let mut lost = Page::open(port, "join=0000-0000");
        assert_eq!(
            lost.closed(),
            (1008, "No world is shared with that code".to_owned())
        );
        // The room closes with its host.
        drop(host);
        assert_eq!(second.closed(), (1001, "The world closed".to_owned()));
    }
}
