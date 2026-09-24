//! The gate in front of the guest's sockets.
//!
//! Every WASIX socket call reaches the host through the runtime's
//! [`VirtualNetworking`]. [`FilteredNetworking`] wraps the real implementation
//! and asks the [`NetPolicy`] before delegating, so the guest cannot open a
//! connection the policy did not see.
//!
//! Only name resolution and outgoing TCP connections are checked per
//! destination. A TCP socket bound before connecting, a UDP socket, a
//! listener or a raw/ICMP socket can later talk to any address without
//! passing through this type again, so those are refused unless the mode is
//! `host` (or recorded as audit findings in `audit` mode). Methods not
//! overridden here keep the trait's default, which reports `Unsupported`.

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
    policy::{NetMode, NetPolicy, Verdict},
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
    pub fn new(inner: DynVirtualNetworking, policy: Arc<NetPolicy>, log: Arc<ConnectionLog>) -> Self {
        Self { inner, policy, log }
    }

    fn gate(&self, event: Event<'_>, verdict: Verdict) -> Result<()> {
        self.log.record(event, &verdict);
        if verdict.allowed() {
            Ok(())
        } else {
            Err(NetworkError::PermissionDenied)
        }
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

#[async_trait::async_trait]
impl VirtualNetworking for FilteredNetworking {
    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        dns_server: Option<IpAddr>,
    ) -> Result<Vec<IpAddr>> {
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
        let verdict = self.policy.check_connect(peer.ip(), peer.port());
        let host = self.policy.name_of(peer.ip());
        self.gate(
            Event {
                source: GUEST,
                kind: "tcp",
                host: host.as_deref(),
                addr: Some(peer.ip()),
                port: Some(peer.port()),
            },
            verdict,
        )?;
        self.inner.connect_tcp(addr, peer).await
    }

    async fn bind_tcp(
        &self,
        addr: SocketAddr,
        only_v6: bool,
        reuse_port: bool,
        reuse_addr: bool,
    ) -> Result<Box<dyn VirtualTcpBoundSocket + Sync>> {
        // A bound socket connects without passing through `connect_tcp`, so
        // binding is refused outside host mode. The WASIX build of CPython
        // binds every new TCP socket to 0.0.0.0:10275 and ignores the
        // failure; refusing that silently keeps its connects on the checked
        // path without a misleading note in every result. Audit mode refuses
        // it too, so that its connects are still logged.
        if addr.ip().is_unspecified() && self.policy.mode() != NetMode::Host {
            tracing::debug!(%addr, "refusing wildcard bind of a TCP socket");
            return Err(NetworkError::PermissionDenied);
        }
        self.socket_gate("bind", "binding a TCP socket before connecting", Some(addr))?;
        self.inner
            .bind_tcp(addr, only_v6, reuse_port, reuse_addr)
            .await
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
    }

    fn setup(mode: NetMode, allow: &[&str]) -> (Arc<Fake>, FilteredNetworking, Arc<ConnectionLog>) {
        let fake = Arc::new(Fake::default());
        let log = Arc::new(ConnectionLog::memory());
        let policy = Arc::new(NetPolicy::new(mode, NetPolicy::parse_entries(allow).unwrap()));
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
            net.listen_tcp(sa("127.0.0.1:8080"), false, false, false).await,
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
        assert!(notes.iter().any(|n| n.contains("dns other.com")), "{notes:?}");
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
            net.bind_tcp(sa("127.0.0.1:5000"), false, false, false).await,
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
    async fn audit_mode_lets_everything_through_and_notes_it() {
        let (fake, net, log) = setup(NetMode::Audit, &[]);
        net.resolve("other.com", None, None).await.unwrap();
        let _ = net.connect_tcp(sa("0.0.0.0:0"), sa("93.184.216.34:443")).await;
        let _ = net.bind_udp(sa("0.0.0.0:0"), false, false).await;
        assert_eq!(fake.calls.lock().unwrap().len(), 3);
        assert_eq!(log.notes_since(0).len(), 3);
    }
}
