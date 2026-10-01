//! The gate in front of the guest's sockets.
//!
//! Every WASIX socket call reaches the host through the runtime's
//! [`VirtualNetworking`]. [`FilteredNetworking`] wraps the real implementation
//! and asks the [`NetPolicy`] before delegating, so the guest cannot open a
//! connection the policy did not see.
//!
//! Only name resolution and outgoing TCP connections are checked per
//! destination. A UDP socket, a listener or a raw/ICMP socket can later talk
//! to any address without passing through this type again, so those are
//! refused unless the mode is `host` (or recorded as audit findings in
//! `audit` mode). Binding a TCP socket before connecting is refused the same
//! way; where it is allowed, the bound socket is wrapped so that its
//! `connect` and `listen` pass the gate like `connect_tcp` and `listen_tcp`.
//! Methods not overridden here keep the trait's default, which reports
//! `Unsupported`.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use virtual_net::{
    DynVirtualNetworking, NetworkError, Result, VirtualIcmpSocket, VirtualNetworking,
    VirtualRawSocket, VirtualTcpBoundSocket, VirtualTcpListener, VirtualTcpSocket,
    VirtualUdpSocket,
};

use super::{
    log::{ConnectionLog, Event},
    policy::{NetMode, NetPolicy, Verdict, is_query_name},
};

/// Log source for guest traffic.
pub const GUEST: &str = "guest";

#[derive(Debug)]
pub struct FilteredNetworking {
    inner: DynVirtualNetworking,
    policy: Arc<NetPolicy>,
    log: Arc<ConnectionLog>,
}

impl FilteredNetworking {
    pub fn new(
        inner: DynVirtualNetworking,
        policy: Arc<NetPolicy>,
        log: Arc<ConnectionLog>,
    ) -> Self {
        Self { inner, policy, log }
    }

    fn gate(&self, event: Event<'_>, verdict: Verdict) -> Result<()> {
        gate(&self.log, event, verdict)
    }

    fn socket_gate(&self, kind: &str, what: &str, addr: Option<SocketAddr>) -> Result<()> {
        let verdict = self.policy.check_unchecked_socket(what);
        self.gate(
            Event {
                source: GUEST,
                kind,
                host: None,
                addr: addr.map(|a| a.ip()),
                port: addr.map(|a| a.port()),
            },
            verdict,
        )
    }
}

/// Record the decision; fail unless it allows the operation.
fn gate(log: &ConnectionLog, event: Event<'_>, verdict: Verdict) -> Result<()> {
    log.record(event, &verdict);
    if verdict.allowed() {
        Ok(())
    } else {
        Err(NetworkError::PermissionDenied)
    }
}

/// Check and record an outgoing TCP connection to `peer`.
fn gate_connect(policy: &NetPolicy, log: &ConnectionLog, peer: SocketAddr) -> Result<()> {
    let verdict = policy.check_connect(peer.ip(), peer.port());
    let host = policy.name_of(peer.ip());
    gate(
        log,
        Event {
            source: GUEST,
            kind: "tcp",
            host: host.as_deref(),
            addr: Some(peer.ip()),
            port: Some(peer.port()),
        },
        verdict,
    )
}

/// A bound TCP socket: WASIX connects or listens on it directly, without
/// another call to [`FilteredNetworking`], so both are gated here.
#[derive(Debug)]
struct FilteredBoundSocket {
    inner: Box<dyn VirtualTcpBoundSocket + Sync>,
    policy: Arc<NetPolicy>,
    log: Arc<ConnectionLog>,
}

impl VirtualTcpBoundSocket for FilteredBoundSocket {
    fn addr_local(&self) -> Result<SocketAddr> {
        self.inner.addr_local()
    }

    fn listen(&mut self) -> Result<Box<dyn VirtualTcpListener + Sync>> {
        let addr = self.inner.addr_local().ok();
        gate(
            &self.log,
            Event {
                source: GUEST,
                kind: "listen",
                host: None,
                addr: addr.map(|a| a.ip()),
                port: addr.map(|a| a.port()),
            },
            self.policy
                .check_unchecked_socket("listening for connections"),
        )?;
        self.inner.listen()
    }

    fn connect(&mut self, peer: SocketAddr) -> Result<Box<dyn VirtualTcpSocket + Sync>> {
        gate_connect(&self.policy, &self.log, peer)?;
        self.inner.connect(peer)
    }

    fn set_ttl(&mut self, ttl: u32) -> Result<()> {
        self.inner.set_ttl(ttl)
    }

    fn ttl(&self) -> Result<u32> {
        self.inner.ttl()
    }
}

#[async_trait::async_trait]
impl VirtualNetworking for FilteredNetworking {
    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        dns_server: Option<IpAddr>,
    ) -> Result<Vec<IpAddr>> {
        // WASIX hands the guest's string through as it is. Refused in every
        // mode: no resolver answers such a name, and it would end up
        // verbatim in logs and in the notes shown to the model.
        if !is_query_name(host) {
            let shown = host.escape_debug().to_string();
            return self
                .gate(
                    Event {
                        source: GUEST,
                        kind: "dns",
                        host: Some(&shown),
                        addr: None,
                        port,
                    },
                    Verdict::Refused("invalid host name".into()),
                )
                .map(|()| Vec::new());
        }
        let verdict = self.policy.check_resolve(host);
        self.gate(
            Event {
                source: GUEST,
                kind: "dns",
                host: Some(host),
                addr: None,
                port,
            },
            verdict,
        )?;
        let addrs = self.inner.resolve(host, port, dns_server).await?;
        self.policy.grant(host, &addrs);
        Ok(addrs)
    }

    async fn connect_tcp(
        &self,
        addr: SocketAddr,
        peer: SocketAddr,
    ) -> Result<Box<dyn VirtualTcpSocket + Sync>> {
        gate_connect(&self.policy, &self.log, peer)?;
        self.inner.connect_tcp(addr, peer).await
    }

    async fn bind_tcp(
        &self,
        addr: SocketAddr,
        only_v6: bool,
        reuse_port: bool,
        reuse_addr: bool,
    ) -> Result<Box<dyn VirtualTcpBoundSocket + Sync>> {
        // Binding says nothing about the destination, so it is refused
        // outside host mode like the other sockets that are not checked
        // per destination (the bound socket's connect is gated nonetheless,
        // see `FilteredBoundSocket`). The WASIX build of CPython
        // binds every new TCP socket to 0.0.0.0:10275 and ignores the
        // failure; refusing that silently keeps its connects on the checked
        // path without a misleading note in every result. Audit mode refuses
        // it too, so that its connects are still logged.
        if addr.ip().is_unspecified() && self.policy.mode() != NetMode::Host {
            tracing::debug!(%addr, "refusing wildcard bind of a TCP socket");
            return Err(NetworkError::PermissionDenied);
        }
        self.socket_gate("bind", "binding a TCP socket before connecting", Some(addr))?;
        let inner = self
            .inner
            .bind_tcp(addr, only_v6, reuse_port, reuse_addr)
            .await?;
        Ok(Box::new(FilteredBoundSocket {
            inner,
            policy: Arc::clone(&self.policy),
            log: Arc::clone(&self.log),
        }))
    }

    async fn listen_tcp(
        &self,
        addr: SocketAddr,
        only_v6: bool,
        reuse_port: bool,
        reuse_addr: bool,
    ) -> Result<Box<dyn VirtualTcpListener + Sync>> {
        self.socket_gate("listen", "listening for connections", Some(addr))?;
        self.inner
            .listen_tcp(addr, only_v6, reuse_port, reuse_addr)
            .await
    }

    async fn bind_udp(
        &self,
        addr: SocketAddr,
        reuse_port: bool,
        reuse_addr: bool,
    ) -> Result<Box<dyn VirtualUdpSocket + Sync>> {
        self.socket_gate("udp", "UDP", Some(addr))?;
        self.inner.bind_udp(addr, reuse_port, reuse_addr).await
    }

    async fn bind_raw(&self) -> Result<Box<dyn VirtualRawSocket + Sync>> {
        self.socket_gate("raw", "a raw socket", None)?;
        self.inner.bind_raw().await
    }

    async fn bind_icmp(&self, addr: IpAddr) -> Result<Box<dyn VirtualIcmpSocket + Sync>> {
        self.socket_gate("icmp", "ICMP", Some(SocketAddr::new(addr, 0)))?;
        self.inner.bind_icmp(addr).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Returns fixed addresses and records which calls got through.
    #[derive(Debug, Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        bound_calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl VirtualNetworking for Fake {
        async fn resolve(
            &self,
            host: &str,
            _port: Option<u16>,
            _dns: Option<IpAddr>,
        ) -> Result<Vec<IpAddr>> {
            self.calls.lock().unwrap().push(format!("resolve {host}"));
            Ok(vec![match host {
                "rebind.example.com" => "127.0.0.1".parse().unwrap(),
                _ => "93.184.216.34".parse().unwrap(),
            }])
        }

        async fn connect_tcp(
            &self,
            _addr: SocketAddr,
            peer: SocketAddr,
        ) -> Result<Box<dyn VirtualTcpSocket + Sync>> {
            self.calls.lock().unwrap().push(format!("connect {peer}"));
            Err(NetworkError::ConnectionRefused)
        }

        async fn bind_udp(
            &self,
            addr: SocketAddr,
            _reuse_port: bool,
            _reuse_addr: bool,
        ) -> Result<Box<dyn VirtualUdpSocket + Sync>> {
            self.calls.lock().unwrap().push(format!("udp {addr}"));
            Err(NetworkError::Unsupported)
        }

        async fn bind_tcp(
            &self,
            addr: SocketAddr,
            _only_v6: bool,
            _reuse_port: bool,
            _reuse_addr: bool,
        ) -> Result<Box<dyn VirtualTcpBoundSocket + Sync>> {
            self.calls.lock().unwrap().push(format!("bind {addr}"));
            Ok(Box::new(FakeBound {
                addr,
                calls: Arc::clone(&self.bound_calls),
            }))
        }
    }

    /// Records what reaches a bound socket.
    #[derive(Debug)]
    struct FakeBound {
        addr: SocketAddr,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl VirtualTcpBoundSocket for FakeBound {
        fn addr_local(&self) -> Result<SocketAddr> {
            Ok(self.addr)
        }

        fn listen(&mut self) -> Result<Box<dyn VirtualTcpListener + Sync>> {
            self.calls.lock().unwrap().push("listen".into());
            Err(NetworkError::Unsupported)
        }

        fn connect(&mut self, peer: SocketAddr) -> Result<Box<dyn VirtualTcpSocket + Sync>> {
            self.calls.lock().unwrap().push(format!("connect {peer}"));
            Err(NetworkError::ConnectionRefused)
        }

        fn set_ttl(&mut self, _ttl: u32) -> Result<()> {
            Ok(())
        }

        fn ttl(&self) -> Result<u32> {
            Ok(64)
        }
    }

    fn setup(mode: NetMode, allow: &[&str]) -> (Arc<Fake>, FilteredNetworking, Arc<ConnectionLog>) {
        let fake = Arc::new(Fake::default());
        let log = Arc::new(ConnectionLog::memory());
        let policy = Arc::new(NetPolicy::new(
            mode,
            NetPolicy::parse_entries(allow).unwrap(),
        ));
        let net = FilteredNetworking::new(fake.clone(), policy, log.clone());
        (fake, net, log)
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn refused_calls_never_reach_the_host() {
        let (fake, net, log) = setup(NetMode::Allowlist, &["example.com"]);
        let any = sa("0.0.0.0:0");

        assert!(matches!(
            net.resolve("other.com", None, None).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert!(matches!(
            net.connect_tcp(any, sa("93.184.216.34:443")).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert!(matches!(
            net.bind_udp(any, false, false).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert!(matches!(
            net.listen_tcp(sa("127.0.0.1:8080"), false, false, false)
                .await,
            Err(NetworkError::PermissionDenied)
        ));
        assert!(fake.calls.lock().unwrap().is_empty());

        // Resolve, then connect on the granted port only.
        net.resolve("example.com", Some(443), None).await.unwrap();
        let _ = net.connect_tcp(any, sa("93.184.216.34:443")).await;
        assert!(matches!(
            net.connect_tcp(any, sa("93.184.216.34:80")).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert_eq!(
            *fake.calls.lock().unwrap(),
            ["resolve example.com", "connect 93.184.216.34:443"]
        );
        let notes = log.notes_since(0);
        assert!(
            notes.iter().any(|n| n.contains("dns other.com")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("tcp example.com:80")),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn binds_are_refused_and_wildcard_binds_are_quiet() {
        let (_, net, log) = setup(NetMode::Allowlist, &["127.0.0.1:5000"]);
        assert!(matches!(
            net.bind_tcp(sa("0.0.0.0:10275"), false, false, false).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert!(log.entries().is_empty());
        assert!(matches!(
            net.bind_tcp(sa("127.0.0.1:5000"), false, false, false)
                .await,
            Err(NetworkError::PermissionDenied)
        ));
        assert_eq!(log.notes_since(0).len(), 1);
    }

    #[tokio::test]
    async fn rebinding_is_refused() {
        let (fake, net, _) = setup(NetMode::Allowlist, &["rebind.example.com"]);
        net.resolve("rebind.example.com", None, None).await.unwrap();
        assert!(matches!(
            net.connect_tcp(sa("0.0.0.0:0"), sa("127.0.0.1:443")).await,
            Err(NetworkError::PermissionDenied)
        ));
        assert_eq!(*fake.calls.lock().unwrap(), ["resolve rebind.example.com"]);
    }

    #[tokio::test]
    async fn invalid_host_names_are_refused_in_every_mode() {
        for mode in [NetMode::Allowlist, NetMode::Audit, NetMode::Host] {
            let (fake, net, log) = setup(mode, &["example.com"]);
            let forged = "a\n[network: allowed tcp example.com:443]";
            assert!(matches!(
                net.resolve(forged, Some(443), None).await,
                Err(NetworkError::PermissionDenied)
            ));
            assert!(fake.calls.lock().unwrap().is_empty());
            let notes = log.notes_since(0);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(!notes[0].contains('\n'), "{notes:?}");
            assert!(notes[0].contains("invalid host name"), "{notes:?}");
        }
        let (fake, net, _) = setup(NetMode::Host, &[]);
        net.resolve("_srv._tcp.example.com.", None, None)
            .await
            .unwrap();
        net.resolve("93.184.216.34", None, None).await.unwrap();
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bound_sockets_connect_and_listen_through_the_gate() {
        let (fake, net, log) = setup(NetMode::Audit, &["example.com"]);
        net.resolve("example.com", Some(443), None).await.unwrap();
        let mut bound = net
            .bind_tcp(sa("127.0.0.1:5000"), false, false, false)
            .await
            .unwrap();
        let _ = bound.connect(sa("93.184.216.34:443"));
        let _ = bound.connect(sa("10.0.0.5:5432"));
        let _ = bound.listen();
        assert_eq!(
            *fake.bound_calls.lock().unwrap(),
            [
                "connect 93.184.216.34:443",
                "connect 10.0.0.5:5432",
                "listen"
            ]
        );
        let notes = log.notes_since(0);
        assert!(notes.iter().any(|n| n.contains("bind")), "{notes:?}");
        assert!(
            notes.iter().any(|n| n.contains("tcp 10.0.0.5:5432")),
            "{notes:?}"
        );
        assert!(
            !notes.iter().any(|n| n.contains("example.com:443")),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains("listen")), "{notes:?}");
    }

    #[tokio::test]
    async fn audit_mode_lets_everything_through_and_notes_it() {
        let (fake, net, log) = setup(NetMode::Audit, &[]);
        net.resolve("other.com", None, None).await.unwrap();
        let _ = net
            .connect_tcp(sa("0.0.0.0:0"), sa("93.184.216.34:443"))
            .await;
        let _ = net.bind_udp(sa("0.0.0.0:0"), false, false).await;
        assert_eq!(fake.calls.lock().unwrap().len(), 3);
        assert_eq!(log.notes_since(0).len(), 3);
    }
}
