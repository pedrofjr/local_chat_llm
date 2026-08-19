//! Thin Windows glue: DPAPI-protected secrets and clipboard access.
//!
//! Hand-rolled FFI on purpose. Pulling `windows-sys` in for six calls would
//! cost more binary than the whole feature is worth, and the release profile
//! targets a single small exe.

/// What came off the clipboard.
pub enum Grab {
    /// Already a picture file: PNG from a browser, or a file copied in
    /// Explorer. Goes straight to the decoder.
    Encoded(Vec<u8>),
    /// Raw pixels from a device-independent bitmap, which is what the snipping
    /// tool leaves. Somebody else has to encode these.
    Raw { w: u32, h: u32, rgba: Vec<u8> },
}

#[cfg(windows)]
mod imp {
    use super::Grab;
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
        fn GlobalSize(mem: *mut c_void) -> usize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
        fn CloseClipboard() -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn DragQueryFileW(drop: *mut c_void, index: u32, buf: *mut u16, cch: u32) -> u32;
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

    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;

    /// Guard so every path out of a clipboard read closes it. Leaving the
    /// clipboard open locks it for every other program on the desktop.
    struct Clipboard;

    impl Clipboard {
        fn open() -> Option<Self> {
            // Another program may hold it for a moment; a few tries is the
            // conventional answer and cheaper than failing in the user's face.
            for _ in 0..5 {
                if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                    return Some(Clipboard);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    /// Reads a clipboard handle's bytes. The handle belongs to the clipboard,
    /// so it is locked and unlocked but never freed.
    fn handle_bytes(format: u32) -> Option<Vec<u8>> {
        unsafe {
            if IsClipboardFormatAvailable(format) == 0 {
                return None;
            }
            let mem = GetClipboardData(format);
            if mem.is_null() {
                return None;
            }
            let size = GlobalSize(mem);
            if size == 0 {
                return None;
            }
            let src = GlobalLock(mem);
            if src.is_null() {
                return None;
            }
            let out = std::slice::from_raw_parts(src as *const u8, size).to_vec();
            GlobalUnlock(mem);
            Some(out)
        }
    }

    fn registered(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    }

    /// First path from a CF_HDROP, which is what copying a file in Explorer
    /// leaves behind.
    fn dropped_path() -> Option<std::path::PathBuf> {
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                return None;
            }
            let drop = GetClipboardData(CF_HDROP);
            if drop.is_null() {
                return None;
            }
            // 0xFFFF_FFFF asks for the count rather than a path.
            if DragQueryFileW(drop, 0xFFFF_FFFF, ptr::null_mut(), 0) == 0 {
                return None;
            }
            let needed = DragQueryFileW(drop, 0, ptr::null_mut(), 0);
            if needed == 0 {
                return None;
            }
            // The returned length excludes the terminator.
            let mut buf = vec![0u16; needed as usize + 1];
            let wrote = DragQueryFileW(drop, 0, buf.as_mut_ptr(), buf.len() as u32);
            if wrote == 0 {
                return None;
            }
            buf.truncate(wrote as usize);
            Some(std::path::PathBuf::from(String::from_utf16_lossy(&buf)))
        }
    }

    /// Whether there is a picture to paste, without copying it out. Cheap
    /// enough to ask on every Ctrl+V.
    pub fn has_image() -> bool {
        let Some(_guard) = Clipboard::open() else {
            return false;
        };
        let png = registered("PNG");
        unsafe {
            if png != 0 && IsClipboardFormatAvailable(png) != 0 {
                return true;
            }
            if IsClipboardFormatAvailable(CF_DIB) != 0 {
                return true;
            }
        }
        // A copied file only counts if it looks like a picture; otherwise
        // Ctrl+V after copying a spreadsheet would try to send it.
        dropped_path().is_some_and(|path| {
            matches!(
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("png" | "jpg" | "jpeg" | "gif")
            )
        })
    }

    /// Pulls a picture off the clipboard, preferring formats that need no
    /// conversion.
    pub fn grab_image() -> Option<Grab> {
        let _guard = Clipboard::open()?;

        // Browsers and image editors offer a real PNG, which is both smaller
        // and lossless compared to going through a bitmap.
        let png = registered("PNG");
        if png != 0 {
            if let Some(bytes) = handle_bytes(png) {
                return Some(Grab::Encoded(bytes));
            }
        }

        // A file copied in Explorer: read it from disk rather than guessing.
        if let Some(path) = dropped_path() {
            if let Ok(bytes) = std::fs::read(&path) {
                return Some(Grab::Encoded(bytes));
            }
        }

        let dib = handle_bytes(CF_DIB)?;
        dib_to_rgba(&dib)
    }

    /// Turns a packed DIB into straight RGBA.
    ///
    /// Three things here are easy to get wrong and all of them are silent: a
    /// positive height means the rows are stored bottom-up, each row is padded
    /// to a multiple of four bytes, and 32-bit bitmaps from the snipping tool
    /// routinely carry an all-zero alpha channel that would render the whole
    /// picture invisible if taken at face value.
    fn dib_to_rgba(dib: &[u8]) -> Option<Grab> {
        // BITMAPINFOHEADER is 40 bytes; a V5 header is longer but starts the
        // same way and declares its own size.
        if dib.len() < 40 {
            return None;
        }
        let u32_at = |off: usize| -> u32 {
            u32::from_le_bytes([dib[off], dib[off + 1], dib[off + 2], dib[off + 3]])
        };
        let i32_at = |off: usize| -> i32 { u32_at(off) as i32 };

        let header = u32_at(0) as usize;
        if header < 40 || header > dib.len() {
            return None;
        }
        let width = i32_at(4);
        let raw_height = i32_at(8);
        let bits = u16::from_le_bytes([dib[14], dib[15]]);
        let compression = u32_at(16);
        let palette_entries = u32_at(32);

        // Only uncompressed 24/32-bit bitmaps. BI_BITFIELDS (3) stores three
        // masks after the header and is what 32-bit captures normally use.
        if !matches!(compression, 0 | 3) || !matches!(bits, 24 | 32) {
            return None;
        }
        if width <= 0 || raw_height == 0 {
            return None;
        }
        let bottom_up = raw_height > 0;
        let height = raw_height.unsigned_abs();
        let width_u = width as u32;
        // Same ceiling media.rs uses, applied before allocating.
        if u64::from(width_u) * u64::from(height) > 50_000_000 {
            return None;
        }

        let masks = if compression == 3 { 12 } else { 0 };
        let palette = if bits <= 8 {
            palette_entries as usize * 4
        } else {
            0
        };
        let start = header + masks + palette;
        // Each row is padded out to a multiple of four bytes. Checked rather
        // than relying on the pixel ceiling above to keep this in range: the
        // two limits are unrelated, and a silent wrap here would compute a
        // short stride and read the picture sideways.
        let stride = (width_u as usize)
            .checked_mul(usize::from(bits))?
            .div_ceil(32)
            .checked_mul(4)?;
        let needed = start.checked_add(stride.checked_mul(height as usize)?)?;
        if dib.len() < needed {
            return None;
        }

        let per_px = (bits / 8) as usize;
        let mut rgba = vec![0u8; (width_u as usize) * (height as usize) * 4];
        let mut any_alpha = false;
        for y in 0..height as usize {
            // Bottom-up bitmaps store the last row first.
            let src_row = if bottom_up {
                height as usize - 1 - y
            } else {
                y
            };
            let row = start + src_row * stride;
            for x in 0..width_u as usize {
                let src = row + x * per_px;
                let dst = (y * width_u as usize + x) * 4;
                // DIBs are BGR(A), not RGB(A).
                rgba[dst] = dib[src + 2];
                rgba[dst + 1] = dib[src + 1];
                rgba[dst + 2] = dib[src];
                let a = if per_px == 4 { dib[src + 3] } else { 255 };
                any_alpha |= a != 0;
                rgba[dst + 3] = a;
            }
        }
        // An all-zero alpha channel means the bitmap simply does not use one.
        // Trusting it would produce a fully transparent picture.
        if !any_alpha {
            for px in rgba.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        Some(Grab::Raw {
            w: width_u,
            h: height,
            rgba,
        })
    }

    #[cfg(test)]
    mod dib {
        use super::*;

        /// Builds a BITMAPINFOHEADER plus rows exactly as Windows packs them.
        fn dib(w: i32, h: i32, bits: u16, rows: &[Vec<u8>]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&40u32.to_le_bytes()); // biSize
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
            out.extend_from_slice(&bits.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
            out.extend_from_slice(&[0u8; 20]); // size, ppm x2, clrUsed, clrImportant
            let stride = (w.unsigned_abs() as usize)
                .saturating_mul(usize::from(bits))
                .div_ceil(32)
                .saturating_mul(4);
            for row in rows {
                let mut padded = row.clone();
                padded.resize(stride, 0);
                out.extend_from_slice(&padded);
            }
            out
        }

        fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
            let at = ((y * w + x) * 4) as usize;
            [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
        }

        /// A positive height means the rows are stored bottom-up. Reading them
        /// in order flips every screenshot upside down.
        #[test]
        fn a_bottom_up_bitmap_is_turned_the_right_way_up() {
            // Wanted: red green on the top row, blue white underneath.
            // Stored bottom-up, so the blue/white row comes first, and each
            // pixel is BGRA with the alpha byte left at zero.
            let bottom = vec![255, 0, 0, 0, 255, 255, 255, 0];
            let top = vec![0, 0, 255, 0, 0, 255, 0, 0];
            let bytes = dib(2, 2, 32, &[bottom, top]);

            let Some(Grab::Raw { w, h, rgba }) = dib_to_rgba(&bytes) else {
                panic!("should have decoded");
            };
            assert_eq!((w, h), (2, 2));
            assert_eq!(px(&rgba, w, 0, 0), [255, 0, 0, 255], "top-left red");
            assert_eq!(px(&rgba, w, 1, 0), [0, 255, 0, 255], "top-right green");
            assert_eq!(px(&rgba, w, 0, 1), [0, 0, 255, 255], "bottom-left blue");
            assert_eq!(px(&rgba, w, 1, 1), [255, 255, 255, 255], "bottom-right white");
        }

        /// An all-zero alpha channel means the bitmap does not use one. Taking
        /// it literally makes the whole picture invisible -- and the snipping
        /// tool produces exactly this.
        #[test]
        fn an_unused_alpha_channel_is_read_as_opaque() {
            let row = vec![10, 20, 30, 0];
            let bytes = dib(1, 1, 32, &[row]);
            let Some(Grab::Raw { rgba, .. }) = dib_to_rgba(&bytes) else {
                panic!("should have decoded");
            };
            assert_eq!(rgba[3], 255, "a picture must not come out transparent");

            // A bitmap that really does use alpha keeps what it says.
            let row = vec![10, 20, 30, 128];
            let bytes = dib(1, 1, 32, &[row]);
            let Some(Grab::Raw { rgba, .. }) = dib_to_rgba(&bytes) else {
                panic!("should have decoded");
            };
            assert_eq!(rgba[3], 128, "real transparency has to survive");
        }

        /// Rows are padded to a multiple of four bytes. A 24-bit row of three
        /// pixels is nine bytes of colour and three of padding; walking rows
        /// without that skews the picture diagonally.
        #[test]
        fn row_padding_is_accounted_for() {
            let top = vec![1, 1, 1, 2, 2, 2, 3, 3, 3];
            let bottom = vec![4, 4, 4, 5, 5, 5, 6, 6, 6];
            // Negative height: stored top-down, so no flip.
            let bytes = dib(3, -2, 24, &[top, bottom]);
            let Some(Grab::Raw { w, h, rgba }) = dib_to_rgba(&bytes) else {
                panic!("should have decoded");
            };
            assert_eq!((w, h), (3, 2));
            assert_eq!(px(&rgba, w, 0, 0), [1, 1, 1, 255]);
            assert_eq!(px(&rgba, w, 2, 0), [3, 3, 3, 255], "last pixel of row 0");
            assert_eq!(px(&rgba, w, 0, 1), [4, 4, 4, 255], "row 1 starts after padding");
            assert_eq!(px(&rgba, w, 2, 1), [6, 6, 6, 255]);
        }

        /// Reads whatever picture is actually on this machine's clipboard.
        ///
        /// Ignored by default: it touches global desktop state and needs a
        /// human to have copied something. Run it after a Win+Shift+S with
        ///
        ///   cargo test reads_a_real_clipboard_picture -- --ignored --nocapture
        #[test]
        #[ignore]
        fn reads_a_real_clipboard_picture() {
            assert!(
                has_image(),
                "nothing picture-shaped on the clipboard -- snip something first"
            );
            match grab_image() {
                Some(Grab::Encoded(bytes)) => {
                    println!("encoded, {} bytes, header {:02x?}", bytes.len(), &bytes[..8.min(bytes.len())]);
                    assert!(crate::store::ImageKind::sniff(&bytes).is_some(), "unknown format");
                }
                Some(Grab::Raw { w, h, rgba }) => {
                    println!("bitmap {w}x{h}, {} bytes of pixels", rgba.len());
                    let at = |x: u32, y: u32| {
                        let i = ((y * w + x) * 4) as usize;
                        [rgba[i], rgba[i + 1], rgba[i + 2]]
                    };
                    println!(
                        "corners  TL {:?}  TR {:?}  BL {:?}  BR {:?}",
                        at(0, 0),
                        at(w - 1, 0),
                        at(0, h - 1),
                        at(w - 1, h - 1)
                    );
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    assert!(
                        rgba.chunks_exact(4).any(|px| px[3] != 0),
                        "every pixel came out transparent"
                    );
                    assert!(
                        rgba.chunks_exact(4).any(|px| px[..3] != [0, 0, 0]),
                        "the whole bitmap came out black -- byte order is wrong"
                    );
                }
                None => panic!("has_image said yes but grab_image gave nothing"),
            }
        }

        /// Whatever a peer or another program put on the clipboard is not to
        /// be trusted to be well formed.
        #[test]
        fn malformed_bitmaps_are_refused_rather_than_panicking() {
            assert!(dib_to_rgba(&[]).is_none(), "empty");
            assert!(dib_to_rgba(&[0u8; 39]).is_none(), "shorter than a header");

            // Declares two rows and supplies none.
            let mut truncated = dib(4, 2, 32, &[]);
            truncated.truncate(40);
            assert!(dib_to_rgba(&truncated).is_none(), "pixels missing");

            // Sizes that would overflow the allocation.
            let huge = dib(i32::MAX, i32::MAX, 32, &[]);
            assert!(dib_to_rgba(&huge).is_none(), "absurd dimensions");

            // Formats we do not decode.
            assert!(dib_to_rgba(&dib(1, 1, 8, &[vec![0]])).is_none(), "8bpp");
            assert!(dib_to_rgba(&dib(0, 1, 32, &[vec![0; 4]])).is_none(), "zero width");
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

    pub fn grab_image() -> Option<super::Grab> {
        None
    }

    pub fn has_image() -> bool {
        false
    }
}

pub use imp::{copy, grab_image, has_image, protect, unprotect};

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
