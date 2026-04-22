use crate::discovery::{self, Peer};
use crate::ssh::Session;
use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const REMOTE_WORK_DIR: &str = "~/.partest-work";

// ── Data structures ──────────────────────────────────────────────────

struct Batch {
    index: usize,
    total: usize,
    tests: Vec<String>,
}

struct BatchResult {
    batch_index: usize,
    exit_code: Result<u32>,
    elapsed: Duration,
    output: String,
}

// ── Nextest JSON list parsing ────────────────────────────────────────

#[derive(Deserialize)]
struct NextestListOutput {
    #[serde(rename = "rust-suites")]
    rust_suites: std::collections::HashMap<String, NextestSuite>,
}

#[derive(Deserialize)]
struct NextestSuite {
    testcases: std::collections::HashMap<String, serde_json::Value>,
}

/// List all tests locally via `cargo nextest list`.
async fn list_tests_locally() -> Result<Vec<String>> {
    let output = tokio::process::Command::new("cargo")
        .args(["nextest", "list", "--message-format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run cargo nextest list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cargo nextest list failed:\n{stderr}");
    }

    let parsed: NextestListOutput =
        serde_json::from_slice(&output.stdout).context("failed to parse nextest list JSON")?;

    let mut tests = Vec::new();
    for (binary, suite) in &parsed.rust_suites {
        for test_name in suite.testcases.keys() {
            tests.push(format!("{binary}::{test_name}"));
        }
    }
    tests.sort();
    Ok(tests)
}

/// Split a flat test list into batches.
fn create_batches(tests: Vec<String>, batch_size: usize) -> VecDeque<Batch> {
    let total_batches = (tests.len() + batch_size - 1) / batch_size;
    tests
        .chunks(batch_size)
        .enumerate()
        .map(|(i, chunk)| Batch {
            index: i + 1,
            total: total_batches,
            tests: chunk.to_vec(),
        })
        .collect()
}

/// Build a nextest `-E` filter expression for an exact set of tests.
fn build_filter_expr(tests: &[String]) -> String {
    tests
        .iter()
        .map(|t| {
            // Extract just the test name (after ::) for the filter
            let name = t.rsplit("::").next().unwrap_or(t);
            format!("test(={name})")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── Main entry point ─────────────────────────────────────────────────

/// Run the full distributed test pipeline.
pub async fn run(
    ssh_key: &str,
    release: bool,
    batch_size: usize,
    nextest_args: &[String],
) -> Result<()> {
    let ssh_key_path = PathBuf::from(shellexpand::tilde(ssh_key).as_ref());

    // 1. List tests locally
    info!("Enumerating tests locally...");
    let tests = list_tests_locally().await?;
    if tests.is_empty() {
        anyhow::bail!("No tests found by `cargo nextest list`");
    }
    let total_tests = tests.len();
    let batches = create_batches(tests, batch_size);
    let total_batches = batches.len();
    info!("Found {total_tests} test(s), split into {total_batches} batch(es) of up to {batch_size}");

    // 2. Discover peers
    info!("Discovering peers on the local network...");
    let mut peers = discovery::discover_peers(DISCOVERY_TIMEOUT)?;

    if peers.is_empty() {
        anyhow::bail!(
            "No partest peers found on the local network.\n\
             Make sure `partest daemon` is running on at least one machine."
        );
    }

    peers.sort_by_key(|p| p.ip.to_string());
    let n = peers.len();
    info!(
        "Found {n} peer(s): {}",
        peers.iter().map(|p| p.hostname.as_str()).collect::<Vec<_>>().join(", ")
    );

    // 3. Create source tarball
    info!("Creating source tarball...");
    let source_tar_path = create_source_tarball().await?;

    // 4. Connect to all peers and distribute
    info!("Distributing to {n} peer(s)...");
    let sessions = connect_all(&peers, &ssh_key_path).await?;
    distribute_all(&sessions, &peers, &source_tar_path).await?;
    info!("Files distributed to all peers");

    // 5. Set up MultiProgress TUI
    let multi = MultiProgress::new();
    let spinner_style = ProgressStyle::with_template("{spinner:.cyan} {wide_msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let bars: Vec<ProgressBar> = peers
        .iter()
        .map(|peer| {
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(spinner_style.clone());
            pb.set_message(format!("{}  — connecting", peer.hostname));
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        })
        .collect();

    // 6. Pre-build on all workers
    let release_flag = if release { " --release" } else { "" };

    {
        let mut build_handles = Vec::new();
        for (i, (session, peer)) in sessions.iter().zip(peers.iter()).enumerate() {
            let hostname = peer.hostname.clone();
            let pb = bars[i].clone();
            pb.set_message(format!("{hostname}  — ⏳ building tests..."));

            let cmd = format!(
                "cd {REMOTE_WORK_DIR}/src && cargo build --tests{release_flag}"
            );

            // We need to borrow session across an await, so use a reference via index
            let session_ref = session;
            build_handles.push(async move {
                let result = session_ref.exec_stream(&cmd, |_line| {}).await;
                (i, hostname, result)
            });
        }

        let results = futures::future::join_all(build_handles).await;
        for (i, hostname, result) in results {
            match result {
                Ok(0) => {
                    bars[i].set_message(format!("{hostname}  — ✓ build complete"));
                }
                Ok(code) => {
                    bars[i].finish_with_message(format!("{hostname}  — ✗ build failed (exit {code})"));
                    anyhow::bail!("Pre-build failed on {hostname} with exit code {code}");
                }
                Err(e) => {
                    bars[i].finish_with_message(format!("{hostname}  — ✗ build error"));
                    anyhow::bail!("Pre-build failed on {hostname}: {e}");
                }
            }
        }
    }

    // 7. Dynamic batch dispatch
    let queue = Arc::new(Mutex::new(batches));
    let nextest_profile = if release { " --cargo-profile release" } else { "" };
    let extra_args = if nextest_args.is_empty() {
        String::new()
    } else {
        format!(" {}", nextest_args.join(" "))
    };

    let start = Instant::now();
    let mut dispatch_handles = Vec::new();

    for (i, peer) in peers.iter().enumerate() {
        let hostname = peer.hostname.clone();
        let pb = bars[i].clone();
        let queue = Arc::clone(&queue);
        let extra_args = extra_args.clone();
        let nextest_profile = nextest_profile.to_string();
        let session = &sessions[i];

        pb.set_message(format!("{hostname}  — ○ idle, waiting for batch"));

        dispatch_handles.push(async move {
            let mut results: Vec<BatchResult> = Vec::new();
            let mut batches_run = 0usize;

            loop {
                // Pop next batch
                let batch = {
                    let mut q = queue.lock().unwrap();
                    q.pop_front()
                };
                let Some(batch) = batch else {
                    pb.finish_with_message(format!(
                        "{hostname}  — ✓ done (ran {batches_run} batch{})",
                        if batches_run == 1 { "" } else { "es" }
                    ));
                    break;
                };

                let remaining = {
                    let q = queue.lock().unwrap();
                    q.len()
                };

                pb.set_message(format!(
                    "{hostname}  — ⚙ batch {}/{} ({} tests) [queue: {} remaining]",
                    batch.index, batch.total, batch.tests.len(), remaining
                ));

                let filter_expr = build_filter_expr(&batch.tests);
                let cmd = format!(
                    "cd {REMOTE_WORK_DIR}/src && cargo nextest run{nextest_profile} -E '{filter_expr}'{extra_args}"
                );

                let mut output_buf = String::new();
                let batch_start = Instant::now();

                let exit_code = session.exec_stream(&cmd, |line| {
                    output_buf.push_str(line);
                    output_buf.push('\n');
                }).await;

                let elapsed = batch_start.elapsed();
                batches_run += 1;

                results.push(BatchResult {
                    batch_index: batch.index,
                    exit_code,
                    elapsed,
                    output: output_buf,
                });
            }

            (hostname, results)
        });
    }

    let worker_results = futures::future::join_all(dispatch_handles).await;
    let total_elapsed = start.elapsed();

    // 8. Summary
    println!();
    println!("─── Summary ───");

    let mut any_failed = false;
    let mut failed_outputs: Vec<(String, usize, String)> = Vec::new();

    for (hostname, results) in &worker_results {
        let passed = results.iter().filter(|r| matches!(&r.exit_code, Ok(0))).count();
        let failed = results.len() - passed;
        let total_time: Duration = results.iter().map(|r| r.elapsed).sum();

        if failed > 0 {
            any_failed = true;
            println!(
                "  ✗ {hostname}: {}/{} batches passed ({:.1}s)",
                passed,
                results.len(),
                total_time.as_secs_f64()
            );
            for r in results {
                if !matches!(&r.exit_code, Ok(0)) {
                    failed_outputs.push((hostname.clone(), r.batch_index, r.output.clone()));
                }
            }
        } else {
            println!(
                "  ✓ {hostname}: {passed} batch{} passed ({:.1}s)",
                if passed == 1 { "" } else { "es" },
                total_time.as_secs_f64()
            );
        }
    }

    println!();
    println!(
        "Total: {total_tests} tests in {total_batches} batches across {n} worker(s), wall time {:.1}s",
        total_elapsed.as_secs_f64()
    );

    // Print failure details
    if !failed_outputs.is_empty() {
        println!();
        println!("─── Failure Details ───");
        for (hostname, batch_idx, output) in &failed_outputs {
            println!();
            println!("── [{hostname}] batch {batch_idx} ──");
            println!("{output}");
        }
    }

    if any_failed {
        anyhow::bail!("Some batches had test failures");
    }

    // Clean up
    let _ = tokio::fs::remove_file(&source_tar_path).await;

    Ok(())
}

// ── Helpers (unchanged) ──────────────────────────────────────────────

/// Connect to all peers via SSH in parallel.
async fn connect_all(peers: &[Peer], key_path: &Path) -> Result<Vec<Session>> {
    let mut handles = Vec::new();

    for peer in peers {
        let ip = peer.ip;
        let port = peer.ssh_port;
        let user = peer.ssh_user.clone();
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

/// Upload source to all peers and extract it.
async fn distribute_all(
    sessions: &[Session],
    peers: &[Peer],
    source_tar_path: &Path,
) -> Result<()> {
    for (session, peer) in sessions.iter().zip(peers) {
        let name = &peer.hostname;

        session
            .upload(source_tar_path, &format!("{REMOTE_WORK_DIR}/source.tar.gz"))
            .await
            .with_context(|| format!("failed to upload source to {name}"))?;

        session
            .exec_ignore(&format!(
                "mkdir -p {REMOTE_WORK_DIR}/src && \
                 cd {REMOTE_WORK_DIR}/src && \
                 find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {{}} + && \
                 tar xzf {REMOTE_WORK_DIR}/source.tar.gz"
            ))
            .await
            .with_context(|| format!("failed to extract source on {name}"))?;
    }
    Ok(())
}
