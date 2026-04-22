use crate::discovery::{self, Peer};
use crate::ssh::Session;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const REMOTE_WORK_DIR: &str = "~/.partest-work";

/// Run the full distributed test pipeline.
pub async fn run(ssh_user: &str, ssh_key: &str, release: bool, nextest_args: &[String]) -> Result<()> {
    let ssh_key_path = PathBuf::from(shellexpand::tilde(ssh_key).as_ref());

    // 1. Discover peers
    info!("Discovering peers on the local network...");
    let mut peers = discovery::discover_peers(DISCOVERY_TIMEOUT)?;

    if peers.is_empty() {
        anyhow::bail!(
            "No partest peers found on the local network.\n\
             Make sure `partest daemon` is running on at least one machine."
        );
    }

    // Sort deterministically by IP for stable partition assignment
    peers.sort_by_key(|p| p.ip.to_string());
    let n = peers.len();
    info!("Found {n} peer(s): {}", peers.iter().map(|p| p.hostname.as_str()).collect::<Vec<_>>().join(", "));

    // 2. Build nextest archive + source tarball
    info!("Building nextest archive...");
    let archive_path = build_archive(release).await?;
    info!("Archive created at {}", archive_path.display());

    info!("Creating source tarball...");
    let source_tar_path = create_source_tarball().await?;
    info!("Source tarball created at {}", source_tar_path.display());

    // 3. Connect to all peers and distribute
    info!("Distributing to {n} peer(s)...");
    let sessions = connect_all(&peers, ssh_user, &ssh_key_path).await?;

    distribute_all(&sessions, &peers, &archive_path, &source_tar_path).await?;
    info!("Files distributed to all peers");

    // 4. Execute tests on all peers in parallel, streaming output
    info!("Running tests across {n} partition(s)...\n");
    let start = Instant::now();

    let extra_args = if nextest_args.is_empty() {
        String::new()
    } else {
        format!(" {}", nextest_args.join(" "))
    };

    let mut handles = Vec::new();
    for (i, (session, peer)) in sessions.into_iter().zip(peers.iter()).enumerate() {
        let partition_id = i + 1;
        let hostname = peer.hostname.clone();
        let cmd = format!(
            "cd {REMOTE_WORK_DIR} && cargo nextest run --archive-file archive.tar.zst \
             --workspace-remap {REMOTE_WORK_DIR}/src --partition hash:{partition_id}/{n}{extra_args}"
        );

        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let exit_code = session
                .exec_stream(&cmd, |line| {
                    println!("[{hostname}] {line}");
                })
                .await;
            let elapsed = start.elapsed();
            (hostname, exit_code, elapsed)
        }));
    }

    // 5. Collect results
    let mut any_failed = false;
    println!();
    println!("─── Summary ───");

    for handle in handles {
        let (hostname, result, elapsed) = handle.await?;
        match result {
            Ok(0) => {
                println!("  ✓ {hostname}: all tests passed ({:.1}s)", elapsed.as_secs_f64());
            }
            Ok(code) => {
                println!("  ✗ {hostname}: tests failed (exit code {code}, {:.1}s)", elapsed.as_secs_f64());
                any_failed = true;
            }
            Err(e) => {
                println!("  ✗ {hostname}: error — {e}");
                any_failed = true;
            }
        }
    }

    let total_elapsed = start.elapsed();
    println!();
    println!(
        "Total wall time: {:.1}s across {n} machine(s)",
        total_elapsed.as_secs_f64()
    );

    if any_failed {
        anyhow::bail!("Some partitions had test failures");
    }

    // Clean up local temp files
    let _ = tokio::fs::remove_file(&archive_path).await;
    let _ = tokio::fs::remove_file(&source_tar_path).await;

    Ok(())
}

/// Build a nextest archive in a temp directory.
async fn build_archive(release: bool) -> Result<PathBuf> {
    let archive_path = std::env::temp_dir().join("partest-archive.tar.zst");
    let archive_str = archive_path.to_string_lossy().to_string();

    let mut args = vec!["nextest", "archive", "--archive-file", &archive_str];
    if release {
        args.push("--cargo-profile");
        args.push("release");
    }

    let output = tokio::process::Command::new("cargo")
        .args(&args)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output()
        .await
        .context("failed to run `cargo nextest archive`")?;

    if !output.status.success() {
        anyhow::bail!(
            "`cargo nextest archive` failed with status {}",
            output.status
        );
    }

    Ok(archive_path)
}

/// Connect to all peers via SSH in parallel.
async fn connect_all(
    peers: &[Peer],
    user: &str,
    key_path: &Path,
) -> Result<Vec<Session>> {
    let mut handles = Vec::new();

    for peer in peers {
        let ip = peer.ip;
        let port = peer.ssh_port;
        let user = user.to_string();
        let key_path = key_path.to_path_buf();

        handles.push(tokio::spawn(async move {
            Session::connect(ip, port, &user, &key_path).await
        }));
    }

    let mut sessions = Vec::new();
    for (handle, peer) in handles.into_iter().zip(peers) {
        let session = handle
            .await?
            .with_context(|| format!("failed to connect to {peer}"))?;
        sessions.push(session);
    }

    Ok(sessions)
}

/// Create a tarball of the project source (excluding target/).
async fn create_source_tarball() -> Result<PathBuf> {
    let tar_path = std::env::temp_dir().join("partest-source.tar.gz");
    let tar_str = tar_path.to_string_lossy().to_string();

    let output = tokio::process::Command::new("tar")
        .args([
            "czf", &tar_str,
            "--exclude", "./target",
            "--exclude", "./.git",
            ".",
        ])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output()
        .await
        .context("failed to create source tarball")?;

    if !output.status.success() {
        anyhow::bail!("tar failed with status {}", output.status);
    }

    Ok(tar_path)
}

/// Upload the archive and source to all peers, then extract source.
async fn distribute_all(
    sessions: &[Session],
    peers: &[Peer],
    archive_path: &Path,
    source_tar_path: &Path,
) -> Result<()> {
    for (session, peer) in sessions.iter().zip(peers) {
        let name = &peer.hostname;

        // Upload nextest archive
        session
            .upload(archive_path, &format!("{REMOTE_WORK_DIR}/archive.tar.zst"))
            .await
            .with_context(|| format!("failed to upload archive to {name}"))?;

        // Upload source tarball
        session
            .upload(source_tar_path, &format!("{REMOTE_WORK_DIR}/source.tar.gz"))
            .await
            .with_context(|| format!("failed to upload source to {name}"))?;

        // Extract source on remote
        session
            .exec_ignore(&format!(
                "mkdir -p {REMOTE_WORK_DIR}/src && \
                 cd {REMOTE_WORK_DIR}/src && \
                 tar xzf {REMOTE_WORK_DIR}/source.tar.gz"
            ))
            .await
            .with_context(|| format!("failed to extract source on {name}"))?;
    }
    Ok(())
}
