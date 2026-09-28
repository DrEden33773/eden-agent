//! Shared file permissions keep host private input and plugin credentials on the same platform policy.
use eden_protocol::Fault;
use std::fs::File;

fn fault(message: &str) -> Fault {
    Fault::new("Unavailable", "private_storage", message)
}

/// Opens or creates storage without truncating it, so callers can lock or update private records.
/// Unix rejects existing files accessible to other users; Windows applies an owner-only DACL.
/// Close Windows handles before replacing files because delete sharing is intentionally disabled.
/// Parent directories must already exist. Errors omit paths and operating-system details.
#[cfg(not(windows))]
pub fn open(path: &std::path::Path) -> Result<File, Fault> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| fault("cannot open private storage"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .metadata()
            .map_err(|_| fault("cannot inspect private storage permissions"))?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(fault(
                "private storage permissions must restrict access to its owner",
            ));
        }
    }
    Ok(file)
}
/// Opens or creates storage without truncating it, so callers can lock or update private records.
/// Unix rejects existing files accessible to other users; Windows applies an owner-only DACL.
/// Close Windows handles before replacing files because delete sharing is intentionally disabled.
/// Parent directories must already exist. Errors omit paths and operating-system details.
#[cfg(windows)]
pub fn open(path: &std::path::Path) -> Result<File, Fault> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SetKernelObjectSecurity,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
            WRITE_DAC,
        },
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)".encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: UTF-16 input remains alive; Win32 allocates the descriptor, released below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(fault("cannot create private storage permissions"));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: All input buffers and attributes live through CreateFileW; handle ownership transfers to File.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: This descriptor was allocated above and has no remaining users.
        unsafe {
            LocalFree(descriptor);
        };
        return Err(fault("cannot open private storage"));
    }
    // SAFETY: The valid owned handle remains live; descriptor is valid through this call.
    let secured = unsafe { SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, descriptor) };
    // SAFETY: Win32 allocated this descriptor above; no references remain after this point.
    unsafe {
        LocalFree(descriptor);
    }
    // SAFETY: CreateFileW returned a new owned handle and no other owner exists.
    let file = unsafe { File::from_raw_handle(handle) };
    if secured == 0 {
        return Err(fault("cannot restrict private storage permissions"));
    }
    Ok(file)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eden-private-file-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reopening_preserves_written_contents() {
        let directory = Directory::new();
        let path = directory.0.join("record");
        let mut file = open(&path).unwrap();
        file.write_all(b"private record").unwrap();
        drop(file);
        let mut contents = String::new();
        open(&path).unwrap().read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "private record");
    }

    #[cfg(unix)]
    #[test]
    fn created_files_are_accessible_only_to_owner() {
        use std::os::unix::fs::PermissionsExt;
        let directory = Directory::new();
        let file = open(&directory.0.join("record")).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
    }

    #[cfg(unix)]
    #[test]
    fn existing_shared_files_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let directory = Directory::new();
        let path = directory.0.join("record");
        drop(open(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(open(&path).is_err());
    }
}
