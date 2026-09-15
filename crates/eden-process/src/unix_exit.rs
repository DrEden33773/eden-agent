//! Process exit observation independent of inherited standard streams.
use super::fault;
use eden_plugin_sdk::protocol::Fault;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

fn error(action: &str, error: std::io::Error) -> Fault {
    fault("CleanupFailure", format!("{action}: {error}"))
}
fn os_error(action: &str) -> Fault {
    error(action, std::io::Error::last_os_error())
}

#[cfg(target_os = "linux")]
pub(super) struct Observer(OwnedFd);
#[cfg(target_os = "linux")]
fn member(pid: i32, group: i32) -> Result<bool, Fault> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(cause) => return Err(error("read shell process status", cause)),
    };
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| fault("CleanupFailure", "invalid process status"))?;
    let mut fields = fields.split_whitespace();
    let _state = fields.next();
    let _parent = fields.next();
    let actual_group = fields.next().and_then(|value| value.parse::<i32>().ok());
    Ok(actual_group == Some(group))
}
#[cfg(target_os = "linux")]
pub(super) fn observe(group: i32) -> Result<Vec<Observer>, Fault> {
    let mut observers = Vec::new();
    for entry in
        std::fs::read_dir("/proc").map_err(|cause| error("enumerate shell group", cause))?
    {
        let entry = entry.map_err(|cause| error("enumerate shell group entry", cause))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // SAFETY: getpgid only queries a numeric process id. Filter unrelated
        // processes before reading status that may have different permissions.
        if unsafe { libc::getpgid(pid) } != group {
            continue;
        }
        if !member(pid, group)? {
            continue;
        }
        // SAFETY: pidfd_open takes a numeric PID and zero flags, returning an
        // owned close-on-exec descriptor. No memory crosses this syscall.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(os_error(
                "observe shell member exit with pidfd_open (Linux 5.3+ required)",
            ));
        }
        // SAFETY: The successful syscall transferred one new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // Revalidate membership after opening to avoid observing a recycled PID.
        if member(pid, group)? && !pidfd_ready(&fd, 0)? {
            observers.push(Observer(fd));
        }
    }
    Ok(observers)
}
#[cfg(target_os = "linux")]
fn pidfd_ready(fd: &OwnedFd, timeout: i32) -> Result<bool, Fault> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: One initialized pollfd is writable for this kernel call and
        // its owned descriptor remains live. Zero only tests current readiness;
        // -1 waits for the actual exit event without periodic polling.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error("observe shell member pidfd"));
        }
        if result == 0 {
            return Ok(false);
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(true);
        }
        return Err(fault(
            "CleanupFailure",
            "shell member pidfd returned unexpected readiness",
        ));
    }
}
#[cfg(target_os = "linux")]
impl Observer {
    pub(super) fn wait(self) -> Result<(), Fault> {
        pidfd_ready(&self.0, -1).map(|_| ())
    }
}

#[cfg(target_os = "macos")]
pub(super) struct Observer(OwnedFd);
#[cfg(target_os = "macos")]
fn member(pid: i32, group: i32) -> Result<bool, Fault> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: The output allocation fits proc_bsdinfo; the kernel fills it on
    // successful PROC_PIDTBSDINFO with the matching buffer length.
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if size == 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(os_error("read shell member status"));
    }
    if size != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        return Err(fault("CleanupFailure", "incomplete shell member status"));
    }
    // SAFETY: The complete expected structure was written above.
    let info = unsafe { info.assume_init() };
    Ok(info.pbi_pgid == group as u32 && info.pbi_status != libc::SZOMB)
}
#[cfg(target_os = "macos")]
pub(super) fn observe(group: i32) -> Result<Vec<Observer>, Fault> {
    // PROC_PGRP_ONLY is the public libproc.h selector for a process group.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 64];
    loop {
        let bytes = i32::try_from(std::mem::size_of_val(pids.as_slice()))
            .map_err(|_| fault("CleanupFailure", "shell process list too large"))?;
        // SAFETY: The PID buffer is initialized, aligned and writable with the
        // supplied length. The group selector returns only numeric process IDs.
        let written = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                group as u32,
                pids.as_mut_ptr().cast(),
                bytes,
            )
        };
        if written < 0 {
            return Err(os_error("enumerate shell process group"));
        }
        if written < bytes {
            pids.truncate(written as usize / std::mem::size_of::<i32>());
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    let mut observers = Vec::new();
    for pid in pids {
        if pid <= 0 || !member(pid, group)? {
            continue;
        }
        // SAFETY: kqueue returns a new owned descriptor and takes no arguments.
        let descriptor = unsafe { libc::kqueue() };
        if descriptor < 0 {
            return Err(os_error("create shell exit observer"));
        }
        // SAFETY: The successful kqueue call transferred this unique descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let event = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: One initialized change record is borrowed synchronously; with
        // zero output events this only registers interest and never waits.
        if unsafe {
            libc::kevent(
                descriptor.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(os_error("register shell NOTE_EXIT"));
        }
        if member(pid, group)? {
            observers.push(Observer(descriptor));
        }
    }
    Ok(observers)
}
#[cfg(target_os = "macos")]
impl Observer {
    pub(super) fn wait(self) -> Result<(), Fault> {
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        loop {
            // SAFETY: The kqueue descriptor is live and one output event fits
            // the allocation. No timeout means a kernel event wait, not polling.
            let received = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    event.as_mut_ptr(),
                    1,
                    std::ptr::null(),
                )
            };
            if received < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(os_error("wait for shell NOTE_EXIT"));
            }
            if received != 1 {
                return Err(fault(
                    "CleanupFailure",
                    "shell exit observer returned no event",
                ));
            }
            // SAFETY: kevent reported exactly one initialized output event.
            let event = unsafe { event.assume_init() };
            if event.flags & libc::EV_ERROR != 0 {
                return Err(error(
                    "shell exit observer",
                    std::io::Error::from_raw_os_error(event.data as i32),
                ));
            }
            if event.fflags & libc::NOTE_EXIT != 0 {
                return Ok(());
            }
            return Err(fault(
                "CleanupFailure",
                "shell exit observer returned an unexpected event",
            ));
        }
    }
}
