//! Windows job-object smoke tests.
//!
//! [`crate::tests`] is Unix-only, so before this module the Windows target
//! compiled zero tests: the job-object ownership in `platform` was exercised
//! only through the installed acceptance suites. These tests assert the same
//! exit-status contract as the Unix suite against a real shell, and they observe
//! descendant liveness through Win32 process handles rather than through the job
//! membership the implementation enumerates, so a cleanup that stopped nothing
//! cannot pass by re-reading its own bookkeeping.
//!
//! Git for Windows provides the shell these tests drive; that is the same
//! prerequisite the Windows native job and the repository hooks already have.
#![cfg(windows)]

use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, Lines};

/// Every barrier fails rather than hangs, so a lost cleanup step is reported as
/// an assertion failure instead of a stalled run.
async fn within<F: std::future::Future>(label: &str, future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .unwrap_or_else(|_| panic!("{label} did not finish within the deadline"))
}

/// Read a shell stream to its end so its writer never lacks a reader.
async fn drain(mut stream: impl AsyncRead + Unpin) {
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink).await;
}

/// Whether a process is still running, read from a Win32 handle rather than from
/// the job membership the implementation queries.
///
/// A process that no longer exists cannot be opened at all, which is a definite
/// answer for this probe.
fn process_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };
    // SAFETY: OpenProcess returns an owned handle for this numeric id, or null.
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    // SAFETY: The handle owns SYNCHRONIZE access, and a zero timeout only
    // reports the current state instead of waiting.
    let state = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: The handle was returned by OpenProcess, is owned here, and is
    // closed exactly once.
    unsafe { CloseHandle(handle) };
    match state {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 => false,
        other => panic!("unexpected wait state {other} while observing a shell descendant"),
    }
}

/// Poll the probe until the process is gone, so a test observes the cleanup
/// result instead of racing process teardown.
async fn wait_until_stopped(pid: u32, label: &str) {
    within(label, async {
        while process_running(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// One spawned shell: the leader, the job that owns it, and the report channel
/// its descendants use to publish their process ids.
struct Shell {
    child: tokio::process::Child,
    tree: crate::Tree,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Shell {
    /// Spawn the native shell with `command` as its script.
    ///
    /// The shell reports each background process id as `descendant=<pid>` on
    /// stdout, which is how these tests learn about members without asking the
    /// job object what it contains.
    async fn spawn(command: &str) -> Self {
        let (mut child, tree) = crate::spawn(
            "bash",
            &["--noprofile", "--norc", "-c", command],
            Path::new("."),
        )
        .await
        .expect("a native Bash must be on PATH for process tests");
        let stdout = child.stdout.take().expect("shell stdout is piped");
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain(stderr));
        }
        Self {
            child,
            tree,
            lines: BufReader::new(stdout).lines(),
        }
    }

    fn leader(&self) -> u32 {
        self.child
            .id()
            .expect("the shell reports its own process id")
    }

    /// Read the next reported descendant id, failing instead of hanging when the
    /// shell stops reporting.
    async fn next_descendant(&mut self, label: &str) -> u32 {
        within(label, async {
            while let Some(line) = self.lines.next_line().await.expect("read shell stdout") {
                if let Some(rest) = line.strip_prefix("descendant=") {
                    return rest.trim().parse::<u32>().expect("a process id is numeric");
                }
            }
            panic!("{label}: the shell closed its output without reporting a descendant");
        })
        .await
    }

    /// Reap the leader, failing rather than hanging if it never exits.
    async fn wait_for_leader(&mut self) -> std::process::ExitStatus {
        within("leader exit", self.child.wait())
            .await
            .expect("wait for the shell")
    }
}

/// The completion-path action stops descendants and leaves the leader's own exit
/// status untouched.
///
/// The leader blocks in `wait` for a background descendant, so the descendant is
/// provably alive and the leader cannot have exited on its own when cleanup
/// runs. A stop that reached the leader would replace the real exit code 7 with
/// the stop's own status, which is the rewrite the exit-status contract forbids.
#[tokio::test]
async fn completion_cleanup_preserves_the_leaders_exit_status() {
    let mut shell = Shell::spawn(
        "sleep 300 & descendant=$!; echo descendant=$descendant; wait $descendant; exit 7",
    )
    .await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader),
        "the leader must still be running when the completion path runs, or this test proves nothing"
    );
    assert!(
        process_running(descendant),
        "the descendant must be alive before cleanup"
    );
    shell
        .tree
        .cleanup_descendants()
        .expect("cleanup descendants while the leader is alive");
    wait_until_stopped(descendant, "the descendant outlived the completion path").await;
    let status = shell.wait_for_leader().await;
    assert_eq!(
        status.code(),
        Some(7),
        "completion cleanup stopped the leader instead of only its descendants"
    );
    let stop = crate::Stop::complete(&status);
    assert_eq!(stop.exit_code, Some(7));
    assert_eq!(stop.termination, None);
    within("settle", shell.tree.settle())
        .await
        .expect("settle the completed job");
}

/// The cancellation-path action stops the whole job, leader included, and the
/// caller records that decision instead of inferring it from the status.
#[tokio::test]
async fn requested_termination_stops_the_leader_and_its_descendants() {
    let mut shell =
        Shell::spawn("sleep 300 & descendant=$!; echo descendant=$descendant; wait").await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "both members must be alive before the requested stop"
    );
    shell
        .tree
        .terminate_group()
        .expect("terminate the owned job");
    let status = shell.wait_for_leader().await;
    let stop = crate::Stop::stopped(&status);
    assert_eq!(stop.termination, Some(crate::Termination::Requested));
    wait_until_stopped(descendant, "the descendant outlived the requested stop").await;
    within("settle", shell.tree.settle())
        .await
        .expect("settle the stopped job");
}

/// Descendants started while cleanup enumerates the job must still be stopped.
///
/// The leader exits immediately while a background loop keeps starting members,
/// so cleanup has to re-read membership instead of trusting one snapshot.
/// Collecting more reports than the first process-id query buffer can hold also
/// drives the `ERROR_MORE_DATA` growth path, which one descendant never reaches.
#[tokio::test]
async fn cleanup_reaches_descendants_started_while_it_enumerates() {
    const WANTED: usize = 24;
    let mut shell = Shell::spawn(
        "while :; do sleep 300 & echo descendant=$!; done & echo descendant=$!; exit 0",
    )
    .await;
    let mut descendants = Vec::with_capacity(WANTED);
    for _ in 0..WANTED {
        descendants.push(shell.next_descendant("descendant report").await);
    }
    let status = shell.wait_for_leader().await;
    assert_eq!(status.code(), Some(0), "the leader completed on its own");
    shell
        .tree
        .cleanup_descendants()
        .expect("cleanup descendants of a completed command");
    within("settle", shell.tree.settle())
        .await
        .expect("settle the job that kept growing");
    for pid in &descendants {
        assert!(
            !process_running(*pid),
            "descendant {pid} started while cleanup enumerated the job and was never stopped"
        );
    }
}

/// Abandoning the owned tree stops the leader and its descendants while the
/// caller still holds the leader's child handle, so the job itself owns the
/// domain rather than the caller's reaping.
#[tokio::test]
async fn dropping_the_tree_stops_the_leader_and_its_descendants() {
    let mut shell =
        Shell::spawn("sleep 300 & descendant=$!; echo descendant=$descendant; wait").await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "both members must be alive before the tree is abandoned"
    );
    let Shell {
        mut child, tree, ..
    } = shell;
    drop(tree);
    wait_until_stopped(leader, "the leader outlived the abandoned job").await;
    wait_until_stopped(descendant, "a descendant outlived the abandoned job").await;
    let status = within("leader exit", child.wait())
        .await
        .expect("wait for the abandoned shell");
    let stop = crate::Stop::stopped(&status);
    assert_eq!(stop.termination, Some(crate::Termination::Requested));
}
