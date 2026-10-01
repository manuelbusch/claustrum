//! Local HTTP proxy for host actions.
//!
//! Host actions run as ordinary host processes, so the guest gate does not
//! see their traffic. They are started with `HTTP_PROXY`/`HTTPS_PROXY`
//! pointing here, and this proxy applies the same [`NetPolicy`]:
//!
//! - `CONNECT host:port` (HTTPS, git over HTTPS, cargo, npm, pip) opens a
//!   tunnel after the name and every resolved address passed the policy;
//! - absolute-form requests (`GET http://host/path`) are forwarded once with
//!   `Connection: close`.
//!
//! The proxy listens on loopback only and requires Basic credentials: the
//! user name is the action (used as the log source), the password a random
//! per-sandbox secret, so other local processes cannot use it. Confined
//! actions cannot reach anything else, so for them the proxy is enforced; an
//! unconfined action could ignore the proxy variables.
//!
//! On Linux the proxy also listens on a Unix socket in a private directory:
//! a confined action there has its own network namespace, where the
//! confinement helper relays `localhost:<port>` to that socket.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use super::{
    log::{ConnectionLog, Event},
    policy::{NetPolicy, Verdict, is_query_name, normalize_name},
};

/// Largest request head accepted.
const MAX_HEAD: usize = 16 * 1024;
/// Time allowed for the request head to arrive.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed to connect upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Connections served at once; more wait in the listen backlog. Keeps an
/// action from exhausting the broker's file descriptors.
const MAX_CONNECTIONS: usize = 256;
/// A relayed connection without traffic in either direction for this long
/// is closed, so half-open connections do not hold descriptors forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Pause after a failed `accept` (e.g. `EMFILE`), instead of spinning.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

pub struct ActionProxy;

/// A running proxy; stops when dropped.
pub struct ProxyHandle {
    addr: SocketAddr,
    secret: String,
    task: JoinHandle<()>,
    /// Unix socket of the proxy and its private directory (Linux only).
    unix: Option<(std::path::PathBuf, JoinHandle<()>)>,
}

impl ProxyHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy's Unix socket, where there is one (see the module docs).
    pub fn unix_socket(&self) -> Option<std::path::PathBuf> {
        self.unix.as_ref().map(|(dir, _)| dir.join(UNIX_SOCKET))
    }

    /// Proxy URL for one action; its name becomes the log source.
    pub fn url_for(&self, action: &str) -> String {
        format!("http://{action}:{}@{}", self.secret, self.addr)
    }
}

/// Without the secret: whoever holds it can use the proxy, and handles are
/// reachable from types that end up in debug logs.
impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("addr", &self.addr)
            .field("unix_socket", &self.unix_socket())
            .finish_non_exhaustive()
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
        if let Some((dir, task)) = self.unix.take() {
            task.abort();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// File name of the Unix socket inside its private directory.
const UNIX_SOCKET: &str = "proxy.sock";

impl ActionProxy {
    /// Listen on `127.0.0.1` on a free port. Needs a tokio runtime.
    pub async fn start(
        policy: Arc<NetPolicy>,
        log: Arc<ConnectionLog>,
    ) -> std::io::Result<ProxyHandle> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let secret = random_secret()?;
        let unix = if cfg!(target_os = "linux") {
            Some(start_unix(&policy, &log, &secret)?)
        } else {
            None
        };
        let task = accept_loop(listener, policy, log, secret.clone());
        Ok(ProxyHandle {
            addr,
            secret,
            task,
            unix,
        })
    }
}

/// Listen on a Unix socket in a fresh directory only the user can enter.
#[cfg(unix)]
fn start_unix(
    policy: &Arc<NetPolicy>,
    log: &Arc<ConnectionLog>,
    secret: &str,
) -> std::io::Result<(std::path::PathBuf, JoinHandle<()>)> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = std::env::temp_dir().canonicalize()?.join(format!(
        "claustrum-proxy-{}-{}",
        std::process::id(),
        &random_secret()?[..8]
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let listener = tokio::net::UnixListener::bind(dir.join(UNIX_SOCKET))?;
    let task = accept_loop(
        listener,
        Arc::clone(policy),
        Arc::clone(log),
        secret.to_owned(),
    );
    Ok((dir, task))
}

/// A listener the proxy serves.
trait Listener: Send + 'static {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    fn accept_one(&self)
    -> impl std::future::Future<Output = std::io::Result<Self::Stream>> + Send;
}

impl Listener for TcpListener {
    type Stream = TcpStream;
    async fn accept_one(&self) -> std::io::Result<TcpStream> {
        self.accept().await.map(|(s, _)| s)
    }
}

#[cfg(unix)]
impl Listener for tokio::net::UnixListener {
    type Stream = tokio::net::UnixStream;
    async fn accept_one(&self) -> std::io::Result<tokio::net::UnixStream> {
        self.accept().await.map(|(s, _)| s)
    }
}

/// Serve `listener` until the task is aborted: at most [`MAX_CONNECTIONS`]
/// at once, pausing after a failed `accept` instead of retrying at once.
fn accept_loop<L: Listener>(
    listener: L,
    policy: Arc<NetPolicy>,
    log: Arc<ConnectionLog>,
    secret: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                return;
            };
            let stream = match listener.accept_one().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "proxy cannot accept a connection");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
            let (policy, log, secret) = (Arc::clone(&policy), Arc::clone(&log), secret.clone());
            tokio::spawn(async move {
                let _slot = slot;
                if let Err(e) = serve(stream, &policy, &log, &secret).await {
                    tracing::debug!(error = %e, "proxy connection ended");
                }
            });
        }
    })
}

/// Copy between `a` and `b` in both directions until both are done, like
/// `copy_bidirectional`, but give up after [`IDLE_TIMEOUT`] without any
/// traffic.
async fn relay(
    a: impl AsyncRead + AsyncWrite + Unpin + Send,
    b: impl AsyncRead + AsyncWrite + Unpin + Send,
    idle: Duration,
) -> std::io::Result<()> {
    let last = std::sync::Mutex::new(tokio::time::Instant::now());
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let both = async {
        tokio::try_join!(pump(&mut ar, &mut bw, &last), pump(&mut br, &mut aw, &last)).map(|_| ())
    };
    let watchdog = async {
        loop {
            tokio::time::sleep(idle / 4).await;
            let since = last.lock().unwrap_or_else(|p| p.into_inner()).elapsed();
            if since > idle {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connection idle",
                ));
            }
        }
    };
    tokio::select! {
        r = both => r,
        e = watchdog => e,
    }
}

/// One direction of [`relay`]; records the time of the last traffic.
async fn pump(
    r: &mut (impl AsyncRead + Unpin),
    w: &mut (impl AsyncWrite + Unpin),
    last: &std::sync::Mutex<tokio::time::Instant>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return w.shutdown().await;
        }
        w.write_all(&buf[..n]).await?;
        *last.lock().unwrap_or_else(|p| p.into_inner()) = tokio::time::Instant::now();
    }
}

#[cfg(not(unix))]
fn start_unix(
    _: &Arc<NetPolicy>,
    _: &Arc<ConnectionLog>,
    _: &str,
) -> std::io::Result<(std::path::PathBuf, JoinHandle<()>)> {
    Err(std::io::Error::other("no Unix sockets on this platform"))
}

fn random_secret() -> std::io::Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn respond(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let extra = if status == 407 {
        "Proxy-Authenticate: Basic realm=\"claustrum\"\r\n"
    } else {
        ""
    };
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\n{extra}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(msg.as_bytes()).await?;
    stream.shutdown().await
}

/// Read until the end of the request head. Returns head and any bytes after it.
async fn read_head(
    stream: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if end + 4 > MAX_HEAD {
                return Ok(None);
            }
            let rest = buf.split_off(end + 4);
            return Ok(Some((buf, rest)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Split `host:port` / `[v6]:port`.
fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_owned(), p.parse().ok()?)),
        Some(_) => None,
        None => Some((authority.to_owned(), default_port)),
    }
}

/// Decode `Proxy-Authorization: Basic ...` into the user name if the
/// password matches.
fn authenticate(headers: &[httparse::Header<'_>], expected: &str) -> Option<String> {
    let value = headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("proxy-authorization"))?
        .value;
    let value = std::str::from_utf8(value).ok()?.trim();
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    let ok = pass.len() == expected.len()
        && pass
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    let valid_user = !user.is_empty()
        && user.len() <= 32
        && user
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    (ok && valid_user).then(|| user.to_owned())
}

/// Check the name and port, resolve, check every address, connect.
async fn open_upstream(
    policy: &NetPolicy,
    log: &ConnectionLog,
    source: &str,
    kind: &str,
    host: &str,
    port: u16,
) -> Result<TcpStream, String> {
    let host = normalize_name(host);
    let ev = |addr: Option<IpAddr>| Event {
        source,
        kind,
        host: Some(host.as_str()),
        addr,
        port: Some(port),
    };
    let verdict = policy.check_name_port(&host, port);
    log.record(ev(None), &verdict);
    if let Verdict::Refused(r) = verdict {
        return Err(r);
    }
    let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| format!("cannot resolve {host}: {e}"))?
            .map(|a| a.ip())
            .collect(),
    };
    let mut last_err = format!("{host} has no addresses");
    for ip in addrs {
        let verdict = policy.check_resolved_addr(ip, port);
        if let Verdict::Refused(r) = &verdict {
            log.record(ev(Some(ip)), &verdict);
            last_err = r.clone();
            continue;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port))).await {
            Ok(Ok(stream)) => {
                if matches!(verdict, Verdict::Audited(_)) {
                    log.record(ev(Some(ip)), &verdict);
                }
                return Ok(stream);
            }
            Ok(Err(e)) => last_err = format!("cannot connect to {ip}:{port}: {e}"),
            Err(_) => last_err = format!("connecting to {ip}:{port} timed out"),
        }
    }
    Err(last_err)
}

/// Refuse a request whose end is ambiguous: with duplicate, differing
/// `Content-Length` headers or `Transfer-Encoding` next to them, the proxy
/// and the server behind it could disagree on where it ends, and the rest
/// would reach the server as a request of its own (request smuggling).
fn check_framing(headers: &[httparse::Header<'_>]) -> Result<(), &'static str> {
    let values = |name: &str| {
        headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.trim_ascii())
            .collect::<Vec<_>>()
    };
    let lengths = values("content-length");
    if let Some(first) = lengths.first() {
        if first.is_empty() || !first.iter().all(u8::is_ascii_digit) {
            return Err("invalid Content-Length");
        }
        if lengths.iter().any(|l| l != first) {
            return Err("conflicting Content-Length headers");
        }
    }
    let encodings = values("transfer-encoding");
    if !encodings.is_empty() {
        if !lengths.is_empty() {
            return Err("both Transfer-Encoding and Content-Length");
        }
        if encodings.len() > 1 || !encodings[0].eq_ignore_ascii_case(b"chunked") {
            return Err("only Transfer-Encoding: chunked is supported");
        }
    }
    Ok(())
}

/// The request head sent upstream for a plain HTTP request: origin-form
/// target, the checked `authority` as `Host` (the client's own `Host` could
/// name another site on the same server), without hop-by-hop and proxy
/// headers, and `Connection: close`.
fn forward_head(
    method: &str,
    path: &str,
    authority: &str,
    headers: &[httparse::Header<'_>],
) -> String {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\n");
    for h in headers {
        let name = h.name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host" | "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
        ) {
            continue;
        }
        out.push_str(h.name);
        out.push_str(": ");
        out.push_str(&String::from_utf8_lossy(h.value));
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

async fn serve(
    mut stream: impl AsyncRead + AsyncWrite + Unpin + Send,
    policy: &NetPolicy,
    log: &ConnectionLog,
    secret: &str,
) -> std::io::Result<()> {
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(r) => r?,
        Err(_) => return respond(&mut stream, 408, "Request Timeout", "").await,
    };
    let Some((head, rest)) = head else {
        return respond(&mut stream, 431, "Request Header Fields Too Large", "").await;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(&head), Ok(httparse::Status::Complete(_))) {
        return respond(&mut stream, 400, "Bad Request", "malformed request\n").await;
    }
    let method = req.method.unwrap_or_default().to_owned();
    let target = req.path.unwrap_or_default().to_owned();
    let Some(user) = authenticate(req.headers, secret) else {
        return respond(&mut stream, 407, "Proxy Authentication Required", "").await;
    };
    let source = format!("action:{user}");

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_authority(&target, 443).filter(|(h, _)| is_query_name(h))
        else {
            return respond(&mut stream, 400, "Bad Request", "bad CONNECT target\n").await;
        };
        let mut upstream = match open_upstream(policy, log, &source, "connect", &host, port).await {
            Ok(s) => s,
            Err(reason) => {
                return respond(
                    &mut stream,
                    403,
                    "Forbidden",
                    &format!("claustrum: {host}:{port} refused: {reason}\n"),
                )
                .await;
            }
        };
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        return relay(stream, upstream, IDLE_TIMEOUT).await;
    }

    // Absolute-form plain HTTP.
    let Some(after_scheme) = target.strip_prefix("http://") else {
        return respond(
            &mut stream,
            400,
            "Bad Request",
            "only CONNECT and absolute http:// requests are supported\n",
        )
        .await;
    };
    let (authority, path) = match after_scheme.find('/') {
        Some(i) => after_scheme.split_at(i),
        None => (after_scheme, "/"),
    };
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let Some((host, port)) = split_authority(authority, 80).filter(|(h, _)| is_query_name(h))
    else {
        return respond(&mut stream, 400, "Bad Request", "bad request target\n").await;
    };
    if let Err(why) = check_framing(req.headers) {
        return respond(&mut stream, 400, "Bad Request", &format!("{why}\n")).await;
    }
    let mut upstream = match open_upstream(policy, log, &source, "http", &host, port).await {
        Ok(s) => s,
        Err(reason) => {
            return respond(
                &mut stream,
                403,
                "Forbidden",
                &format!("claustrum: {host}:{port} refused: {reason}\n"),
            )
            .await;
        }
    };
    let out = forward_head(&method, path, authority, req.headers);
    upstream.write_all(out.as_bytes()).await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    relay(stream, upstream, IDLE_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::policy::NetMode;

    fn header<'a>(name: &'a str, value: &'a str) -> httparse::Header<'a> {
        httparse::Header {
            name,
            value: value.as_bytes(),
        }
    }

    #[tokio::test]
    async fn debug_output_leaves_out_the_secret() {
        let handle = ProxyHandle {
            addr: "127.0.0.1:1".parse().unwrap(),
            secret: random_secret().unwrap(),
            task: tokio::spawn(async {}),
            unix: None,
        };
        let text = format!("{handle:?}");
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(!text.contains(&handle.secret), "{text}");
    }

    #[test]
    fn ambiguous_request_framing_is_refused() {
        for ok in [
            &[][..],
            &[header("Content-Length", "5")],
            &[
                header("Content-Length", "5"),
                header("content-length", " 5"),
            ],
            &[header("Transfer-Encoding", "chunked")],
        ] {
            assert_eq!(check_framing(ok), Ok(()), "{ok:?}");
        }
        for bad in [
            &[header("Content-Length", "5"), header("Content-Length", "6")][..],
            &[header("Content-Length", "5, 5")],
            &[header("Content-Length", "")],
            &[
                header("Transfer-Encoding", "chunked"),
                header("Content-Length", "5"),
            ],
            &[header("Transfer-Encoding", "gzip, chunked")],
            &[
                header("Transfer-Encoding", "chunked"),
                header("Transfer-Encoding", "chunked"),
            ],
        ] {
            assert!(check_framing(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn forwarded_requests_name_the_checked_host() {
        let head = forward_head(
            "GET",
            "/x",
            "allowed.example:8080",
            &[
                header("Host", "other.example"),
                header("Proxy-Authorization", "Basic c2VjcmV0"),
                header("Connection", "keep-alive"),
                header("Accept", "*/*"),
            ],
        );
        assert_eq!(
            head,
            "GET /x HTTP/1.1\r\nHost: allowed.example:8080\r\nAccept: */*\r\n\
             Connection: close\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn relay_copies_both_ways_and_drops_idle_connections() {
        let idle = Duration::from_millis(300);
        let (mut client, a) = tokio::io::duplex(1024);
        let (b, mut server) = tokio::io::duplex(1024);
        let relayed = tokio::spawn(relay(a, b, idle));

        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        // Traffic in one direction only keeps the connection open.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            server.write_all(b"pong").await.unwrap();
            client.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"pong");
        }
        assert!(!relayed.is_finished());

        // Silence: closed after the idle timeout.
        let started = std::time::Instant::now();
        let err = relayed.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn relay_ends_when_both_sides_close() {
        let (mut client, a) = tokio::io::duplex(1024);
        let (b, mut server) = tokio::io::duplex(1024);
        let relayed = tokio::spawn(relay(a, b, Duration::from_secs(60)));
        client.write_all(b"bye").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = Vec::new();
        server.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"bye");
        server.shutdown().await.unwrap();
        drop(server);
        tokio::time::timeout(Duration::from_secs(2), relayed)
            .await
            .expect("relay ended")
            .unwrap()
            .unwrap();
    }

    async fn echo_server() -> SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    /// Answers every request with its own request head as the body.
    async fn mirror_server() -> SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    if let Ok(Some((head, _))) = read_head(&mut s).await {
                        let body = String::from_utf8_lossy(&head).into_owned();
                        let msg = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = s.write_all(msg.as_bytes()).await;
                    }
                });
            }
        });
        addr
    }

    fn auth(handle: &ProxyHandle, user: &str) -> String {
        let pass = &handle.secret;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Proxy-Authorization: Basic {token}\r\n")
    }

    async fn request(proxy: SocketAddr, text: &str) -> (TcpStream, String) {
        let mut s = TcpStream::connect(proxy).await.unwrap();
        s.write_all(text.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        (s, String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    async fn start(allow: &[String]) -> (ProxyHandle, Arc<ConnectionLog>) {
        let policy = Arc::new(NetPolicy::new(
            NetMode::Allowlist,
            NetPolicy::parse_entries(allow).unwrap(),
        ));
        let log = Arc::new(ConnectionLog::memory());
        (ActionProxy::start(policy, log.clone()).await.unwrap(), log)
    }

    #[tokio::test]
    async fn tunnels_allowed_destinations_only() {
        let echo = echo_server().await;
        let (proxy, log) = start(&[format!("127.0.0.1:{}", echo.port())]).await;
        let a = auth(&proxy, "build");

        let (mut s, head) =
            request(proxy.addr(), &format!("CONNECT {echo} HTTP/1.1\r\n{a}\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        s.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        let (_, head) = request(
            proxy.addr(),
            &format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n{a}\r\n", echo.port() + 1),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(head.contains("local or private"), "{head}");

        let (_, head) = request(
            proxy.addr(),
            &format!("CONNECT example.com:443 HTTP/1.1\r\n{a}\r\n"),
        )
        .await;
        assert!(head.contains("not in the allowlist"), "{head}");

        let entries = log.entries();
        assert!(entries.iter().all(|e| e.source == "action:build"));
        assert!(
            entries
                .iter()
                .any(|e| e.verdict == "refused" && e.host.as_deref() == Some("example.com"))
        );
    }

    #[tokio::test]
    async fn requires_credentials() {
        let echo = echo_server().await;
        let (proxy, _) = start(&[format!("127.0.0.1:{}", echo.port())]).await;
        let (_, head) = request(proxy.addr(), &format!("CONNECT {echo} HTTP/1.1\r\n\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 407"), "{head}");
        let bad = base64::engine::general_purpose::STANDARD.encode("build:wrong");
        let (_, head) = request(
            proxy.addr(),
            &format!("CONNECT {echo} HTTP/1.1\r\nProxy-Authorization: Basic {bad}\r\n\r\n"),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 407"), "{head}");
        assert!(proxy.url_for("build").starts_with("http://build:"));
    }

    #[tokio::test]
    async fn forwards_plain_http_and_limits_heads() {
        let mirror = mirror_server().await;
        let (proxy, _) = start(&[format!("127.0.0.1:{}", mirror.port())]).await;
        let a = auth(&proxy, "fetch");
        let (_, resp) = request(
            proxy.addr(),
            &format!(
                "GET http://127.0.0.1:{}/index?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n{a}Proxy-Connection: keep-alive\r\n\r\n",
                mirror.port()
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("GET /index?x=1 HTTP/1.1"), "{resp}");
        assert!(resp.contains("Connection: close"), "{resp}");
        assert!(
            !resp.to_ascii_lowercase().contains("proxy-authorization"),
            "{resp}"
        );

        let huge = format!(
            "GET http://x/ HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD + 10)
        );
        let (_, resp) = request(proxy.addr(), &huge).await;
        assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");
    }

    #[test]
    fn splits_authorities() {
        assert_eq!(
            split_authority("a.com:8080", 443),
            Some(("a.com".into(), 8080))
        );
        assert_eq!(split_authority("a.com", 80), Some(("a.com".into(), 80)));
        assert_eq!(split_authority("[::1]:22", 443), Some(("::1".into(), 22)));
        assert_eq!(split_authority("[::1]", 443), Some(("::1".into(), 443)));
        assert_eq!(split_authority("::1", 443), None);
        assert_eq!(split_authority("a.com:x", 443), None);
    }
}
