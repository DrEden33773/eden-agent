//! Cleanup-domain behavior tests.
//!
//! These cover the exit-status contract documented on [`crate::Tree`]: cleanup
//! on the completion path must never rewrite the leader's own terminal status,
//! and a member that exits while cleanup enumerates it must not turn into a
//! cleanup failure.
#![cfg(unix)]

use std::{path::Path, time::Duration};
use tokio::io::AsyncReadExt;

/// Pids whose pgid equals `group` and that can still run, read straight from
/// `/proc` so the test observes liveness independently of the implementation.
///
/// A process that was stopped but not yet reaped stays a group member in state
/// `Z`. It can no longer execute or hold a resource, so counting it would fail
/// the barrier after a correct cleanup. macOS has no `/proc`, so this probe is
/// Linux-only and the barriers below skip it elsewhere.
#[cfg(target_os = "linux")]
fn live_group_members(group: i32) -> Vec<i32> {
    let mut members = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return members;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // SAFETY: getpgid only queries a numeric process id.
        if unsafe { libc::getpgid(pid) } != group {
            continue;
        }
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .and_then(|state| state.chars().next())
            });
        if state != Some('Z') {
            members.push(pid);
        }
    }
    members
}

/// Barrier assertion helper: on Linux the group must be empty of live members.
#[cfg(target_os = "linux")]
fn assert_group_stopped(group: i32, context: &str) {
    let remaining = live_group_members(group);
    assert!(remaining.is_empty(), "{context}: {remaining:?}");
}

/// Everywhere else there is no `/proc`, so the barrier is checked by the
/// absence of a hang rather than by re-reading group membership.
#[cfg(not(target_os = "linux"))]
fn assert_group_stopped(_group: i32, _context: &str) {}

/// Every barrier in this module must fail rather than hang, so a lost cleanup
/// step is reported as a test failure instead of a stalled run.
async fn within<F: std::future::Future>(label: &str, future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .unwrap_or_else(|_| panic!("{label} did not finish within the deadline"))
}

/// Reap the leader, failing rather than hanging if it never exits.
///
/// A blocked leader would otherwise stall the whole run, so every test reaps
/// through this helper and turns a lost stop into a normal assertion failure.
async fn wait_for_shell(
    child: &mut tokio::process::Child,
    label: &str,
) -> std::process::ExitStatus {
    tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap_or_else(|_| panic!("{label}: shell did not exit within the deadline"))
        .expect("wait for shell")
}

async fn spawn(command: &str) -> (tokio::process::Child, crate::Tree, i32) {
    let (mut child, tree) = crate::spawn(
        "bash",
        &["--noprofile", "--norc", "-c", command],
        Path::new("/"),
    )
    .await
    .expect("spawn shell");
    let group = child.id().expect("shell pid") as i32;
    // The product drains both streams. Keep that reader alive here as well: a
    // shell diagnostic about a signalled descendant must not kill the leader
    // itself with SIGPIPE and turn a cleanup assertion into a stream artifact.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain(stderr));
    }
    (child, tree, group)
}

/// Read a shell stream to its end so its writer never lacks a reader.
async fn drain(mut stream: impl tokio::io::AsyncRead + Unpin) {
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink).await;
}

/// A probe process that joined the stopped group, killed when the test ends.
///
/// The joiner outlives the group on purpose, so every exit path, including a
/// panic, must reap it instead of leaving a process behind for its lifetime.
struct Joiner(std::process::Child);
impl Joiner {
    fn spawn(group: i32) -> Self {
        use std::os::unix::process::CommandExt;
        Self(
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(group)
                .spawn()
                .expect("a process can join the group of an unreaped leader"),
        )
    }
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }
}
impl Drop for Joiner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A file marker removed when the test ends either way.
///
/// The shell creates one marker after it really forked its descendant and then
/// blocks on another until the test releases it. The leader therefore stays
/// alive, with a live descendant, while the completion-path cleanup below runs:
/// a cleanup that signalled the leader instead of only its descendants would
/// kill it and the shell would report no exit code at all.
struct Marker(std::path::PathBuf);
impl Marker {
    fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "eden-process-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }
    /// Shell fragment that creates this marker.
    fn create(&self) -> String {
        format!("echo ready > '{}'", self.0.display())
    }
    /// Shell fragment that blocks until [`Marker::open`] runs.
    fn block(&self) -> String {
        format!("while [ ! -e '{}' ]; do sleep 0.05; done", self.0.display())
    }
    fn open(&self) {
        std::fs::write(&self.0, b"released\n").expect("release the leader");
    }
    /// Wait until the shell reports that its descendant exists.
    async fn created(&self) {
        for _ in 0..300 {
            if self.0.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} was not created within the deadline", self.0.display());
    }
}
impl Drop for Marker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The completion-path action must not change the leader's own exit status.
///
/// A live descendant keeps the group alive while the leader is still running,
/// which is exactly the state in which the old group-wide signal ran. The
/// leader's own reported status must survive cleanup, because `status.code()`
/// cannot change after the fact: only killing the leader would remove the code.
#[tokio::test]
async fn descendant_cleanup_preserves_the_leaders_own_exit_status() {
    let ready = Marker::new("leader-exit-status");
    let release = Marker::new("leader-exit-status-release");
    let command = format!(
        "sleep 300 & {}; {}; exit 7",
        ready.create(),
        release.block()
    );
    let (mut child, tree, group) = spawn(&command).await;
    ready.created().await;
    tree.cleanup_descendants()
        .expect("cleanup descendants while the leader is alive");
    release.open();
    let status = wait_for_shell(&mut child, "leader").await;
    assert_eq!(
        status.code(),
        Some(7),
        "completion cleanup signalled the leader instead of its descendants only"
    );
    let stop = crate::Stop::complete(&status);
    assert_eq!(stop.exit_code, Some(7));
    assert_eq!(stop.termination, None);
    within("settle", tree.settle()).await.expect("settle");
    assert_group_stopped(group, "descendants survived the barrier");
}

/// The completion-path action leaves the leader alone, so repeating it is safe
/// and never converts a completed command into a signalled one.
#[tokio::test]
async fn descendant_cleanup_is_repeatable_and_leader_safe() {
    let ready = Marker::new("repeatable-cleanup");
    let release = Marker::new("repeatable-cleanup-release");
    let command = format!(
        "sleep 300 & {}; {}; exit 0",
        ready.create(),
        release.block()
    );
    let (mut child, tree, group) = spawn(&command).await;
    ready.created().await;
    for round in 0..3 {
        tree.cleanup_descendants()
            .unwrap_or_else(|error| panic!("round {round} cleanup: {}", error.message));
    }
    release.open();
    let status = wait_for_shell(&mut child, "leader").await;
    assert_eq!(
        status.code(),
        Some(0),
        "repeated completion cleanup signalled the leader instead of its descendants only"
    );
    within("settle", tree.settle()).await.expect("settle");
    assert_group_stopped(group, "descendants survived cleanup");
}

/// A member that exits while cleanup enumerates it is completion, not failure.
///
/// The member's lifetime is short enough that the snapshot races its exit, so
/// this exercises the path that previously reported a vanished pid as a cleanup
/// failure and could abandon the settle barrier.
#[tokio::test]
async fn members_exiting_during_enumeration_are_not_cleanup_failures() {
    let rounds = 400;
    let mut failures = Vec::new();
    for round in 0..rounds {
        let (mut child, tree, _group) = spawn("sleep 0.001 & exit 0").await;
        let status = wait_for_shell(&mut child, "leader").await;
        assert_eq!(status.code(), Some(0), "round {round}");
        if let Err(error) = tree.cleanup_descendants() {
            failures.push(format!("round {round} cleanup: {}", error.message));
        }
        if let Err(error) = tree.settle().await {
            failures.push(format!("round {round} settle: {}", error.message));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {rounds} rounds reported cleanup failures; first: {:?}",
        failures.len(),
        failures.first()
    );
}

/// The cancellation-path action stops the whole domain, leader included, and
/// the caller reports that decision explicitly instead of a missing exit code.
#[tokio::test]
async fn group_termination_stops_the_leader_and_descendants() {
    let (mut child, tree, group) = spawn("sleep 300 & wait").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tree.terminate_group().expect("terminate group");
    let status = wait_for_shell(&mut child, "stopped leader").await;
    let stop = crate::Stop::stopped(&status);
    assert_eq!(stop.termination, Some(crate::Termination::Requested));
    // The leader really was stopped by us rather than exiting on its own.
    assert_eq!(status.code(), None);
    assert_eq!(stop.exit_code, None, "our own stop claims no exit code");
    within("settle", tree.settle()).await.expect("settle");
    assert_group_stopped(group, "group survived termination");
}

/// A hard group stop is idempotent, so a repeated call cannot signal a recycled
/// group id belonging to an unrelated process.
///
/// After the first stop the unreaped leader keeps this group id alive, so a new
/// unrelated process can really join it. That member is what a second, unguarded
/// group signal would kill; only the recorded stop keeps it untouched.
#[tokio::test]
async fn group_termination_is_idempotent() {
    let (mut child, tree, group) = spawn("sleep 300 & wait").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tree.terminate_group().expect("first terminate");
    let mut joiner = Joiner::spawn(group);
    // SAFETY: getpgid only queries a numeric process id.
    let joiner_group = unsafe { libc::getpgid(joiner.id() as i32) };
    assert_eq!(
        joiner_group, group,
        "the probe process must really be a member of the stopped group"
    );
    tree.terminate_group().expect("second terminate");
    // Poll instead of sleeping once: a leaked signal would kill the probe
    // immediately, and a probe that survives the whole window is the evidence.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(status) = joiner.try_wait().expect("probe process") {
            panic!(
                "repeated group stop signalled a process that joined after the first stop: {status}"
            );
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = wait_for_shell(&mut child, "stopped leader").await;
    within("settle", tree.settle()).await.expect("settle");
    assert_group_stopped(group, "group survived repeated termination");
}

/// Enumeration must report survivors rather than failing when a member exits
/// between the snapshot and the signal.
#[tokio::test]
async fn cleanup_tolerates_a_group_that_is_already_empty() {
    let (mut child, tree, group) = spawn("exit 0").await;
    let status = wait_for_shell(&mut child, "leader").await;
    assert_eq!(status.code(), Some(0));
    tree.cleanup_descendants().expect("cleanup empty group");
    within("settle", tree.settle())
        .await
        .expect("settle empty group");
    assert_group_stopped(group, "empty group did not stay empty");
}

/// Descendants created while cleanup is running must still be stopped.
///
/// The background process keeps starting long-lived children while the barrier
/// runs, so at least one of them is always created after the pass that would
/// otherwise have signalled it. Without re-signalling on each pass, the barrier
/// waits out those children's whole lifetime; the deadline is what turns that
/// lost cleanup step into a failure instead of a hang.
#[tokio::test]
async fn descendants_created_after_the_snapshot_are_still_stopped() {
    let (mut child, tree, group) = spawn("while :; do sleep 300 & done & exit 0").await;
    let status = wait_for_shell(&mut child, "leader").await;
    assert_eq!(status.code(), Some(0));
    tree.cleanup_descendants().expect("cleanup descendants");
    within("settle", tree.settle()).await.expect("settle");
    assert_group_stopped(
        group,
        "a descendant created during cleanup outlived the barrier",
    );
}
