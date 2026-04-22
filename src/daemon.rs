use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

const SERVICE_TYPE: &str = "_partest._tcp.local.";

/// Run the partest daemon: register this machine on the local network via mDNS
/// and keep running until interrupted.
pub async fn run_daemon(ssh_port: u16) -> Result<()> {
    let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;

    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .to_string();

    let ssh_user = std::env::var("USER").unwrap_or_else(|_| "root".into());

    let instance_name = format!("partest-{hostname}");

    let properties = [
        ("hostname", hostname.as_str()),
        ("ssh_port", &ssh_port.to_string()),
        ("ssh_user", ssh_user.as_str()),
    ];

    let my_addr = local_ipv4_addr()?;

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &format!("{hostname}.local."),
        my_addr,
        ssh_port,
        &properties[..],
    )
    .context("failed to create service info")?;

    mdns.register(service)
        .context("failed to register mDNS service")?;

    eprintln!("partest daemon started");
    eprintln!("  hostname:  {hostname}");
    eprintln!("  address:   {my_addr}");
    eprintln!("  ssh user:  {ssh_user}");
    eprintln!("  ssh port:  {ssh_port}");
    eprintln!("  service:   {SERVICE_TYPE}");
    eprintln!();
    eprintln!("Press Ctrl+C to stop");

    // Wait until interrupted
    tokio::signal::ctrl_c().await?;

    eprintln!("Shutting down...");
    let _ = mdns.shutdown();

    Ok(())
}

/// Get the primary non-loopback IPv4 address of this machine.
fn local_ipv4_addr() -> Result<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .context("failed to bind UDP socket")?;
    socket.connect("8.8.8.8:80")
        .context("failed to determine local IP (no network?)")?;
    Ok(socket.local_addr()?.ip())
}
