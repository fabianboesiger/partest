use crate::discovery::{self, Peer};
use crate::ssh::Session;
use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::info;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const REMOTE_WORK_DIR: &str = "~/.partest-work";

// ── Data structures ──────────────────────────────────────────────────

/// Shared test pool for adaptive work-stealing dispatch.
struct TestPool {
    tests: Mutex<VecDeque<String>>,
    max_batch_size: usize,
    active_workers: AtomicUsize,
    batches_dispatched: AtomicUsize,
}

impl TestPool {
    fn new(tests: Vec<String>, max_batch_size: usize, num_workers: usize) -> Self {
        Self {
            tests: Mutex::new(VecDeque::from(tests)),
            max_batch_size,
            active_workers: AtomicUsize::new(num_workers),
            batches_dispatched: AtomicUsize::new(0),
        }
    }

    /// Steal a batch of tests. The batch size adapts to distribute remaining
    /// work evenly across active workers, ensuring no single worker hogs a
    /// large chunk while others sit idle.
    fn steal(&self) -> Option<(usize, Vec<String>)> {
        let mut tests = self.tests.lock().unwrap();
        if tests.is_empty() {
            return None;
        }

        let remaining = tests.len();
        let active = self.active_workers.load(Ordering::Relaxed).max(1);

        // Adaptive sizing: split remaining work evenly across active workers,
        // but never exceed max_batch_size and never go below 1.
        let chunk = (remaining / active).max(1).min(self.max_batch_size);

        let batch: Vec<String> = tests.drain(..chunk.min(remaining)).collect();
        let batch_idx = self.batches_dispatched.fetch_add(1, Ordering::Relaxed) + 1;
        Some((batch_idx, batch))
    }

    fn remaining(&self) -> usize {
        self.tests.lock().unwrap().len()
    }

    fn mark_worker_done(&self) {
        self.active_workers.fetch_sub(1, Ordering::Relaxed);
    }
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
    for (_binary, suite) in &parsed.rust_suites {
        for test_name in suite.testcases.keys() {
            tests.push(test_name.clone());
        }
    }
    tests.sort();
    Ok(tests)
}

/// Build a nextest `-E` filter expression for an exact set of tests.
fn build_filter_expr(tests: &[String]) -> String {
    tests
        .iter()
        .map(|t| format!("test(={t})"))
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── Main entry point ─────────────────────────────────────────────────

/// Run the full distributed test pipeline.
pub async fn run(
    ssh_key: &str,
    release: bool,
    batch_size: usize,
    jobs_per_worker: usize,
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
    info!("Found {total_tests} test(s), max batch size {batch_size}");

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
    let sessions: Vec<Arc<Session>> = sessions.into_iter().map(Arc::new).collect();
    distribute_all(&sessions, &peers, &source_tar_path).await?;
    info!("Files distributed to all peers");

    // 5. Set up MultiProgress TUI
    let multi = MultiProgress::new();

    // Total progress bar at the top
    let total_bar = multi.add(ProgressBar::new(total_tests as u64));
    total_bar.set_style(
        ProgressStyle::with_template(
            "{bar:40.green/dim} {pos}/{len} tests ({percent}%) — {elapsed_precise} elapsed"
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
    );

    let spinner_style = ProgressStyle::with_template("{spinner:.cyan} {wide_msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let bars: Vec<ProgressBar> = peers
        .iter()
        .map(|peer| {
            let pb = multi.insert_after(&total_bar, ProgressBar::new_spinner());
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

    // 7. Adaptive work-stealing dispatch with concurrent slots per worker
    let pool = Arc::new(TestPool::new(tests, batch_size, n * jobs_per_worker));
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
        let pool = Arc::clone(&pool);
        let total_bar = total_bar.clone();
        let extra_args = extra_args.clone();
        let nextest_profile = nextest_profile.to_string();
        let session = Arc::clone(&sessions[i]);
        let semaphore = Arc::new(Semaphore::new(jobs_per_worker));

        pb.set_message(format!("{hostname}  — ○ idle, waiting for work"));

        dispatch_handles.push(async move {
            let results: Arc<Mutex<Vec<BatchResult>>> = Arc::new(Mutex::new(Vec::new()));
            let tests_run = Arc::new(AtomicUsize::new(0));
            let batches_run = Arc::new(AtomicUsize::new(0));
            let active_slots = Arc::new(AtomicUsize::new(0));
            let mut slot_handles = Vec::new();

            loop {
                // Steal next batch before acquiring a permit so we can exit early
                let Some((batch_idx, batch_tests)) = pool.steal() else {
                    break;
                };

                let batch_len = batch_tests.len();

                // Wait for a free slot on this worker
                let permit = semaphore.clone().acquire_owned().await.unwrap();

                let session = Arc::clone(&session);
                let pool_ref = Arc::clone(&pool);
                let results = Arc::clone(&results);
                let total_bar = total_bar.clone();
                let tests_run = Arc::clone(&tests_run);
                let batches_run = Arc::clone(&batches_run);
                let active_slots = Arc::clone(&active_slots);
                let pb = pb.clone();
                let hostname = hostname.clone();
                let nextest_profile = nextest_profile.clone();
                let extra_args = extra_args.clone();

                active_slots.fetch_add(1, Ordering::Relaxed);
                let cur_active = active_slots.load(Ordering::Relaxed);
                let remaining = pool_ref.remaining();
                pb.set_message(format!(
                    "{hostname}  — ⚙ {cur_active} slot(s) active, batch #{batch_idx} ({batch_len} tests) [pool: {remaining}]",
                ));

                slot_handles.push(tokio::spawn(async move {
                    let filter_expr = build_filter_expr(&batch_tests);
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
                    let prev_tests = tests_run.fetch_add(batch_len, Ordering::Relaxed);
                    let _prev_batches = batches_run.fetch_add(1, Ordering::Relaxed);
                    total_bar.inc(batch_len as u64);
                    active_slots.fetch_sub(1, Ordering::Relaxed);

                    results.lock().unwrap().push(BatchResult {
                        batch_index: batch_idx,
                        exit_code,
                        elapsed,
                        output: output_buf,
                    });

                    let cur_active = active_slots.load(Ordering::Relaxed);
                    let remaining = pool_ref.remaining();
                    if cur_active > 0 {
                        pb.set_message(format!(
                            "{hostname}  — ⚙ {cur_active} slot(s) active, {} tests done [pool: {remaining}]",
                            prev_tests + batch_len,
                        ));
                    }

                    drop(permit); // release slot
                }));
            }

            // Wait for all in-flight slots to finish
            for handle in slot_handles {
                let _ = handle.await;
            }

            pool.mark_worker_done();
            let total_tests_run = tests_run.load(Ordering::Relaxed);
            let total_batches_run = batches_run.load(Ordering::Relaxed);
            pb.finish_with_message(format!(
                "{hostname}  — ✓ done ({total_tests_run} tests in {total_batches_run} batch{})",
                if total_batches_run == 1 { "" } else { "es" }
            ));

            let final_results = match Arc::try_unwrap(results) {
                Ok(mutex) => mutex.into_inner().unwrap_or_default(),
                Err(_) => panic!("all slot handles should have been joined"),
            };
            (hostname, final_results, total_tests_run)
        });
    }

    let worker_results = futures::future::join_all(dispatch_handles).await;
    let total_elapsed = start.elapsed();
    total_bar.finish();

    // 8. Summary
    println!();
    println!("─── Summary ───");

    let mut any_failed = false;
    let mut failed_outputs: Vec<(String, usize, String)> = Vec::new();

    for (hostname, results, tests_run) in &worker_results {
        let passed = results.iter().filter(|r| matches!(&r.exit_code, Ok(0))).count();
        let failed = results.len() - passed;
        let total_time: Duration = results.iter().map(|r| r.elapsed).sum();

        if failed > 0 {
            any_failed = true;
            println!(
                "  ✗ {hostname}: {}/{} batches passed, {tests_run} tests ({:.1}s)",
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
                "  ✓ {hostname}: {tests_run} tests in {passed} batch{} ({:.1}s)",
                if passed == 1 { "" } else { "es" },
                total_time.as_secs_f64()
            );
        }
    }

    let total_batches: usize = worker_results.iter().map(|(_, r, _)| r.len()).sum();
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
    sessions: &[Arc<Session>],
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
