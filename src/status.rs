use crate::discovery;
use anyhow::Result;
use std::time::Duration;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

pub fn show_status() -> Result<()> {
    println!("Searching for partest peers (waiting {DISCOVERY_TIMEOUT:?})...\n");

    let peers = discovery::discover_peers(DISCOVERY_TIMEOUT)?;

    if peers.is_empty() {
        println!("No peers found. Make sure `partest daemon` is running on other machines.");
        return Ok(());
    }

    println!("{:<20} {:<20} {:<10} {:<6}", "HOSTNAME", "IP", "SSH USER", "SSH PORT");
    println!("{}", "─".repeat(58));

    for peer in &peers {
        println!("{:<20} {:<20} {:<10} {:<6}", peer.hostname, peer.ip, peer.ssh_user, peer.ssh_port);
    }

    println!("\n{} peer(s) found.", peers.len());

    Ok(())
}
