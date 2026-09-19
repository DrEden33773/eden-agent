//! Platform process ownership. Every shell starts inside its cleanup domain.
//!
//! # Exit-status contract
//!
//! The process handed back is the *foreground leader* of an owned cleanup
//! domain: on Unix its own process group, on Windows a job object. Only the
//! leader's own exit status is ever reported to callers.
//!
//! Cleanup never rewrites that status:
//!
//! - [`Tree::cleanup_descendants`] is the completion-path action. It signals
//!   descendants that are still alive and deliberately excludes the leader, so
//!   finishing a command can never turn the leader's real exit code into a
//!   signal death.
//! - [`Tree::terminate_group`] is the cancellation-path action. It stops the
//!   whole domain, leader included, and exists only because a cancelled command
//!   may still be running. Callers reach it after deciding that the command
//!   must not continue, and report that decision explicitly through
//!   [`Stop::termination`] instead of inferring it from a missing exit code.
//!
//! A member that exits while cleanup enumerates it is a normal race, not a
//! failure: enumeration reports the surviving members and never fails because
//! one of them finished first.
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "process", message)
}
#[cfg(unix)]
mod unix_exit;
use eden_plugin_sdk::protocol::Fault;
use std::{path::Path, process::Stdio};
use tokio::process::{Child, Command};

/// Who asked for a termination, so callers classify an outcome explicitly
/// instead of guessing from a missing exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    /// The tool's caller cancelled the operation.
    Requested,
}

/// Why a process domain stopped.
///
/// `termination` is `Some` exactly when this module stopped the domain because
/// a caller asked for it, and `exit_code` then holds whatever status the leader
/// really reached: an exit code if the command finished before the stop landed,
/// or `None` if our own signal ended it.
///
/// `exit_code` is `None` together with `termination: None` only when the leader
/// died by signal without any requested termination, which means something
/// outside this module stopped it. That is the one case callers can read as an
/// unexplained signal death; `None` on its own never implies one, because a
/// failure before the leader started also reports no exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stop {
    pub exit_code: Option<i32>,
    pub termination: Option<Termination>,
}

impl Stop {
    /// Classify a completion-path status: only the leader decided it.
    pub fn complete(status: &std::process::ExitStatus) -> Self {
        Self {
            exit_code: status.code(),
            termination: None,
        }
    }
    /// Classify a status observed after a requested termination.
    ///
    /// `termination` is always recorded. The observed status is kept as well:
    /// if the command exited on its own before the stop reached it, that exit
    /// code is a real result and discarding it would report a completed command
    /// as having no exit code. An absent code after a requested stop is our own
    /// signal and is explained by `termination`.
    pub fn stopped(status: &std::process::ExitStatus) -> Self {
        Self {
            exit_code: status.code(),
            termination: Some(Termination::Requested),
        }
    }
}

pub async fn spawn(shell: &str, args: &[&str], cwd: &Path) -> Result<(Child, Tree), Fault> {
    #[cfg(windows)]
    let shell = &resolve_windows_shell(shell, cwd)?;
    let mut command = Command::new(shell);
    command.args(args);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    platform::configure(&mut command);
    let mut child = command.spawn().map_err(|error| {
        fault(
            "ShellUnavailable",
            format!(
                "cannot launch shell executable {shell:?}: {error}; configure coding-tools.bash or coding-tools.powershell with its executable path"
            ),
        )
    })?;
    match platform::attach(&child) {
        Ok(tree) => Ok((child, tree)),
        Err(error) => {
            // Assignment/resume failures must reap the still-suspended child.
            let killed = child.kill().await;
            match killed {
                Ok(()) => Err(error),
                Err(kill) => Err(fault(
                    "CleanupFailure",
                    format!("{error}; cannot reap shell: {kill}"),
                )),
            }
        }
    }
}

#[cfg(windows)]
fn resolve_windows_shell(shell: &str, cwd: &Path) -> Result<std::path::PathBuf, Fault> {
    let requested = Path::new(shell);
    if requested.components().count() > 1 || requested.is_absolute() {
        return Ok(cwd.join(requested));
    }
    // CreateProcess searches System32 before PATH for an unqualified name.
    // Resolve PATH ourselves so a selected native Bash is not shadowed by
    // Windows' WSL launcher. Passing the full path fixes the actual launch.
    let executable = if requested.extension().is_some() {
        requested.to_owned()
    } else {
        requested.with_extension("exe")
    };
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join(&executable);
        if candidate.is_file() {
            return std::path::absolute(candidate)
                .map_err(|error| fault("ShellUnavailable", error.to_string()));
        }
    }
    Err(fault(
        "ShellUnavailable",
        format!(
            "native shell executable {shell:?} was not found in PATH; configure the selected shell with its absolute path"
        ),
    ))
}
pub use platform::Tree;

#[cfg(unix)]
mod platform {
    use super::*;
    use crate::unix_exit::observe;
    pub struct Tree {
        /// Process group id, which equals the leader's pid because the shell is
        /// created with `process_group(0)`.
        group: i32,
        /// Set once the whole group has been stopped, so a repeated hard stop
        /// cannot signal an unrelated recycled id a second time.
        stopped: std::sync::atomic::AtomicBool,
    }
    pub(crate) fn configure(command: &mut Command) {
        command.process_group(0);
    }
    pub(crate) fn attach(child: &Child) -> Result<Tree, Fault> {
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| fault("ToolFailure", "shell process id unavailable"))?;
        Ok(Tree {
            group,
            stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }
    /// Signal every live member of the owned group, leader included.
    fn signal_group(group: i32) -> Result<(), Fault> {
        // SAFETY: The child was created as its own group leader. A negative PID
        // targets that owned group; SIGKILL has no borrowed memory.
        if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(fault(
                "CleanupFailure",
                format!("cannot stop shell process group: {error}"),
            ))
        }
    }
    /// Signal the listed survivors individually. The leader is never in this
    /// list, so no signal used for completion can reach a status already read.
    fn signal_members(survivors: &[i32]) -> Result<(), Fault> {
        for member in survivors {
            // SAFETY: A positive PID targets exactly that process. A member
            // that exited since it was observed yields ESRCH, which is the
            // expected race and means it is no longer a survivor.
            if unsafe { libc::kill(*member, libc::SIGKILL) } == 0 {
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(fault(
                "CleanupFailure",
                format!("cannot stop shell descendant {member}: {error}"),
            ));
        }
        Ok(())
    }
    /// Live group members other than the leader.
    fn descendants(group: i32) -> Result<Vec<i32>, Fault> {
        Ok(observe(group)?
            .into_iter()
            .map(|member| member.pid())
            .filter(|pid| *pid != group)
            .collect())
    }
    impl Tree {
        /// Stop the whole owned group, leader included.
        ///
        /// This is the cancellation-path action and the only method that may
        /// signal the leader. Callers reach it only after deciding that the
        /// command must not continue.
        pub fn terminate_group(&self) -> Result<(), Fault> {
            if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            let stopped = signal_group(self.group);
            if stopped.is_ok() {
                self.stopped
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            stopped
        }
        /// Stop descendants that outlived the leader.
        ///
        /// This is the completion-path action. The leader is excluded, so a
        /// command that already reported its own exit code keeps that code.
        pub fn cleanup_descendants(&self) -> Result<(), Fault> {
            signal_members(&descendants(self.group)?)
        }
        /// Stop every surviving member of the owned group, then wait until the
        /// group is empty.
        ///
        /// A descendant can fork while a snapshot is being taken, so a single
        /// enumeration is not a guarantee: each pass signals exactly the
        /// members it observed and waits for them, and a pass that observes a
        /// new member signals that one too. The pidfd opened at observation
        /// time is what receives the signal, so a recycled pid can never be
        /// signalled by mistake.
        pub async fn settle(&self) -> Result<(), Fault> {
            let group = self.group;
            tokio::task::spawn_blocking(move || {
                loop {
                    let survivors = observe(group)?;
                    if survivors.is_empty() {
                        return Ok(());
                    }
                    for member in &survivors {
                        // Signal the descriptor opened for this exact member,
                        // so a descendant that forked after an earlier pass is
                        // stopped rather than only waited for.
                        member.signal()?;
                    }
                    for member in survivors {
                        // A member can exit between the snapshot and this wait.
                        // That race is completion, not failure.
                        member.wait()?;
                    }
                }
            })
            .await
            .map_err(|error| fault("CleanupFailure", error.to_string()))?
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            // The owner abandoned this tree, so the whole domain, leader
            // included, must go.
            if !self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = signal_group(self.group);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr::null,
        sync::Arc,
    };
    use windows_sys::Win32::{
        Foundation::{
            ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
            WAIT_OBJECT_0,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_PROCESS_ID_LIST,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject,
            },
            Threading::{
                CREATE_SUSPENDED, GetExitCodeProcess, INFINITE, OpenProcess, OpenThread,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
            },
        },
    };
    pub struct Tree {
        job: Arc<OwnedHandle>,
        /// The shell itself. Descendant cleanup excludes it so completing a
        /// command can never rewrite the leader's own exit code.
        leader: u32,
    }
    fn win_error(action: &str) -> Fault {
        fault(
            "CleanupFailure",
            format!("{action}: {}", std::io::Error::last_os_error()),
        )
    }
    fn owned(handle: HANDLE, action: &str) -> Result<OwnedHandle, Fault> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(win_error(action))
        } else {
            // SAFETY: These Win32 constructors transfer one unique live handle to us.
            Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
        }
    }
    pub(crate) fn configure(command: &mut Command) {
        command.creation_flags(CREATE_SUSPENDED);
    }
    pub(crate) fn attach(child: &Child) -> Result<Tree, Fault> {
        // SAFETY: Null pointers request an unnamed job with default security.
        let job = owned(
            unsafe { CreateJobObjectW(null(), null()) },
            "create shell job",
        )?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: The input is initialized, its size matches the class, and the
        // owned job handle remains live for this synchronous call.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(win_error("configure shell job limits"));
        }
        let process = child
            .raw_handle()
            .ok_or_else(|| fault("ToolFailure", "shell process handle unavailable"))?;
        // SAFETY: Child owns the live process handle. CREATE_SUSPENDED ensures
        // the shell cannot create a descendant before it belongs to this job.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            return Err(win_error("assign suspended shell to job"));
        }
        let leader = child
            .id()
            .ok_or_else(|| fault("ToolFailure", "shell process id unavailable"))?;
        let tree = Tree {
            job: Arc::new(job),
            leader,
        };
        resume_main_thread(leader)?;
        Ok(tree)
    }
    fn resume_main_thread(pid: u32) -> Result<(), Fault> {
        // std's main_thread_handle is unstable; enumerate the sole thread of
        // the still-suspended process using the documented ToolHelp API.
        // SAFETY: The API returns an owned snapshot; zero is valid with SNAPTHREAD.
        let snapshot = owned(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) },
            "snapshot suspended shell thread",
        )?;
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: entry is writable with its required size and snapshot is live.
        let mut present = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while present != 0 {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: The suspended process still owns this thread id. Open
                // returns a new handle with only the needed resume right.
                let thread = owned(
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) },
                    "open suspended shell thread",
                )?;
                // SAFETY: thread is live and has THREAD_SUSPEND_RESUME access.
                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                    return Err(win_error("resume shell thread"));
                }
                return Ok(());
            }
            // SAFETY: Same initialized entry and live snapshot as Thread32First.
            present = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        Err(fault("ToolFailure", "suspended shell thread was not found"))
    }
    fn members(job: &OwnedHandle) -> Result<Vec<usize>, Fault> {
        let mut storage = vec![0usize; 18];
        loop {
            let bytes = u32::try_from(storage.len() * size_of::<usize>()).map_err(|_| {
                fault(
                    "CleanupFailure",
                    "shell job membership exceeds Win32 buffer limit",
                )
            })?;
            // SAFETY: usize storage has the alignment of PROCESS_ID_LIST; its
            // allocation includes the two u32 counts and all reported ids.
            let success = unsafe {
                QueryInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    storage.as_mut_ptr().cast(),
                    bytes,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: The allocation has at least the fixed structure size and
            // QueryInformationJobObject initializes both counts even on MORE_DATA.
            let list = unsafe { &*storage.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            if success != 0 {
                let count = list.NumberOfProcessIdsInList as usize;
                // SAFETY: A successful query guarantees that the count fits the
                // supplied buffer; copy the variable-length array before resize.
                return Ok(unsafe {
                    std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), count)
                }
                .to_vec());
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(win_error("read shell job membership"));
            }
            let capacity = list.NumberOfAssignedProcesses as usize + 2;
            storage.resize(capacity.max(storage.len() * 2), 0);
        }
    }
    /// Open a job member, treating "already gone" as the normal exit race
    /// rather than a cleanup failure.
    fn open_member(pid: u32) -> Result<Option<OwnedHandle>, Fault> {
        // SAFETY: OpenProcess returns an owned handle or null.
        let handle = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(None);
            }
            return Err(win_error("open shell job member"));
        }
        // SAFETY: The successful call transferred one unique live handle.
        Ok(Some(unsafe { OwnedHandle::from_raw_handle(handle.cast()) }))
    }
    fn in_job(job: &OwnedHandle, process: &OwnedHandle) -> Result<bool, Fault> {
        let mut member = 0;
        // SAFETY: Both handles are live; the output is writable.
        if unsafe { IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut member) } == 0
        {
            return Err(win_error("verify shell job member"));
        }
        Ok(member != 0)
    }
    /// Whether a job member has stopped running.
    ///
    /// The exit code answers this while a process is still terminating, when its
    /// wait state is not yet signaled: `STILL_ACTIVE` means it really is running,
    /// and anything else means it has already stopped or is on its way out.
    ///
    /// A member that deliberately exits with code 259 is indistinguishable from
    /// a running one here, so that member reports a cleanup failure instead of
    /// being tolerated. The error is the safe direction: a real failed stop is
    /// never swallowed.
    fn has_exited(process: &OwnedHandle) -> Result<bool, Fault> {
        let mut code = 0u32;
        // SAFETY: The handle owns PROCESS_QUERY_LIMITED_INFORMATION and the exit
        // code output is writable.
        if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
            return Err(win_error("read shell job member state"));
        }
        Ok(code != STILL_ACTIVE as u32)
    }
    /// Terminate job members one at a time, optionally skipping the leader.
    ///
    /// `TerminateJobObject` is deliberately not used: it overwrites the exit
    /// code of every member, including the leader, with the value passed to it,
    /// which is exactly the class of status rewrite this contract forbids.
    fn stop_members(tree: &Tree, include_leader: bool) -> Result<(), Fault> {
        for pid in members(&tree.job)? {
            let pid = u32::try_from(pid)
                .map_err(|_| fault("CleanupFailure", "invalid shell process id"))?;
            if !include_leader && pid == tree.leader {
                continue;
            }
            // A member that exited between enumeration and open is the normal
            // race and needs no signal.
            let Some(process) = open_member(pid)? else {
                continue;
            };
            if !in_job(&tree.job, &process)? {
                continue;
            }
            // SAFETY: The handle has PROCESS_TERMINATE access and belongs to
            // this job, which contains only the shell and its descendants.
            if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
                // Windows refuses to stop a process that already started
                // terminating and reports that as access denied. A member that
                // stopped after it was enumerated is the normal race, exactly as
                // on Unix, so ask the process whether it still runs instead of
                // trusting the failed stop: only a running member is a failure.
                if has_exited(&process)? {
                    continue;
                }
                return Err(win_error("terminate shell job member"));
            }
        }
        Ok(())
    }
    impl Tree {
        /// Stop every member of the job, leader included.
        ///
        /// This is the cancellation-path action and the only method that may
        /// stop the leader.
        pub fn terminate_group(&self) -> Result<(), Fault> {
            stop_members(self, true)
        }
        /// Stop job members other than the leader.
        ///
        /// This is the completion-path action. The leader is excluded, so a
        /// command that already reported its own exit code keeps that code.
        pub fn cleanup_descendants(&self) -> Result<(), Fault> {
            stop_members(self, false)
        }
        /// Stop every surviving member of the job, then wait until the job is
        /// empty.
        ///
        /// A member can start a new process while membership is enumerated, so
        /// each pass stops exactly the members it observed and waits for them;
        /// a pass that observes a new member stops that one too. Completion-port
        /// exit messages are not guaranteed by Win32, which is why membership
        /// is re-read instead of waiting for notifications.
        pub async fn settle(&self) -> Result<(), Fault> {
            let job = self.job.clone();
            tokio::task::spawn_blocking(move || {
                loop {
                    let pids = members(&job)?;
                    if pids.is_empty() {
                        return Ok(());
                    }
                    for pid in pids {
                        let pid = u32::try_from(pid)
                            .map_err(|_| fault("CleanupFailure", "invalid shell process id"))?;
                        let Some(process) = open_member(pid)? else {
                            continue;
                        };
                        if !in_job(&job, &process)? {
                            continue;
                        }
                        // SAFETY: The handle has PROCESS_TERMINATE access and
                        // belongs to this job. A member that already stopped
                        // returns an error that is ignored, because the goal is
                        // only that it is not running.
                        let _ = unsafe { TerminateProcess(process.as_raw_handle(), 1) };
                        // SAFETY: The owned process has SYNCHRONIZE access. The
                        // leader's status was classified before this barrier, so
                        // waiting on it cannot change that report.
                        if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) }
                            != WAIT_OBJECT_0
                        {
                            return Err(win_error("wait for shell job member exit"));
                        }
                    }
                }
            })
            .await
            .map_err(|error| fault("CleanupFailure", error.to_string()))?
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            // The owner abandoned this tree, so the whole job, leader included,
            // must go. KILL_ON_JOB_CLOSE also covers a failed stop here.
            let _ = self.terminate_group();
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod windows_tests;
