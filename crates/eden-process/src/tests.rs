//! Cleanup-domain behavior tests.
//!
//! These cover the exit-status contract documented on [`crate::Tree`]: cleanup
//! on the completion path must never rewrite the leader's own terminal status,
//! and a member that exits while cleanup enumerates it must not turn into a
//! cleanup failure.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

/// Live pids whose pgid equals `group`, read straight from `/proc` so the test
/// observes group membership independently of the implementation.
fn group_members(group: i32) -> Vec<i32> {
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
        if unsafe { libc::getpgid(pid) } == group {
            members.push(pid);
        }
    }
    members
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
    let _ = child.stdout.take();
    let _ = child.stderr.take();
    (child, tree, group)
}

/// The completion-path action must not change the leader's own exit status.
///
/// A live descendant keeps the group alive after the leader is reaped, which is
/// exactly the state in which the old group-wide signal ran. The leader's own
/// reported status must survive it and must survive the settle barrier.
#[tokio::test]
async fn descendant_cleanup_preserves_the_leaders_own_exit_status() {
    let (mut child, tree, group) = spawn("sleep 300 & exit 7").await;
    let status = child.wait().await.expect("wait for shell");
    assert_eq!(
        status.code(),
        Some(7),
        "leader did not report its exit code"
    );
    let stop = crate::Stop::complete(&status);
    assert_eq!(stop.exit_code, Some(7));
    assert_eq!(stop.termination, None);
    // The descendant usually still holds the group once the leader is reaped,
    // so cleanup has real work to do. `sleep` may also be scheduled after the
    // leader exited, which is the same race the group tolerates.
    let before = group_members(group);
    tree.cleanup_descendants().expect("cleanup descendants");
    tree.settle().await.expect("settle");
    assert!(
        group_members(group).is_empty(),
        "descendants survived the barrier (before={before:?}): {:?}",
        group_members(group)
    );
    // Re-reading the same status must still report the leader's own code.
    assert_eq!(status.code(), Some(7));
}

/// The completion-path action leaves the leader alone, so repeating it is safe
/// and never converts a completed command into a signalled one.
#[tokio::test]
async fn descendant_cleanup_is_repeatable_and_leader_safe() {
    let (mut child, tree, group) = spawn("sleep 300 & exit 0").await;
    let status = child.wait().await.expect("wait for shell");
    assert_eq!(status.code(), Some(0));
    for _ in 0..3 {
        tree.cleanup_descendants().expect("cleanup descendants");
    }
    tree.settle().await.expect("settle");
    assert_eq!(status.code(), Some(0));
    assert!(
        group_members(group).is_empty(),
        "descendants survived: {:?}",
        group_members(group)
    );
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
        let status = child.wait().await.expect("wait for shell");
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
    let status = child.wait().await.expect("wait for stopped shell");
    let stop = crate::Stop::stopped();
    assert_eq!(stop.exit_code, None, "a requested stop claims no exit code");
    assert_eq!(stop.termination, Some(crate::Termination::Requested));
    // The leader really was stopped rather than having exited on its own.
    assert_eq!(status.code(), None);
    tree.settle().await.expect("settle");
    assert!(
        group_members(group).is_empty(),
        "group survived termination: {:?}",
        group_members(group)
    );
}

/// A hard group stop is idempotent, so a repeated call cannot signal a recycled
/// group id belonging to an unrelated process.
#[tokio::test]
async fn group_termination_is_idempotent() {
    let (mut child, tree, group) = spawn("sleep 300 & wait").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tree.terminate_group().expect("first terminate");
    tree.terminate_group().expect("second terminate");
    let _ = child.wait().await.expect("wait for stopped shell");
    tree.settle().await.expect("settle");
    assert!(group_members(group).is_empty());
}

/// Enumeration must report survivors rather than failing when a member exits
/// between the snapshot and the signal.
#[tokio::test]
async fn cleanup_tolerates_a_group_that_is_already_empty() {
    let (mut child, tree, group) = spawn("exit 0").await;
    let status = child.wait().await.expect("wait for shell");
    assert_eq!(status.code(), Some(0));
    tree.cleanup_descendants().expect("cleanup empty group");
    tree.settle().await.expect("settle empty group");
    assert!(group_members(group).is_empty());
}
