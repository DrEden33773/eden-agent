//! Windows job-object smoke tests.
//!
//! [`crate::tests`] is Unix-only, so before this module the Windows target
//! compiled zero tests: the job-object ownership in `platform` was exercised
//! only through the installed acceptance suites. These tests assert the same
//! exit-status contract as the Unix suite against a real shell, and they observe
//! descendants through Win32 process handles rather than through the job
//! membership the implementation enumerates, so a cleanup that stopped nothing
//! cannot pass by re-reading its own bookkeeping.
//!
//! Git for Windows provides the shell these tests drive; that is the same
//! prerequisite the Windows native job and the repository hooks already have.
//! A descendant's own process id comes from the shell, never from `$!`: under
//! Git Bash `$!` is an MSYS pid, and the native runner measured `$!` = 787 for
//! the process Windows knows as 5284, so probing it would observe an unrelated
//! process. `Tree::drop` stops the members through `terminate_group` before the
//! job handle closes, so these tests exercise job membership and termination,
//! not the `KILL_ON_JOB_CLOSE` limit that covers a failed stop.
#![cfg(windows)]

use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, Lines};

/// Shell fragment that reports the Windows process id of the background process
/// in `descendant`.
const REPORT_WINDOWS_PID: &str = "echo \"descendant=$(cat /proc/$descendant/winpid)\"";

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
/// answer for this probe. A caller stops a descendant and probes it immediately,
/// so the id cannot have been recycled by then.
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
/// its descendants use to publish their Windows process ids.
struct Shell {
    child: tokio::process::Child,
    tree: crate::Tree,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Shell {
    /// Spawn the native shell with `command` as its script.
    ///
    /// The script first reports the shell it reached, so a `PATH` that resolves
    /// some other `bash.exe` fails here with its own output instead of as a
    /// cleanup timeout. Each descendant is then reported as
    /// `descendant=<windows pid>`, which is how these tests learn about members
    /// without asking the job object what it contains.
    async fn spawn(command: &str) -> Self {
        let script = format!("echo \"shell=${{BASH_VERSION:-missing}}\"; {command}");
        let (mut child, tree) = crate::spawn(
            "bash",
            &["--noprofile", "--norc", "-c", &script],
            Path::new("."),
        )
        .await
        .expect("a native Bash must be on PATH for process tests");
        let stdout = child.stdout.take().expect("shell stdout is piped");
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain(stderr));
        }
        let mut lines = BufReader::new(stdout).lines();
        let greeting = within("shell greeting", lines.next_line())
            .await
            .expect("read the shell greeting");
        assert!(
            greeting
                .as_deref()
                .is_some_and(|line| line.starts_with("shell=") && !line.ends_with("missing")),
            "PATH resolved a shell that is not a working Bash: {greeting:?}"
        );
        Self { child, tree, lines }
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

    /// Fail unless the leader is still running, so a test never reports a
    /// preserved exit status for a leader that finished before cleanup ran.
    fn assert_leader_running(&mut self, context: &str) {
        assert!(
            self.child.try_wait().expect("query the shell").is_none(),
            "{context}: the shell had already exited, so the assertion below would be vacuous"
        );
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
    let command =
        format!("sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; wait $descendant; exit 7");
    let mut shell = Shell::spawn(&command).await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "the leader and its descendant must both be alive before cleanup"
    );
    shell.assert_leader_running("completion cleanup");
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
    within("settle", shell.tree.settle())
        .await
        .expect("settle the completed job");
}

/// The cancellation-path action stops the whole job, leader included.
///
/// The leader is blocked in `wait`, which bash can only leave by itself if the
/// descendant dies, and the script would then report code 7. Observing the
/// stop's own code instead is what tells the caller's explicit cancellation
/// apart from a command that finished on its own.
#[tokio::test]
async fn requested_termination_stops_the_leader_and_its_descendants() {
    // The leader pauses before reporting its own code, so terminating the
    // descendant first cannot let a leader this job failed to stop win a race
    // and report code 7 by itself.
    let command = format!(
        "sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; wait $descendant; sleep 2; exit 7"
    );
    let mut shell = Shell::spawn(&command).await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "both members must be alive before the requested stop"
    );
    shell.assert_leader_running("requested termination");
    shell
        .tree
        .terminate_group()
        .expect("terminate the owned job");
    let status = shell.wait_for_leader().await;
    assert_eq!(
        status.code(),
        Some(1),
        "the requested stop did not end the leader before it could report its own code"
    );
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
    let command = format!(
        "while :; do sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; done & descendant=$!; {REPORT_WINDOWS_PID}; exit 0"
    );
    let mut shell = Shell::spawn(&command).await;
    let mut descendants = Vec::with_capacity(WANTED);
    for _ in 0..WANTED {
        descendants.push(shell.next_descendant("descendant report").await);
    }
    let status = shell.wait_for_leader().await;
    assert_eq!(status.code(), Some(0), "the leader completed on its own");
    for pid in &descendants {
        assert!(
            process_running(*pid),
            "descendant {pid} must be alive before cleanup enumerates the job"
        );
    }
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
/// caller still holds the leader's child handle.
///
/// The stop has to come from the job's membership rather than from the caller's
/// reaping: the leader is blocked in `wait` and pauses before reporting its own
/// code, and the child handle stays open across the drop.
#[tokio::test]
async fn dropping_the_tree_stops_the_leader_and_its_descendants() {
    let command = format!(
        "sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; wait $descendant; sleep 2; exit 7"
    );
    let mut shell = Shell::spawn(&command).await;
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "both members must be alive before the tree is abandoned"
    );
    shell.assert_leader_running("abandoning the tree");
    let Shell {
        mut child, tree, ..
    } = shell;
    drop(tree);
    wait_until_stopped(leader, "the leader outlived the abandoned job").await;
    wait_until_stopped(descendant, "a descendant outlived the abandoned job").await;
    let status = within("leader exit", child.wait())
        .await
        .expect("wait for the abandoned shell");
    assert_eq!(
        status.code(),
        Some(1),
        "the abandoned job did not stop the leader before it could report its own code"
    );
}
