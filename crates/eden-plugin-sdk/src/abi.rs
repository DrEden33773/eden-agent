//! Stable-layout header inspection before accessing the exact-version function table.
use eden_protocol::Fault;
/// Header always read before the remainder of a plugin table.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Header {
    /// `EDENABI\0`, so a foreign library is rejected before anything else is read.
    pub magic: [u8; 8],
    /// The ABI generation this table follows.
    pub abi: u32,
    /// Size of the whole table, checked against this host's own.
    pub size: u32,
    /// The SDK pairing string, zero-padded.
    pub sdk: [u8; 32],
    /// The target triple the library was built for, zero-padded.
    pub target: [u8; 64],
}
/// Produce zero-padded version fields.
pub const fn field<const N: usize>(text: &str) -> [u8; N] {
    let mut bytes = [0; N];
    let mut i = 0;
    while i < text.len() && i < N {
        bytes[i] = text.as_bytes()[i];
        i += 1;
    }
    bytes
}
/// This build's target triple.
pub const TARGET: &str = env!("EDEN_TARGET");
/// Verify the ABI prefix before any function pointer is read.
pub fn validate_header(header: &Header, expected_size: usize) -> Result<(), Fault> {
    if header.magic != *b"EDENABI\0"
        || header.abi != 1
        || header.size as usize != expected_size
        || header.sdk != field(eden_protocol::CONTRACT)
        || header.target != field(TARGET)
    {
        return Err(Fault::new(
            "IncompatibleContract",
            "loader",
            "ABI table, SDK, or target does not match this host",
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_sdk_is_rejected_before_reading_functions() {
        let header = Header {
            magic: *b"EDENABI\0",
            abi: 1,
            size: 128,
            sdk: field("wrong-sdk"),
            target: field(TARGET),
        };
        assert_eq!(
            validate_header(&header, 128).unwrap_err().code,
            "IncompatibleContract"
        );
    }
}

/// Borrowed bytes, valid only during the receiving function call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    /// Start of the borrowed span. May be null only when `len` is zero.
    pub ptr: *const u8,
    /// Length of the borrowed span in bytes.
    pub len: usize,
}
impl Bytes {
    /// Borrow a byte span for one synchronous call. Nothing is copied, so the
    /// span has to stay readable for as long as the callee needs it.
    pub fn new(bytes: &[u8]) -> Self {
        Self {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
    /// # Safety
    /// The sender must keep `ptr` readable for `len` bytes throughout this call.
    pub unsafe fn decode<T: serde::de::DeserializeOwned>(self) -> Result<T, Fault> {
        if self.len != 0 && self.ptr.is_null() {
            return Err(Fault::new("InvalidInput", "abi", "null bytes"));
        }
        // SAFETY: The caller guarantees this synchronous borrowed span is readable.
        let bytes = if self.len == 0 {
            &[]
        } else {
            // SAFETY: The sender keeps this non-null span live throughout decode.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        };
        serde_json::from_slice(bytes).map_err(|e| Fault::new("InvalidInput", "abi", e.to_string()))
    }
}
/// A receiver-owned token and a callback. Accepted requests consume it exactly once.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Reply {
    /// The receiver's own token, passed back to `call` unchanged.
    pub context: usize,
    /// Delivers one value to that receiver, exactly once per accepted call.
    pub call: unsafe extern "C" fn(usize, Bytes),
}
impl Reply {
    /// # Safety
    /// `context` must belong to this callback and remain live until this single invocation.
    pub unsafe fn send<T: serde::Serialize>(self, value: &T) {
        let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
        // SAFETY: This call consumes the receiver's unique token and borrows bytes synchronously.
        unsafe {
            (self.call)(self.context, Bytes::new(&bytes));
        }
    }
}
/// Host-owned service request and event callbacks. Lifetime covers the native instance.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApi {
    /// The host's own token, passed to every callback unchanged.
    pub context: usize,
    /// Admits one service request and returns its id, or zero when admission has closed.
    pub request: unsafe extern "C" fn(usize, Bytes, Reply) -> u64,
    /// Requests cancellation of an admitted request by its id.
    pub cancel: unsafe extern "C" fn(usize, u64),
    /// Publishes one event during an operation that is still open.
    pub event: unsafe extern "C" fn(usize, Bytes),
}
/// Exact ABI function table. Instance/operation handles are owned by the exporting library.
#[repr(C)]
pub struct Api {
    /// Read and validated before any function pointer below is dereferenced.
    pub header: Header,
    /// Returns the package descriptor synchronously through `Reply`.
    pub describe: unsafe extern "C" fn(Reply),
    /// Creates one instance from the host callbacks and its configuration; zero means refusal.
    pub create: unsafe extern "C" fn(HostApi, Bytes, Reply) -> usize,
    /// Starts one operation on an instance and returns its id.
    pub start: unsafe extern "C" fn(usize, Bytes, Reply) -> u64,
    /// Requests cancellation of an operation by its id.
    pub cancel: unsafe extern "C" fn(usize, u64),
    /// Releases one completed operation's resources.
    pub release: unsafe extern "C" fn(usize, u64),
    /// Finalizes an instance and lets the library unload.
    pub destroy: unsafe extern "C" fn(usize),
}
