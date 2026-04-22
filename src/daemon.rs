use anyhow::{Context, Result};
use std::time::Duration;

const SERVICE_TYPE: &str = "_partest._tcp.local.";

enum MdnsRegistration {
    /// On macOS, we delegate to the system's mDNSResponder via the `dns-sd` command.
    /// The mdns-sd crate's userspace multicast is shadowed by mDNSResponder on port 5353.
    #[cfg(target_os = "macos")]
    Native(std::process::Child),
    /// On other platforms, use the mdns-sd crate directly.
    #[cfg(not(target_os = "macos"))]
    Crate(mdns_sd::ServiceDaemon),
}

/// Run the partest daemon: register this machine on the local network via mDNS
/// and keep running until interrupted.
pub async fn run_daemon(ssh_port: u16) -> Result<()> {
    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .to_string();

    let ssh_user = std::env::var("USER").unwrap_or_else(|_| "root".into());

    let instance_name = format!("partest-{hostname}");

    // Wait for network to become available (important when started at boot via launchd/systemd)
    let my_addr = wait_for_network(Duration::from_secs(60)).await?;

    let properties = [
        ("hostname", hostname.as_str()),
        ("ssh_port", &ssh_port.to_string()),
        ("ssh_user", ssh_user.as_str()),
    ];

    let registration = register_mdns(&instance_name, &hostname, ssh_port, my_addr, &properties)?;

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
    match registration {
        #[cfg(target_os = "macos")]
        MdnsRegistration::Native(mut child) => { let _ = child.kill(); }
        #[cfg(not(target_os = "macos"))]
        MdnsRegistration::Crate(mdns) => { let _ = mdns.shutdown(); }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn register_mdns(
    instance_name: &str,
    _hostname: &str,
    ssh_port: u16,
    _addr: std::net::IpAddr,
    properties: &[(&str, &str)],
) -> Result<MdnsRegistration> {
    // Use macOS's native Bonjour via `dns-sd -R` which registers through mDNSResponder.
    let txt_records: Vec<String> = properties
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let mut args = vec![
        "-R".to_string(),
        instance_name.to_string(),
        "_partest._tcp".to_string(),
        "local".to_string(),
        ssh_port.to_string(),
    ];
    args.extend(txt_records);

    let child = std::process::Command::new("dns-sd")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn dns-sd for mDNS registration")?;

    Ok(MdnsRegistration::Native(child))
}

#[cfg(not(target_os = "macos"))]
fn register_mdns(
    instance_name: &str,
    hostname: &str,
    ssh_port: u16,
    addr: std::net::IpAddr,
    properties: &[(&str, &str)],
) -> Result<MdnsRegistration> {
    use mdns_sd::{ServiceDaemon, ServiceInfo};

    let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        instance_name,
        &format!("{hostname}.local."),
        addr,
        ssh_port,
        properties,
    )
    .context("failed to create service info")?;

    mdns.register(service)
        .context("failed to register mDNS service")?;

    Ok(MdnsRegistration::Crate(mdns))
}

/// Wait for a usable network interface, retrying every 2 seconds up to `timeout`.
async fn wait_for_network(timeout: Duration) -> Result<std::net::IpAddr> {
    let start = std::time::Instant::now();
    loop {
        match local_ipv4_addr() {
            Ok(addr) => return Ok(addr),
            Err(e) => {
                if start.elapsed() >= timeout {
                    return Err(e).context("network not available after waiting");
                }
                eprintln!("waiting for network... ({e:#})");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Get the primary non-loopback IPv4 address of this machine.
fn local_ipv4_addr() -> Result<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .context("failed to bind UDP socket")?;
    socket.connect("8.8.8.8:80")
        .context("failed to determine local IP (no network?)")?;
    Ok(socket.local_addr()?.ip())
}
