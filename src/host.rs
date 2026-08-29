//! What the plugin is running inside: the DAW, its version, and the OS
//! version. Resolved once per process and cached in a `OnceLock`, because
//! none of it can change while the process lives.
//!
//! Three rules shape this module:
//!
//! 1. **Nothing here may panic.** It is reached from `initialize()`, inside a
//!    `crash::scope()` and inside the host's `extern "C"` activation frame —
//!    an unwind out of that aborts the DAW. Every fallible step returns
//!    `Option`, and the CoreFoundation work uses the raw `-sys` externs with
//!    hand-written null checks rather than the safe `core-foundation`
//!    wrappers, whose `wrap_under_get_rule` asserts on a NULL reference (a
//!    host with no main bundle — `auval`, a CLI validator — would abort).
//! 2. **The DAW is reported as an allowlisted label, never as a raw path or
//!    an arbitrary file name.** `current_exe()` can contain the user's home
//!    directory (a portable REAPER install, an app in `~/Applications`), and
//!    even the bare stem of an unrecognized host is a fingerprint. Anything
//!    off the list reports [`UNKNOWN_DAW`] and carries no version, because an
//!    unknown executable *plus* its version identifies far more than either
//!    alone.
//! 3. **The labels are wire values.** They land in Mixpanel as the `daw`
//!    property; renaming one splits its history in two. Add freely, rename
//!    never.
//!
//! Why `current_exe()` rather than the host name the plugin API offers: nih-plug
//! exposes neither CLAP's `clap_host.name` nor VST3's `IHostApplication::getName`,
//! and reaching them would mean a fourth local patch on the vendored tree. It
//! would also be *wrong* for AudioUnit — on that path clap-wrapper is the CLAP
//! host, so `clap_host.name` names the wrapper, not Logic. The host process's
//! own executable is the same answer in all three formats.

use std::sync::OnceLock;

/// Reported when the executable is not one we recognize. Deliberately not
/// "unknown": this is a bucket, not a missing value.
pub const UNKNOWN_DAW: &str = "other";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostInfo {
    /// An allowlisted label, or [`UNKNOWN_DAW`].
    pub daw: &'static str,
    /// `None` for an unrecognized host (rule 2), and whenever the platform
    /// lookup fails.
    pub daw_version: Option<String>,
    /// The running OS version — `"26.3.1"`, `"10.0.26100"` — as opposed to the
    /// `os` property, which is the *build target* and so cannot move.
    pub os_version: Option<String>,
}

/// Resolved on first call, then cached for the life of the process.
pub fn info() -> &'static HostInfo {
    static INFO: OnceLock<HostInfo> = OnceLock::new();
    INFO.get_or_init(resolve)
}

fn resolve() -> HostInfo {
    // The plugin is a dylib inside the host's process, so this is the DAW's
    // own executable on every platform and in every plugin format.
    let daw = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(|path| path.file_stem())
        .and_then(|stem| stem.to_str())
        .map(daw_label)
        .unwrap_or(UNKNOWN_DAW);

    HostInfo {
        daw,
        daw_version: if daw == UNKNOWN_DAW {
            None
        } else {
            daw_version()
        },
        os_version: os_version(),
    }
}

/// Maps an executable's file stem to a stable label. Pure, so the whole
/// allowlist is unit-testable without a DAW.
///
/// Several hosts version their executable name (`Cubase 14`, `FL64`,
/// `Adobe Audition 2025`, `Ableton Live 12 Suite`), so most entries are
/// substring matches. The stems short enough for a substring match to catch
/// something unrelated are matched exactly instead.
pub fn daw_label(stem: &str) -> &'static str {
    let stem = stem.trim().to_ascii_lowercase();

    match stem.as_str() {
        // macOS: Ableton's executable is bare `Live` inside the .app.
        "live" => return "Ableton Live",
        "fl" | "fl32" | "fl64" => return "FL Studio",
        "resolve" => return "DaVinci Resolve",
        "luna" => return "LUNA",
        // Not DAWs, but they load the plugin and they run on a machine that
        // may well have consented — labelling them is what lets them be
        // filtered out of the real usage figures instead of inflating them.
        "auval" => return "auval",
        "pluginval" => return "pluginval",
        "clap-validator" => return "clap-validator",
        "standalone" | "gui_preview" => return "ConjureAlign dev",
        _ => {}
    }

    const NEEDLES: &[(&str, &str)] = &[
        ("ableton live", "Ableton Live"),
        ("logic pro", "Logic Pro"),
        ("mainstage", "MainStage"),
        ("garageband", "GarageBand"),
        ("final cut", "Final Cut Pro"),
        ("reaper", "REAPER"),
        ("cubase", "Cubase"),
        ("nuendo", "Nuendo"),
        ("wavelab", "WaveLab"),
        ("studio one", "Studio One"),
        ("bitwig", "Bitwig Studio"),
        ("pro tools", "Pro Tools"),
        ("protools", "Pro Tools"),
        ("fl studio", "FL Studio"),
        ("digital performer", "Digital Performer"),
        ("reason", "Reason"),
        ("mixbus", "Mixbus"),
        ("ardour", "Ardour"),
        ("audacity", "Audacity"),
        ("cakewalk", "Cakewalk"),
        ("sonar", "Cakewalk"),
        ("samplitude", "Samplitude"),
        ("sequoia", "Sequoia"),
        ("audition", "Adobe Audition"),
        ("premiere", "Adobe Premiere Pro"),
        ("after effects", "Adobe After Effects"),
        ("davinci", "DaVinci Resolve"),
        ("waveform", "Waveform"),
        ("tracktion", "Waveform"),
        ("renoise", "Renoise"),
        ("gig performer", "Gig Performer"),
        ("cantabile", "Cantabile"),
        ("mixcraft", "Mixcraft"),
        ("vegas", "VEGAS Pro"),
        ("sound forge", "Sound Forge"),
        ("soundforge", "Sound Forge"),
        ("n-track", "n-Track Studio"),
        ("carla", "Carla"),
        ("qtractor", "Qtractor"),
        ("clap-host", "clap-host"),
    ];

    for (needle, label) in NEEDLES {
        if stem.contains(needle) {
            return label;
        }
    }
    UNKNOWN_DAW
}

// ---------------------------------------------------------------------------
// DAW version
// ---------------------------------------------------------------------------

/// `CFBundleShortVersionString` from the host application's bundle — the
/// marketing version ("11.1.2"), not the build number.
#[cfg(target_os = "macos")]
fn daw_version() -> Option<String> {
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::bundle::{
        CFBundleGetMainBundle, CFBundleGetValueForInfoDictionaryKey,
    };
    use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithBytes};

    const KEY: &[u8] = b"CFBundleShortVersionString";

    // SAFETY: `CFBundleGetMainBundle` and `CFBundleGetValueForInfoDictionaryKey`
    // are both Get-rule (no ownership transferred, nothing to release), and
    // both may return NULL — a process with no main bundle, or a plist without
    // the key — so both results are checked before use. The key string is the
    // one Create-rule value here and is released on every exit from the block.
    unsafe {
        let bundle = CFBundleGetMainBundle();
        if bundle.is_null() {
            return None;
        }
        let key = CFStringCreateWithBytes(
            std::ptr::null(),
            KEY.as_ptr(),
            KEY.len() as isize,
            kCFStringEncodingUTF8,
            0,
        );
        if key.is_null() {
            return None;
        }
        let value = CFBundleGetValueForInfoDictionaryKey(bundle, key);
        let version = cf_string_to_owned(value);
        CFRelease(key as CFTypeRef);
        version
    }
}

/// Reads a `CFTypeRef` as a String, or `None` if it is NULL or is not a
/// CFString. The type check is not paranoia: the value comes from the host's
/// Info.plist, which can hold any plist type under that key.
///
/// # Safety
/// `value` must be NULL or a valid CFTypeRef owned by the caller for the
/// duration of the call.
#[cfg(target_os = "macos")]
unsafe fn cf_string_to_owned(value: core_foundation_sys::base::CFTypeRef) -> Option<String> {
    use core_foundation_sys::base::CFGetTypeID;
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringGetCString, CFStringGetTypeID, CFStringRef,
    };

    if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    // Version strings are a handful of characters; anything longer is not one,
    // and a fixed buffer keeps this allocation-free and unable to fail.
    let mut buf = [0 as std::os::raw::c_char; 64];
    let ok = CFStringGetCString(
        value as CFStringRef,
        buf.as_mut_ptr(),
        buf.len() as isize,
        kCFStringEncodingUTF8,
    );
    if ok == 0 {
        return None;
    }
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

/// The `VS_FIXEDFILEINFO` file version of the host executable, as
/// `major.minor.build.revision`.
///
/// Deliberately the fixed-format block rather than the localized
/// `StringFileInfo`: it needs no language/codepage lookup, and it is the
/// version the installer stamped rather than a translatable display string.
#[cfg(target_os = "windows")]
fn daw_version() -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    /// `VS_FIXEDFILEINFO::dwSignature` for a well-formed block.
    const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;

    let exe = std::env::current_exe().ok()?;
    let path: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // `\` asks VerQueryValue for the root (fixed-info) block.
    let root: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: `path` and `root` are NUL-terminated for the whole call. The
    // version block is sized by `GetFileVersionInfoSizeW` and only read after
    // `GetFileVersionInfoW` reports success. `VerQueryValueW` hands back a
    // pointer INTO `buf` — which outlives the read below — and its length is
    // checked against the struct before the cast, so a truncated or foreign
    // block cannot be read out of bounds.
    unsafe {
        let mut ignored_handle = 0u32;
        let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored_handle);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, buf.as_mut_ptr().cast()) == 0 {
            return None;
        }

        let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut block_len = 0u32;
        if VerQueryValueW(
            buf.as_ptr().cast(),
            root.as_ptr(),
            &mut block,
            &mut block_len,
        ) == 0
            || block.is_null()
            || (block_len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }

        let info = &*(block as *const VS_FIXEDFILEINFO);
        if info.dwSignature != VS_FFI_SIGNATURE {
            return None;
        }
        Some(format!(
            "{}.{}.{}.{}",
            info.dwFileVersionMS >> 16,
            info.dwFileVersionMS & 0xFFFF,
            info.dwFileVersionLS >> 16,
            info.dwFileVersionLS & 0xFFFF,
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn daw_version() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// OS version
// ---------------------------------------------------------------------------

/// `kern.osproductversion` — "26.3.1". One sysctl, no process spawned.
///
/// Note this is the *host process's* view: macOS serves a capped value
/// ("10.16") to processes linked against a pre-Big-Sur SDK, so a very old DAW
/// can under-report. Accepted rather than worked around — the alternative,
/// deriving from `kern.osrelease`, stopped being possible when macOS jumped
/// from 15 to 26 without a matching Darwin jump.
#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    const NAME: &[u8] = b"kern.osproductversion\0";

    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `NAME` is NUL-terminated, and `len` is the true capacity of
    // `buf` going in and is overwritten with the byte count actually written
    // (including the NUL) on success.
    let rc = unsafe {
        libc::sysctlbyname(
            NAME.as_ptr() as *const libc::c_char,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 || len > buf.len() {
        return None;
    }
    let text = buf[..len]
        .iter()
        .take_while(|&&b| b != 0)
        .copied()
        .collect::<Vec<u8>>();
    String::from_utf8(text).ok().filter(|s| !s.is_empty())
}

/// `os_info` reads this from `RtlGetVersion`, which — unlike `GetVersionEx` —
/// does not lie to a process without a compatibility manifest. It is already
/// in the dependency graph on Windows: `sentry-contexts` uses it there (and
/// `uname` everywhere else, which is why macOS goes through sysctl above).
#[cfg(target_os = "windows")]
fn os_version() -> Option<String> {
    match os_info::get().version() {
        os_info::Version::Unknown => None,
        version => Some(version.to_string()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_version() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_map_to_stable_labels() {
        // macOS: the executable inside Contents/MacOS.
        assert_eq!(daw_label("Logic Pro"), "Logic Pro");
        assert_eq!(daw_label("Live"), "Ableton Live");
        assert_eq!(daw_label("REAPER"), "REAPER");
        assert_eq!(daw_label("BitwigStudio"), "Bitwig Studio");
        assert_eq!(daw_label("GarageBand"), "GarageBand");
        // Windows: the .exe stem, often carrying the major version.
        assert_eq!(daw_label("Ableton Live 12 Suite"), "Ableton Live");
        assert_eq!(daw_label("reaper"), "REAPER");
        assert_eq!(daw_label("Cubase14"), "Cubase");
        assert_eq!(daw_label("Nuendo 13"), "Nuendo");
        assert_eq!(daw_label("FL64"), "FL Studio");
        assert_eq!(daw_label("ProTools"), "Pro Tools");
    }

    #[test]
    fn matching_ignores_case_and_surrounding_space() {
        assert_eq!(daw_label("  logic pro  "), "Logic Pro");
        assert_eq!(daw_label("STUDIO ONE"), "Studio One");
    }

    /// Rule 2: an unrecognized executable is a bucket, never its own name —
    /// the stem can be anything the user's machine happens to run.
    #[test]
    fn unknown_hosts_do_not_leak_their_executable_name() {
        for stem in [
            "SomeInternalTool",
            "michaeljancsy-test-build",
            "",
            "a",
            "Contents",
        ] {
            assert_eq!(daw_label(stem), UNKNOWN_DAW, "leaked for {stem:?}");
        }
    }

    /// The validators run headless on a machine that may have consented, so
    /// they must be distinguishable from real usage rather than landing in
    /// the "other" bucket alongside it.
    #[test]
    fn validators_are_labelled_so_they_can_be_filtered_out() {
        assert_eq!(daw_label("auval"), "auval");
        assert_eq!(daw_label("pluginval"), "pluginval");
        assert_eq!(daw_label("clap-validator"), "clap-validator");
        assert_eq!(daw_label("standalone"), "ConjureAlign dev");
    }

    /// Rule 2 again, at the level `resolve` enforces it: no version may ride
    /// along with an unrecognized host, because the pair identifies far more
    /// than either half.
    #[test]
    fn unknown_hosts_carry_no_version() {
        let unknown = HostInfo {
            daw: UNKNOWN_DAW,
            daw_version: None,
            os_version: os_version(),
        };
        assert_eq!(unknown.daw_version, None);

        // And the real resolution obeys it too, whatever this machine is.
        let info = info();
        if info.daw == UNKNOWN_DAW {
            assert_eq!(info.daw_version, None);
        }
    }

    /// Prints what this machine resolves to. The only way to check the DAW
    /// side end to end is to run the test binary from inside a real
    /// application bundle, which no unit test can arrange for itself:
    ///
    /// ```text
    /// cargo test --release --lib host::tests::print -- --ignored --nocapture
    /// ```
    ///
    /// To exercise the bundle lookup, copy that test binary to
    /// `Foo.app/Contents/MacOS/<a name from the allowlist>`, drop an
    /// Info.plist beside it with a `CFBundleShortVersionString`, and run it
    /// from there — that is the same shape as a DAW loading the plugin.
    #[test]
    #[ignore = "prints the resolved host; run it by hand"]
    fn print_resolved_host_info() {
        println!("{:#?}", info());
    }

    /// Resolution must be total: every path returns, none panics, and the
    /// result is stable across calls.
    #[test]
    fn resolution_is_infallible_and_cached() {
        let first = info();
        let second = info();
        assert!(std::ptr::eq(first, second));
        assert!(!first.daw.is_empty());
    }

    /// The CoreFoundation string handling is the one genuinely unsafe path
    /// that runs on every macOS host, and `CFBundleGetMainBundle` gives a test
    /// binary nothing to read (no .app wrapper) — so exercise the extraction
    /// directly, on a string we build ourselves.
    #[cfg(target_os = "macos")]
    #[test]
    fn cf_strings_round_trip_and_non_strings_are_refused() {
        use core_foundation_sys::base::{CFRelease, CFTypeRef};
        use core_foundation_sys::number::kCFBooleanTrue;
        use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithBytes};

        // SAFETY: the created string is released on every path; `kCFBooleanTrue`
        // is a CF constant and is not ours to release.
        unsafe {
            for original in ["11.2.0", "26.3.1", "", "Ünïcödé 1.0"] {
                let cf = CFStringCreateWithBytes(
                    std::ptr::null(),
                    original.as_ptr(),
                    original.len() as isize,
                    kCFStringEncodingUTF8,
                    0,
                );
                assert!(!cf.is_null());
                let read = cf_string_to_owned(cf as CFTypeRef);
                CFRelease(cf as CFTypeRef);
                assert_eq!(read.as_deref(), Some(original));
            }

            // A NULL value (key absent from the plist) and a value of the
            // wrong type (the plist can hold anything under that key) must
            // both come back as None rather than being read as a string.
            assert_eq!(cf_string_to_owned(std::ptr::null()), None);
            assert_eq!(cf_string_to_owned(kCFBooleanTrue as CFTypeRef), None);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_a_dotted_os_version() {
        let version = os_version().expect("kern.osproductversion is always readable on macOS");
        assert!(
            version
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())),
            "unexpected shape: {version:?}"
        );
    }
}
