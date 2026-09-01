//! Build script. Its only job is to stamp a Windows `VS_VERSION_INFO` resource
//! into the cdylib.
//!
//! Why it exists: on Windows the shipped file is this DLL renamed — to
//! `ConjureAlign.vst3` inside the VST3 bundle, and to `ConjureAlign.clap` — with
//! no other identity on disk. A crash report could not be attributed to a
//! version, and neither we nor a user could tell whether an "update" had
//! actually replaced anything. macOS has never had this problem: the bundle's
//! `Info.plist` carries `CFBundleShortVersionString`.
//!
//! Everything here is a no-op off Windows, so the macOS pipeline is untouched.
//!
//! Two version formats live here and must NOT be conflated:
//!
//! * the STRING `FileVersion` / `ProductVersion` are `CARGO_PKG_VERSION`
//!   verbatim ("1.3.0") — the same three-part shape as the `v*` release tag,
//!   and the only shape `update::parse_version` accepts;
//! * the BINARY `VS_FIXEDFILEINFO` block is four 16-bit words and is always
//!   `MAJOR.MINOR.PATCH.0`. Nothing in the Rust tree ever reads it back.

fn main() {
    // Narrows Cargo's default "rerun if anything in the package changed" to the
    // two inputs the resource actually depends on.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=RC_PATH");

    #[cfg(windows)]
    windows_version_resource();
}

/// `#[cfg(windows)]` is the HOST — build scripts compile for the host, which is
/// what makes the host-evaluated `[target.'cfg(target_os = "windows")'
/// .build-dependencies]` entry enough for `winresource` to exist here. The
/// TARGET is a separate question, checked below, so a cross-build cannot
/// half-fire.
#[cfg(windows)]
fn windows_version_resource() {
    use winresource::{VersionInfo, WindowsResource};

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Cargo guarantees all three to build scripts.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap();
    let license = std::env::var("CARGO_PKG_LICENSE").unwrap();

    let mut res = WindowsResource::new();

    // winresource defaults ProductName and FileDescription to CARGO_PKG_NAME
    // ("conjure_align"). Every user-visible string is overridden so the resource
    // reads as the product rather than the crate. ASCII throughout — the
    // generated .rc does declare UTF-8, but not relying on it is free.
    res.set("ProductName", "ConjureAlign")
        // Windows labels this "Description" in the property sheet and uses it as
        // the Task Manager display name; it is the field a human reads.
        .set("FileDescription", &description)
        // Matches Plugin::VENDOR in src/lib.rs and bundler.toml's
        // manufacturer_name, so every format and every OS surface says one brand.
        .set("CompanyName", "ConjureDSP")
        .set(
            "LegalCopyright",
            &format!("Copyright (C) Michael Jancsy - {license}"),
        )
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("InternalName", "ConjureAlign")
        // Deliberately the LINKER's output name, not a shipped one: this single
        // DLL is installed twice, as ConjureAlign.vst3 and as ConjureAlign.clap.
        // Either shipped name here would be a lie about the other, and Windows
        // uses OriginalFilename precisely to flag a renamed binary.
        .set("OriginalFilename", "conjure_align.dll");

    // VS_FIXEDFILEINFO.dwFileType. winresource defaults to VFT_APP (1); this is
    // a DLL, so VFT_DLL (2). FILEVERSION/PRODUCTVERSION keep winresource's
    // default packing of CARGO_PKG_VERSION_* into MAJOR.MINOR.PATCH.0.
    res.set_version_info(VersionInfo::FILETYPE, 2);

    // A hard failure, not a warning. A silently missing resource is exactly the
    // invisible failure this change exists to remove — and rc.exe ships with the
    // Windows SDK, which any working MSVC Rust toolchain already needs.
    res.compile().unwrap_or_else(|e| {
        panic!(
            "could not embed the Windows version resource: {e}\n\
             This needs rc.exe from the Windows SDK. winresource looks for it under\n\
             HKLM\\SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots; if that fails,\n\
             set RC_PATH to the full path of rc.exe and rebuild."
        )
    });
}
