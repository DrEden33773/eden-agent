//! Stable-layout header inspection before accessing the exact-version function table.
use eden_protocol::Fault;
/// Header always read before the remainder of a plugin table.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Header {
    pub magic: [u8; 8],
    pub abi: u32,
    pub size: u32,
    pub sdk: [u8; 32],
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
    pub ptr: *const u8,
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
    pub context: usize,
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
    pub context: usize,
    pub request: unsafe extern "C" fn(usize, Bytes, Reply) -> u64,
    pub cancel: unsafe extern "C" fn(usize, u64),
    pub event: unsafe extern "C" fn(usize, Bytes),
}
/// Exact ABI function table. Instance/operation handles are owned by the exporting library.
#[repr(C)]
pub struct Api {
    pub header: Header,
    pub describe: unsafe extern "C" fn(Reply),
    pub create: unsafe extern "C" fn(HostApi, Bytes, Reply) -> usize,
    pub start: unsafe extern "C" fn(usize, Bytes, Reply) -> u64,
    pub cancel: unsafe extern "C" fn(usize, u64),
    pub release: unsafe extern "C" fn(usize, u64),
    pub destroy: unsafe extern "C" fn(usize),
}
