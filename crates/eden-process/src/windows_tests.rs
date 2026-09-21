//! Windows job-object smoke tests.
//!
//! [`crate::tests`] is Unix-only, so before this module the Windows target
//! compiled zero tests: the job-object ownership in `platform` was exercised
//! only through the installed acceptance suites. These tests drive a real shell
//! through the paths the product takes — completing a command, cancelling one,
//! abandoning the tree — and they observe descendants through Win32 process
//! handles rather than through the job membership the implementation
//! enumerates, so a cleanup that stopped nothing cannot pass by re-reading its
//! own bookkeeping.
//!
//! Git for Windows provides the shell these tests drive; that is the same
//! prerequisite the Windows native job and the repository hooks already have.
//! `bin\bash.exe` is a launcher: the native runner measured a spawned pid of
//! 5992 for a shell whose own pid was 3256, with both in the job, so the
//! completion test keeps the production order of reaping the shell before it
//! clears what outlived it. A descendant's pid comes from the shell's
//! `/proc/<pid>/winpid`, never from `$!`, which names an MSYS pid (the same run
//! measured `$!` = 787 for the process Windows knows as 5284). `Tree::drop`
//! stops the members through `terminate_group` before the job handle closes, so
//! these tests exercise job membership and termination, not the
//! `KILL_ON_JOB_CLOSE` limit that covers a failed stop.
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

/// The Bash that runs the script in the process it is spawned as.
///
/// Git for Windows ships `bin\bash.exe`, a launcher whose child runs the script,
/// beside the `usr\bin\bash.exe` that does not: the native runner measured a
/// spawned pid of 6712 for a shell whose own pid was 1124. Only a test that must
/// keep the leader alive across cleanup needs this direct shell, and it checks
/// the identity it depends on before asserting anything.
fn direct_shell() -> String {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let launcher = directory.join("bash.exe");
        if !launcher.is_file() {
            continue;
        }
        if let Some(root) = launcher.parent().and_then(std::path::Path::parent) {
            let direct = root.join("usr").join("bin").join("bash.exe");
            if direct.is_file() {
                return direct.to_string_lossy().into_owned();
            }
        }
        return launcher.to_string_lossy().into_owned();
    }
    panic!("a native Bash must be on PATH for process tests");
}

/// One spawned shell: the leader, its own reported Windows pid, the job that owns
/// it, and the report channel its descendants publish their ids on.
struct Shell {
    child: tokio::process::Child,
    tree: crate::Tree,
    shell_pid: u32,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Shell {
    /// Spawn `bash` from `PATH` with `command` as its script.
    async fn spawn(command: &str) -> Self {
        Self::spawn_with("bash", command).await
    }

    /// Spawn `shell` with `command` as its script.
    ///
    /// The script first reports the Windows process id the shell has for itself,
    /// so a `PATH` that resolves an unusable `bash.exe` fails here with its own
    /// output instead of as a cleanup timeout, and so a test can tell whether the
    /// process it owns is the shell. Each descendant is then reported as
    /// `descendant=<windows pid>`, which is how these tests learn about members
    /// without asking the job object what it contains.
    async fn spawn_with(shell: &str, command: &str) -> Self {
        let script = format!("echo \"shell=$(cat /proc/$$/winpid)\"; {command}");
        let (mut child, tree) = crate::spawn(
            shell,
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
        let shell_pid = greeting
            .as_deref()
            .and_then(|line| line.strip_prefix("shell="))
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or_else(|| panic!("the resolved shell is not a working Bash: {greeting:?}"));
        Self {
            child,
            tree,
            shell_pid,
            lines,
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

/// The completion path must not signal the process the crate owns.
///
/// This is the exit-status contract the module documents: cleanup excludes the
/// leader, so a command that is still running keeps its own code. It needs a
/// Bash that runs the script in the process the crate spawned, because with the
/// launcher the script runs in a descendant and killing it is what the
/// completion path is supposed to do to descendants. The leader blocks in `wait`
/// for a live descendant and pauses after it before reporting its own code, so
/// neither cleanup nor a lost scheduling race can be mistaken for the code the
/// command reached.
#[tokio::test]
async fn completion_cleanup_leaves_the_leader_running() {
    let shell = direct_shell();
    let command = format!(
        "sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; wait $descendant; sleep 2; exit 7"
    );
    let mut shell = Shell::spawn_with(&shell, &command).await;
    assert_eq!(
        shell.leader(),
        shell.shell_pid,
        "this test needs a Bash that runs the script in the process the crate spawned"
    );
    let leader = shell.leader();
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(leader) && process_running(descendant),
        "the leader and its descendant must both be alive before cleanup"
    );
    shell.assert_leader_running("completion cleanup with a live leader");
    shell
        .tree
        .cleanup_descendants()
        .expect("cleanup descendants while the leader is alive");
    wait_until_stopped(descendant, "the descendant outlived the completion path").await;
    let status = shell.wait_for_leader().await;
    assert_eq!(
        status.code(),
        Some(7),
        "completion cleanup signalled the leader instead of its descendants only"
    );
    within("settle", shell.tree.settle())
        .await
        .expect("settle the completed job");
}

/// The completion-path action stops the descendants that outlived a command and
/// keeps the exit code that command already reported.
///
/// This is the completion path the product takes: it reaps the shell, reads its
/// own code 7, and only then clears descendants that are still running, so the
/// surviving background process is provably alive when cleanup starts and gone
/// when the barrier returns. The leader is deliberately not kept alive across
/// cleanup here. Git for Windows' `bin\bash.exe` is a launcher whose child runs
/// the script — the native runner measured a spawned pid of 5992 for a shell
/// whose own pid was 3256, both job members — so an alive "leader" during
/// cleanup is not the shell that owns the command, and the Unix suite already
/// covers that race for a shell that is its own leader.
#[tokio::test]
async fn completion_cleanup_stops_survivors_of_a_completed_command() {
    let command = format!("sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; exit 7");
    let mut shell = Shell::spawn(&command).await;
    let descendant = shell.next_descendant("descendant report").await;
    assert!(
        process_running(descendant),
        "the descendant must outlive the command"
    );
    let status = shell.wait_for_leader().await;
    assert_eq!(
        status.code(),
        Some(7),
        "the shell's own code must reach the caller"
    );
    shell
        .tree
        .cleanup_descendants()
        .expect("cleanup the descendants that outlived the command");
    // Before the settle barrier: this is what shows that the completion action
    // itself stopped the descendant rather than the barrier that follows it.
    wait_until_stopped(descendant, "the descendant outlived the completion path").await;
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
        "while :; do sleep 300 & descendant=$!; {REPORT_WINDOWS_PID}; done & descendant=$!; \
         {REPORT_WINDOWS_PID}; exit 0"
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
    // The completion action stops every member it observed, so the barrier below
    // is not what these children depend on. A descendant the loop starts after
    // the snapshot is exactly what settle has to keep re-reading for.
    for pid in &descendants {
        wait_until_stopped(*pid, "a descendant outlived the completion action").await;
    }
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
