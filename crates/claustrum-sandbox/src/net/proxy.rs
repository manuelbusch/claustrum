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
//! per-sandbox secret, so other local processes cannot use it. It is
//! cooperative: a program that ignores the proxy variables is not stopped
//! until actions are confined.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use super::{
    log::{ConnectionLog, Event},
    policy::{NetPolicy, Verdict, normalize_name},
};

/// Largest request head accepted.
const MAX_HEAD: usize = 16 * 1024;
/// Time allowed for the request head to arrive.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed to connect upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ActionProxy;

/// A running proxy; stops when dropped.
#[derive(Debug)]
pub struct ProxyHandle {
    addr: SocketAddr,
    secret: String,
    task: JoinHandle<()>,
}

impl ProxyHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Proxy URL for one action; its name becomes the log source.
    pub fn url_for(&self, action: &str) -> String {
        format!("http://{action}:{}@{}", self.secret, self.addr)
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ActionProxy {
    /// Listen on `127.0.0.1` on a free port. Needs a tokio runtime.
    pub async fn start(
        policy: Arc<NetPolicy>,
        log: Arc<ConnectionLog>,
    ) -> std::io::Result<ProxyHandle> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let secret = random_secret()?;
        let expected = secret.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let policy = Arc::clone(&policy);
                let log = Arc::clone(&log);
                let expected = expected.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(stream, &policy, &log, &expected).await {
                        tracing::debug!(error = %e, "proxy connection ended");
                    }
                });
            }
        });
        Ok(ProxyHandle { addr, secret, task })
    }
}

fn random_secret() -> std::io::Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn respond(
    stream: &mut TcpStream,
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
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
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

async fn serve(
    mut stream: TcpStream,
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
        let Some((host, port)) = split_authority(&target, 443) else {
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
        tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
        return Ok(());
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
    let Some((host, port)) = split_authority(authority, 80) else {
        return respond(&mut stream, 400, "Bad Request", "bad request target\n").await;
    };
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
    let mut out = format!("{method} {path} HTTP/1.1\r\n");
    for h in req.headers.iter() {
        let name = h.name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
        ) {
            continue;
        }
        out.push_str(h.name);
        out.push_str(": ");
        out.push_str(&String::from_utf8_lossy(h.value));
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    upstream.write_all(out.as_bytes()).await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::policy::NetMode;

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
