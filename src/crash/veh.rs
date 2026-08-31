//! A vectored exception handler, so the hardware faults the panic hook cannot
//! see leave a record behind. Windows only.
//!
//! `crate::crash` reports Rust panics. An access violation, an illegal
//! instruction or a stack overflow is not a panic: the process dies with
//! nothing sent, which is precisely how a plugin can kill a DAW repeatedly and
//! produce an empty Sentry project. This handler cannot *report* — a crashing
//! process is no place for an HTTPS request — so it writes a short record next
//! to the session marker and lets the next process report it
//! ([`crate::session_marker`]).
//!
//! ## Why this is written so defensively
//!
//! A vectored handler is **process-global**. Ours is called for every exception
//! raised anywhere in the host, including ones the host and other plugins raise
//! and handle perfectly normally — a C++ `throw` is an exception, and some
//! libraries fault on guard pages deliberately. Misbehaving here does not look
//! like our bug; it looks like the DAW's. So:
//!
//! - `AddVectoredExceptionHandler`, never `SetUnhandledExceptionFilter`. The
//!   latter is a single global slot that the host may already own, and taking
//!   it would silently disable the host's own crash reporting.
//! - Three filters before we touch anything: a fatal-code allowlist, an
//!   "is the faulting address inside our own image" test, and a re-entrancy
//!   guard. Everything else returns immediately.
//! - The handler allocates nothing, takes no lock, and calls no CRT function.
//!   It formats into a stack buffer and makes one `WriteFile` call on a handle
//!   opened at registration.
//! - It **always** returns `EXCEPTION_CONTINUE_SEARCH`, so the host's own
//!   exception handling proceeds exactly as it would have.
//! - [`uninstall`] runs from `Reporter::drop`, i.e. before the last plugin
//!   instance goes away and long before the DLL can be unmapped. **A handler
//!   left registered across an unload means the OS calls into freed memory on
//!   the next exception anywhere in the process** — that is the one mistake
//!   here that would be worse than the bug this module exists to find.
//!
//! ## Two things it deliberately cannot do
//!
//! A fault *inside the graphics driver* called from our code has its faulting
//! address in the driver, not in us, so the ownership filter rejects it. And
//! `EXCEPTION_STACK_OVERFLOW` arrives with almost no stack left, so the write
//! may not complete. Both are accepted: the session marker still records the
//! stage, which is the coarse answer, and this module adds the fine one when it
//! can.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_IN_PAGE_ERROR, EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_STACK_OVERFLOW, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, WriteFile, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, RemoveVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};

/// The faults worth recording. Everything absent from this list is either
/// routine control flow somewhere else in the process (`0xE06D7363` is a C++
/// `throw`, `0x406D1388` is a thread being named, `0x40010006` is
/// `OutputDebugString`) or a debugger breakpoint — none of which is our crash,
/// and all of which are common enough that reacting to them would be a
/// performance bug in someone else's host.
const FATAL_CODES: [NTSTATUS; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_IN_PAGE_ERROR,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_STACK_OVERFLOW,
];

/// Half-open address range of this DLL's loaded image, resolved at [`install`].
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);
static IMAGE_END: AtomicUsize = AtomicUsize::new(0);
/// The fault file, opened for append at [`install`] so the handler never has to
/// open one. `INVALID_HANDLE_VALUE as isize` when closed.
static FAULT_FILE: AtomicIsize = AtomicIsize::new(-1);
/// Set for the duration of a recorded fault. A fault raised *inside* the
/// handler therefore finds it set and returns immediately instead of looping;
/// a clean pass clears it, so one benign event cannot disarm us for good.
static IN_HANDLER: AtomicBool = AtomicBool::new(false);
/// The registration cookie, as a `usize` because a raw pointer is not `Sync`.
static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Whether this exception code is one we record.
///
/// Pure, so the allowlist is testable without raising anything.
fn is_fatal(code: NTSTATUS) -> bool {
    FATAL_CODES.contains(&code)
}

/// Whether `addr` lies inside this DLL's image. Pure given the range, so the
/// boundary conditions are testable.
fn in_range(addr: usize, base: usize, end: usize) -> bool {
    base != 0 && end > base && addr >= base && addr < end
}

/// `SizeOfImage` from the loaded PE headers at `base`.
///
/// `VirtualQuery` gives the allocation base but only the size of one
/// protection region, which for a DLL is a single section. The header read is
/// the cheap way to the whole image, and it needs no extra import library the
/// way `GetModuleInformation` (psapi) would.
///
/// # Safety
/// `base` must be the load address of a mapped PE image.
unsafe fn size_of_image(base: usize) -> Option<usize> {
    const DOS_MAGIC: u16 = 0x5A4D; // "MZ"
    const PE_SIGNATURE: u32 = 0x0000_4550; // "PE\0\0"
    /// Signature (4) + IMAGE_FILE_HEADER (20) + OptionalHeader.SizeOfImage (0x38).
    /// The offset within the optional header is the same for PE32 and PE32+.
    const SIZE_OF_IMAGE_OFFSET: usize = 4 + 20 + 0x38;

    let ptr = base as *const u8;
    if ptr.cast::<u16>().read_unaligned() != DOS_MAGIC {
        return None;
    }
    let e_lfanew = ptr.add(0x3C).cast::<i32>().read_unaligned();
    if e_lfanew <= 0 || e_lfanew > 0x1000 {
        return None;
    }
    let nt = ptr.add(e_lfanew as usize);
    if nt.cast::<u32>().read_unaligned() != PE_SIGNATURE {
        return None;
    }
    let size = nt.add(SIZE_OF_IMAGE_OFFSET).cast::<u32>().read_unaligned();
    (size > 0).then_some(size as usize)
}

/// This DLL's image range, found by asking the OS about an address we know is
/// inside it — the address of a function in this very module.
fn image_range() -> Option<(usize, usize)> {
    let probe = image_range as *const () as *const core::ffi::c_void;
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };

    // SAFETY: `probe` is a valid code address, and `mbi` is a correctly sized,
    // correctly aligned, writable buffer of exactly the type asked for.
    let written = unsafe {
        VirtualQuery(
            probe,
            &mut mbi,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        return None;
    }
    let base = mbi.AllocationBase as usize;
    if base == 0 {
        return None;
    }
    // SAFETY: `AllocationBase` for a code address in a loaded module is that
    // module's mapped PE image.
    let size = unsafe { size_of_image(base) }?;
    Some((base, base.saturating_add(size)))
}

/// Opens the fault file for append. `FILE_APPEND_DATA` (rather than
/// `GENERIC_WRITE` plus a seek) is what makes the handler's single `WriteFile`
/// land at the end without a second call.
///
/// Note `OPEN_ALWAYS` **creates** the file, so an empty one exists from here
/// on — which is why `session_marker` treats a fault record as evidence only
/// when it is non-empty. `FILE_SHARE_DELETE` lets the marker's own teardown
/// remove it without depending on this handle having been closed first.
fn open_fault_file(path: &Path) -> Option<HANDLE> {
    use std::os::windows::ffi::OsStrExt;

    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is NUL-terminated and outlives the call; the two pointer
    // arguments we do not use are null, which the API documents as "no
    // security attributes" and "no template file".
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_APPEND_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    (handle != INVALID_HANDLE_VALUE && !handle.is_null()).then_some(handle)
}

/// Registers the handler. Idempotent, and a no-op if the image range or the
/// fault file cannot be resolved — a crash recorder that cannot tell our code
/// from anyone else's must not run at all.
pub fn install(fault_path: &Path) {
    if HANDLER.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some((base, end)) = image_range() else {
        return;
    };
    let Some(file) = open_fault_file(fault_path) else {
        return;
    };

    IMAGE_BASE.store(base, Ordering::Release);
    IMAGE_END.store(end, Ordering::Release);
    FAULT_FILE.store(file as isize, Ordering::Release);

    // `0` = append rather than prepend: every handler registered before ours,
    // by the host or another plugin, keeps seeing exceptions first.
    // SAFETY: `handler` has the required `extern "system"` signature and stays
    // valid until `uninstall` removes this registration.
    let cookie = unsafe { AddVectoredExceptionHandler(0, Some(handler)) };
    if cookie.is_null() {
        FAULT_FILE.store(-1, Ordering::Release);
        // SAFETY: `file` came from `CreateFileW` above and is not used again.
        unsafe { CloseHandle(file) };
        return;
    }
    HANDLER.store(cookie as usize, Ordering::Release);
}

/// Removes the handler and closes the fault file, in that order — a fault
/// between the two would otherwise write to a closed handle.
///
/// Must run before the DLL can be unmapped. It is called from `Reporter::drop`,
/// which runs while plugin instances still exist.
pub fn uninstall() {
    let cookie = HANDLER.swap(0, Ordering::AcqRel);
    if cookie != 0 {
        // SAFETY: `cookie` is the value `AddVectoredExceptionHandler` returned
        // and is removed exactly once — the swap above is what guarantees that.
        unsafe { RemoveVectoredExceptionHandler(cookie as *const core::ffi::c_void) };
    }
    let file = FAULT_FILE.swap(-1, Ordering::AcqRel);
    if file != -1 {
        // SAFETY: the handler can no longer be running with this handle: it was
        // unregistered above, and the swap means only one caller sees it.
        unsafe { CloseHandle(file as HANDLE) };
    }
}

/// Called by the OS for every exception in the process. Everything in here is
/// on the "no allocation, no locks, no CRT" budget described in the module
/// docs, and every path returns `EXCEPTION_CONTINUE_SEARCH`.
unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let record = (*info).ExceptionRecord;
    if record.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    // Filters first, cheapest and least contended first, so a host throwing
    // C++ exceptions in a loop pays two loads and a comparison.
    let code = (*record).ExceptionCode;
    if !is_fatal(code) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let addr = (*record).ExceptionAddress as usize;
    let base = IMAGE_BASE.load(Ordering::Acquire);
    if !in_range(addr, base, IMAGE_END.load(Ordering::Acquire)) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if IN_HANDLER.swap(true, Ordering::AcqRel) {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    write_record(code, addr - base);

    IN_HANDLER.store(false, Ordering::Release);
    EXCEPTION_CONTINUE_SEARCH
}

/// Formats and writes one record. The offset is relative to the image base,
/// which is what makes it comparable against an uploaded PDB — the absolute
/// address depends on where the loader happened to put us.
fn write_record(code: NTSTATUS, offset: usize) {
    let file = FAULT_FILE.load(Ordering::Acquire);
    if file == -1 {
        return;
    }

    // Fixed capacity, filled by hand: `format!` allocates, and this runs in a
    // process that has already faulted once.
    let mut buf = [0u8; 96];
    let mut len = 0;
    let mut push = |bytes: &[u8], len: &mut usize| {
        for &b in bytes {
            if *len < buf.len() {
                buf[*len] = b;
                *len += 1;
            }
        }
    };
    push(b"exception\tcode=0x", &mut len);
    push(&hex(code as u32), &mut len);
    push(b" offset=0x", &mut len);
    push(&hex(offset as u32), &mut len);
    push(
        if super::in_plugin_code() {
            b" scope=1\n"
        } else {
            b" scope=0\n"
        },
        &mut len,
    );

    let mut written = 0u32;
    // SAFETY: `buf` is a live stack buffer of at least `len` bytes, `written`
    // is a valid out-parameter, and a null OVERLAPPED means a synchronous
    // write — which is what `FILE_APPEND_DATA` needs to land at the end.
    unsafe {
        WriteFile(
            file as HANDLE,
            buf.as_ptr(),
            len as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        FlushFileBuffers(file as HANDLE);
    }
}

/// Eight zero-padded lowercase hex digits, allocation-free.
///
/// Exactly `u32` wide, and both callers are: an `NTSTATUS` is 32 bits, and an
/// offset into our image is bounded by `SizeOfImage`, which is also a `u32`.
/// Returning a fixed-width array is what lets the caller push it whole.
fn hex(value: u32) -> [u8; 8] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [b'0'; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = DIGITS[((value >> (4 * (7 - i))) & 0xF) as usize];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowlist must contain the faults that kill a process and none of
    /// the codes hosts raise as ordinary control flow. `0xE06D7363` is the one
    /// that matters most: a C++ `throw`, which a DAW may do thousands of times
    /// a second.
    #[test]
    fn only_fatal_codes_are_recorded() {
        assert!(is_fatal(EXCEPTION_ACCESS_VIOLATION));
        assert!(is_fatal(EXCEPTION_STACK_OVERFLOW));
        assert!(is_fatal(EXCEPTION_ILLEGAL_INSTRUCTION));

        for benign in [
            0xE06D7363_u32 as NTSTATUS, // C++ throw
            0x406D1388_u32 as NTSTATUS, // thread naming
            0x40010006_u32 as NTSTATUS, // OutputDebugString
            0x80000003_u32 as NTSTATUS, // breakpoint
            0_i32 as NTSTATUS,
        ] {
            assert!(!is_fatal(benign), "{benign:#x} must not be recorded");
        }
    }

    #[test]
    fn the_ownership_test_is_half_open_and_rejects_an_unset_range() {
        assert!(in_range(0x1000, 0x1000, 0x2000), "base is inside");
        assert!(in_range(0x1fff, 0x1000, 0x2000));
        assert!(!in_range(0x2000, 0x1000, 0x2000), "end is outside");
        assert!(!in_range(0x0fff, 0x1000, 0x2000));
        // Before `install`, or after it failed: nothing is ours.
        assert!(!in_range(0x1000, 0, 0));
        assert!(!in_range(0x1000, 0x2000, 0x1000), "inverted range");
    }

    /// The handler reports an offset, not an address, so a record can be lined
    /// up against an uploaded PDB whatever the loader chose.
    #[test]
    fn our_own_image_range_resolves_and_contains_our_code() {
        let (base, end) = image_range().expect("this test is running from a loaded PE image");
        assert!(end > base);
        assert!(in_range(
            our_own_image_range_resolves_and_contains_our_code as *const () as usize,
            base,
            end
        ));
    }

    /// The record is fixed-width and zero-padded — a stray padding digit would
    /// silently corrupt the code or offset the sweep reads back.
    #[test]
    fn hex_is_exactly_eight_zero_padded_digits() {
        assert_eq!(&hex(0xC000_0005), b"c0000005");
        assert_eq!(&hex(0x1234), b"00001234");
        assert_eq!(&hex(0), b"00000000");
        assert_eq!(&hex(u32::MAX), b"ffffffff");
    }

    /// The whole record, end to end, at the width the sweep expects.
    #[test]
    fn a_record_is_one_line_naming_the_code_and_the_offset() {
        let mut buf = [0u8; 96];
        let mut len = 0;
        {
            let mut push = |bytes: &[u8], len: &mut usize| {
                for &b in bytes {
                    if *len < buf.len() {
                        buf[*len] = b;
                        *len += 1;
                    }
                }
            };
            push(b"exception\tcode=0x", &mut len);
            push(&hex(EXCEPTION_ACCESS_VIOLATION as u32), &mut len);
            push(b" offset=0x", &mut len);
            push(&hex(0x1234), &mut len);
            push(b" scope=1\n", &mut len);
        }
        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            "exception\tcode=0xc0000005 offset=0x00001234 scope=1\n"
        );
    }
}
