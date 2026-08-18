//! Thin Windows glue: DPAPI-protected secrets and clipboard access.
//!
//! Hand-rolled FFI on purpose. Pulling `windows-sys` in for six calls would
//! cost more binary than the whole feature is worth, and the release profile
//! targets a single small exe.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr;

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[repr(C)]
    struct DataBlob {
        cb: u32,
        pb: *mut u8,
    }

    impl DataBlob {
        fn borrowed(bytes: &[u8]) -> Self {
            Self {
                cb: bytes.len() as u32,
                pb: bytes.as_ptr() as *mut u8,
            }
        }

        fn empty() -> Self {
            Self {
                cb: 0,
                pb: ptr::null_mut(),
            }
        }
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            descr: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;

        fn CryptUnprotectData(
            data_in: *const DataBlob,
            descr: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }

    /// Copies the blob out of the buffer DPAPI allocated, then releases it.
    fn drain(blob: &DataBlob) -> Vec<u8> {
        if blob.pb.is_null() || blob.cb == 0 {
            return Vec::new();
        }
        let out = unsafe { std::slice::from_raw_parts(blob.pb, blob.cb as usize) }.to_vec();
        unsafe { LocalFree(blob.pb as *mut c_void) };
        out
    }

    pub fn protect(secret: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
        let input = DataBlob::borrowed(secret);
        let salt = DataBlob::borrowed(entropy);
        let mut out = DataBlob::empty();
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &salt,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        (ok != 0).then(|| drain(&out))
    }

    pub fn unprotect(sealed: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
        let input = DataBlob::borrowed(sealed);
        let salt = DataBlob::borrowed(entropy);
        let mut out = DataBlob::empty();
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &salt,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        (ok != 0).then(|| drain(&out))
    }

    pub fn copy(text: &str) -> bool {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        unsafe {
            if OpenClipboard(ptr::null_mut()) == 0 {
                return false;
            }
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                CloseClipboard();
                return false;
            }
            let dst = GlobalLock(mem);
            if dst.is_null() {
                GlobalFree(mem);
                CloseClipboard();
                return false;
            }
            ptr::copy_nonoverlapping(wide.as_ptr(), dst as *mut u16, wide.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            // On success the clipboard owns the handle; freeing it would be a
            // double free.
            let placed = SetClipboardData(CF_UNICODETEXT, mem);
            CloseClipboard();
            if placed.is_null() {
                GlobalFree(mem);
                return false;
            }
            true
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn protect(_secret: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn unprotect(_sealed: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn copy(_text: &str) -> bool {
        false
    }
}

pub use imp::{copy, protect, unprotect};

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip() {
        let sealed = protect(b"7K2M9QXP", b"topic").expect("dpapi protect");
        assert_ne!(sealed.as_slice(), b"7K2M9QXP");
        assert_eq!(unprotect(&sealed, b"topic").as_deref(), Some(&b"7K2M9QXP"[..]));
    }

    #[test]
    fn dpapi_rejects_wrong_entropy() {
        let sealed = protect(b"7K2M9QXP", b"topic-a").expect("dpapi protect");
        assert!(unprotect(&sealed, b"topic-b").is_none());
    }
}
