//! Platform process ownership. Every shell starts inside its cleanup domain.
use super::fault;
use eden_plugin_sdk::protocol::Fault;
use std::{path::Path, process::Stdio};
use tokio::process::{Child, Command};

pub(crate) async fn spawn(shell: &str, script: &str, cwd: &Path) -> Result<(Child, Tree), Fault> {
    let mut command = Command::new(shell);
    command
        .args(["--noprofile", "--norc", "-c", script])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    platform::configure(&mut command);
    let mut child = command.spawn().map_err(|error| fault("ShellUnavailable", format!("cannot launch bash executable {shell:?}: {error}; install bash or configure coding-tools.bash with its executable path")))?;
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
pub(crate) use platform::Tree;

#[cfg(unix)]
mod platform {
    use super::*;
    use crate::unix_exit::{Observer, observe};
    pub(crate) struct Tree {
        pid: i32,
        terminated: std::sync::atomic::AtomicBool,
        observers: std::sync::Mutex<Vec<Observer>>,
    }
    pub(crate) fn configure(command: &mut Command) {
        command.process_group(0);
    }
    pub(crate) fn attach(child: &Child) -> Result<Tree, Fault> {
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| fault("ToolFailure", "shell process id unavailable"))?;
        Ok(Tree {
            pid,
            terminated: std::sync::atomic::AtomicBool::new(false),
            observers: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn signal(group: i32) -> Result<(), Fault> {
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
    impl Tree {
        pub(crate) fn terminate(&self) -> Result<(), Fault> {
            if self.terminated.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            // Register exit interest before sending a signal. Even observation
            // failure must still attempt to stop the owned process group.
            let observers = observe(self.pid);
            let stopped = signal(self.pid);
            if stopped.is_ok() {
                self.terminated
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *self
                .observers
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = observers?;
            stopped
        }
        pub(crate) async fn settle(&self) -> Result<(), Fault> {
            let group = self.pid;
            let mut observers = std::mem::take(
                &mut *self
                    .observers
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            );
            tokio::task::spawn_blocking(move || {
                loop {
                    for observer in observers {
                        observer.wait()?;
                    }
                    // A descendant can fork while the original snapshot is
                    // taken. Observe remaining live members after exit events,
                    // signal that batch, and wait for their own exit events.
                    observers = observe(group)?;
                    if observers.is_empty() {
                        return Ok(());
                    }
                    signal(group)?;
                }
            })
            .await
            .map_err(|error| fault("CleanupFailure", error.to_string()))?
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            if !self.terminated.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = signal(self.pid);
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
            ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
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
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                CREATE_SUSPENDED, INFINITE, OpenProcess, OpenThread,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread,
                THREAD_SUSPEND_RESUME, WaitForSingleObject,
            },
        },
    };
    pub(crate) struct Tree(Arc<OwnedHandle>);
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
        let tree = Tree(Arc::new(job));
        let pid = child
            .id()
            .ok_or_else(|| fault("ToolFailure", "shell process id unavailable"))?;
        resume_main_thread(pid)?;
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
    impl Tree {
        pub(crate) fn terminate(&self) -> Result<(), Fault> {
            // SAFETY: This job contains only the shell and its descendants.
            if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
                Err(win_error("terminate shell job"))
            } else {
                Ok(())
            }
        }
        pub(crate) async fn settle(&self) -> Result<(), Fault> {
            let job = self.0.clone();
            tokio::task::spawn_blocking(move || {
                // Completion-port exit messages are not guaranteed by Win32.
                // Wait for live member handles, then confirm empty membership.
                loop {
                    let pids = members(&job)?;
                    if pids.is_empty() {
                        return Ok(());
                    }
                    for pid in pids {
                        let pid = u32::try_from(pid)
                            .map_err(|_| fault("CleanupFailure", "invalid shell process id"))?;
                        // SAFETY: OpenProcess returns an owned handle; a stale id
                        // is checked against this job before any wait occurs.
                        let handle = unsafe {
                            OpenProcess(
                                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                                0,
                                pid,
                            )
                        };
                        if handle.is_null()
                            && std::io::Error::last_os_error().raw_os_error()
                                == Some(ERROR_INVALID_PARAMETER as i32)
                        {
                            continue;
                        }
                        let process = owned(handle, "open exiting shell job member")?;
                        let mut in_job = 0;
                        // SAFETY: Both handles are live; in_job is writable.
                        if unsafe {
                            IsProcessInJob(
                                process.as_raw_handle(),
                                job.as_raw_handle(),
                                &mut in_job,
                            )
                        } == 0
                        {
                            return Err(win_error("verify shell job member"));
                        }
                        if in_job == 0 {
                            continue;
                        }
                        // SAFETY: The owned process has SYNCHRONIZE access and
                        // termination was requested for every member beforehand.
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
            let _ = self.terminate();
        }
    }
}
