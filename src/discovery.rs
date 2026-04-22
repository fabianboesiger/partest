use anyhow::{Context, Result};
use flume::RecvTimeoutError;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::IpAddr;
use std::time::Duration;
use tracing::debug;

const SERVICE_TYPE: &str = "_partest._tcp.local.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub hostname: String,
    pub ip: IpAddr,
    pub ssh_port: u16,
    pub ssh_user: String,
}

impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}:{})", self.hostname, self.ip, self.ssh_port)
    }
}

/// Discover peers on the local network via mDNS. Waits `timeout` for responses.
pub fn discover_peers(timeout: Duration) -> Result<Vec<Peer>> {
    let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;
    let receiver = mdns.browse(SERVICE_TYPE).context("failed to browse mDNS")?;

    let mut peers = Vec::new();
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                debug!("resolved: {} addrs={:?}", info.get_fullname(), info.get_addresses());

                let hostname = info
                    .get_property_val_str("hostname")
                    .unwrap_or_else(|| info.get_fullname())
                    .to_string();

                let ssh_port: u16 = info
                    .get_property_val_str("ssh_port")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(22);

                let ssh_user = info
                    .get_property_val_str("ssh_user")
                    .unwrap_or("root")
                    .to_string();

                if let Some(&ip) = info.get_addresses().iter().next() {
                    peers.push(Peer {
                        hostname,
                        ip,
                        ssh_port,
                        ssh_user,
                    });
                } else {
                    eprintln!("warning: peer {hostname} resolved but has no addresses, skipping");
                }
            }
            Ok(event) => {
                debug!("mDNS event: {:?}", event);
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Deduplicate by IP
    peers.sort_by_key(|p| p.ip.to_string());
    peers.dedup_by_key(|p| p.ip.to_string());

    let _ = mdns.stop_browse(SERVICE_TYPE);
    let _ = mdns.shutdown();

    Ok(peers)
}
