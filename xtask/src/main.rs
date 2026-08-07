//! AudioAlign's xtask.
//!
//! `bundle` and `bundle-universal` delegate to `nih_plug_xtask` for the CLAP and VST3
//! bundles, then — on macOS — assemble the AudioUnit v2 `.component` on top of the very
//! same binary. That step has to live here: nih_plug_xtask recognises only the
//! `clap_entry` / `VSTPluginMain` / `GetPluginFactory` exports, and the Info.plist it
//! writes has no `AudioComponents` array, which is the only place an AU host can learn our
//! four-character codes. The codes themselves live in `bundler.toml`.

use anyhow::{bail, Context};
use nih_plug_xtask::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

/// `bundler.toml`. nih_plug_xtask parses the same file into a struct that only has `name`
/// and ignores everything else, so the `auv2` table is ours alone.
#[derive(Deserialize)]
struct PackageConfig {
    name: Option<String>,
    auv2: Option<Auv2Config>,
}

/// The AudioUnit identity. See `bundler.toml` for what each field means and why `subtype` /
/// `manufacturer` must never change.
#[derive(Deserialize)]
struct Auv2Config {
    #[serde(rename = "type")]
    au_type: String,
    subtype: String,
    manufacturer: String,
    manufacturer_name: String,
    bundle_id: String,
    description: String,
    /// AU category tags. Logic uses these to file the plugin in its Audio FX menu;
    /// clap-wrapper's own CMake build-helper emits the CLAP features here with the first
    /// character upper-cased, but Logic only recognises a fixed vocabulary, so these are
    /// spelled out in `bundler.toml` instead.
    #[serde(default)]
    tags: Vec<String>,
}

fn main() -> Result<()> {
    // Everything below reads `bundler.toml` and `cargo metadata` relative to the workspace
    // root. `main_with_args()` chdirs there too; the call is idempotent.
    // NOTE: this is the call that makes bundling from a `.claude/worktrees/` worktree pick
    // the main checkout — see CLAUDE.md "Known upstream issues".
    nih_plug_xtask::chdir_workspace_root()?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = bundler_config()?;
    let auv2_packages = auv2_packages(&args, &config);

    nih_plug_xtask::main_with_args("cargo xtask", args)?;

    for package in &auv2_packages {
        bundle_auv2(package, &config[package])?;
    }

    Ok(())
}

fn bundler_config() -> Result<HashMap<String, PackageConfig>> {
    match fs::read_to_string("bundler.toml") {
        Ok(contents) => toml::from_str(&contents).context("Could not parse 'bundler.toml'"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(err).context("Could not read 'bundler.toml'"),
    }
}

/// Which of the selected packages also get a `.component`: only for the bundling commands,
/// only on macOS, only when not cross compiling away from Darwin, and only for packages
/// that declare an `auv2` table.
fn auv2_packages(args: &[String], config: &HashMap<String, PackageConfig>) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    if !matches!(
        args.first().map(String::as_str),
        Some("bundle") | Some("bundle-universal")
    ) {
        return Vec::new();
    }
    if let Some(target) = cross_compile_target(args) {
        if !target.ends_with("-apple-darwin") {
            return Vec::new();
        }
    }

    selected_packages(args)
        .into_iter()
        .filter(|package| {
            config
                .get(package)
                .map(|c| c.auv2.is_some())
                .unwrap_or(false)
        })
        .collect()
}

fn cross_compile_target(args: &[String]) -> Option<&str> {
    args.iter()
        .enumerate()
        .find_map(|(idx, arg)| match arg.as_str() {
            "--target" => args.get(idx + 1).map(String::as_str),
            arg => arg.strip_prefix("--target="),
        })
}

/// Mirrors nih_plug_xtask's own `split_bundle_args`: either a leading run of `-p <package>`
/// pairs, or a single positional package name.
fn selected_packages(args: &[String]) -> Vec<String> {
    let mut packages = Vec::new();
    let mut rest = args.iter().skip(1).peekable();
    if rest.peek().map(|s| s.as_str()) == Some("-p") {
        while rest.peek().map(|s| s.as_str()) == Some("-p") {
            rest.next();
            if let Some(package) = rest.next() {
                packages.push(package.clone());
            }
        }
    } else if let Some(package) = rest.next() {
        packages.push(package.clone());
    }

    packages
}

fn bundle_auv2(package: &str, config: &PackageConfig) -> Result<()> {
    let auv2 = config.auv2.as_ref().expect("filtered in auv2_packages()");
    let bundle_name = config.name.clone().unwrap_or_else(|| package.to_owned());

    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path("./Cargo.toml")
        .no_deps()
        .exec()
        .context("Could not parse `cargo-metadata`")?;
    let version = &metadata
        .packages
        .iter()
        .find(|p| p.name == package)
        .with_context(|| format!("No package named '{package}' in this workspace"))?
        .version;
    let bundled = metadata.target_directory.as_std_path().join("bundled");

    // Reuse the binary nih_plug_xtask just put in the CLAP bundle. It is the same dylib —
    // the AU entry point is a third exported symbol in it — and for `bundle-universal` it
    // has already been lipo'd, so this one path covers single-arch, cross-compiled Darwin
    // and universal builds without duplicating any of nih_plug_xtask's target/profile path
    // logic.
    let source = bundled
        .join(format!("{bundle_name}.clap"))
        .join("Contents/MacOS")
        .join(&bundle_name);
    if !source.exists() {
        bail!(
            "Expected a CLAP bundle at '{}' to build the AudioUnit from — either the plugin \
             no longer exports `clap_entry`, or nih_plug_xtask changed where it puts the \
             bundle.",
            source.display()
        );
    }
    check_exports_au_factory(&source)?;

    let home = bundled.join(format!("{bundle_name}.component"));
    // Rebuild from scratch: a `_CodeSignature` left over from an earlier run describes
    // files we are about to replace.
    if home.exists() {
        fs::remove_dir_all(&home).context("Could not remove the previous .component bundle")?;
    }
    let contents = home.join("Contents");
    fs::create_dir_all(contents.join("MacOS"))
        .context("Could not create the .component bundle directory")?;
    fs::copy(&source, contents.join("MacOS").join(&bundle_name))
        .context("Could not copy the plugin binary into the .component bundle")?;
    fs::write(contents.join("PkgInfo"), "BNDL????").context("Could not create PkgInfo file")?;
    fs::write(
        contents.join("Info.plist"),
        info_plist(
            &bundle_name,
            auv2,
            version.major,
            version.minor,
            version.patch,
        )?,
    )
    .context("Could not create Info.plist file")?;

    codesign(&home);
    eprintln!("Created an AUv2 bundle at '{}'", home.display());

    Ok(())
}

/// The Info.plist below names `GetPluginFactoryAUV2` as the AU entry point, and nothing else
/// in the build fails when the binary does not have it: `clap_wrapper::export_auv2!()`
/// expands over an empty module when the crate's `auv2` feature is off, so it still compiles.
/// The bundle would then install and sign cleanly and fail only inside `auval` or Logic, with
/// an error that says nothing about the missing symbol. nih_plug_xtask sniffs exports to
/// decide which bundles to write; do the same here.
fn check_exports_au_factory(binary: &Path) -> Result<()> {
    let symbols = match Command::new("nm").arg("-gU").arg(binary).output() {
        Ok(output) if output.status.success() => output.stdout,
        // No usable `nm`: warn rather than fail a build over a missing developer tool.
        _ => {
            eprintln!(
                "WARNING: Could not run `nm` on '{}' to check that it exports \
                 GetPluginFactoryAUV2",
                binary.display()
            );
            return Ok(());
        }
    };

    if !String::from_utf8_lossy(&symbols).contains("_GetPluginFactoryAUV2") {
        bail!(
            "'{}' does not export `GetPluginFactoryAUV2`, which the .component's Info.plist \
             names as its factory function. Is `clap_wrapper::export_auv2!()` still in \
             src/lib.rs, and is the `clap-wrapper` dependency's `auv2` feature enabled?",
            binary.display()
        );
    }

    Ok(())
}

/// The same ad-hoc self-signing nih_plug_xtask does for the other bundles; AArch64 macOS is
/// stricter and unsigned plugin binaries may not load. Warns rather than fails, like
/// upstream.
fn codesign(bundle_home: &Path) {
    let signed = Command::new("codesign")
        .args(["-f", "-s", "-"])
        .arg(bundle_home)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !signed {
        eprintln!(
            "WARNING: Could not self-sign '{}', it may fail to load depending on the \
             environment",
            bundle_home.display()
        );
    }
}

/// AudioUnit packs a version into a `UInt32` as `0xMMMMmmbb`, so 0.1.0 becomes `0x000100`
/// == 256. Zero is not a legal AU version, hence the floor of 1.
fn au_version(major: u64, minor: u64, patch: u64) -> Result<u32> {
    if major > 0xffff || minor > 0xff || patch > 0xff {
        bail!("Version {major}.{minor}.{patch} does not fit AudioUnit's 0xMMMMmmbb packing");
    }

    Ok((((major as u32) << 16) | ((minor as u32) << 8) | patch as u32).max(1))
}

/// The plist is assembled by hand below, so every value out of `bundler.toml` has to be
/// escaped. An unescaped `&` in a name or description produces XML CoreFoundation cannot
/// parse, and the symptom is not a build error — it is the plugin silently never registering.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `type`, `subtype` and `manufacturer` are OSTypes: exactly four ASCII characters, packed
/// into a `UInt32` by the system. A typo'd or over-long code is written verbatim into the
/// plist and fails the same silent way an unescaped metacharacter does.
fn check_four_cc(field: &str, value: &str) -> Result<()> {
    if value.chars().count() != 4 || !value.is_ascii() {
        bail!("`{field}` must be exactly four ASCII characters, got '{value}'");
    }

    Ok(())
}

fn info_plist(
    bundle_name: &str,
    auv2: &Auv2Config,
    major: u64,
    minor: u64,
    patch: u64,
) -> Result<String> {
    let Auv2Config {
        au_type,
        subtype,
        manufacturer,
        manufacturer_name,
        bundle_id,
        description,
        tags,
    } = auv2;
    check_four_cc("type", au_type)?;
    check_four_cc("subtype", subtype)?;
    check_four_cc("manufacturer", manufacturer)?;
    if !manufacturer.chars().any(|c| c.is_ascii_uppercase()) {
        bail!(
            "`manufacturer` must contain an uppercase character, got '{manufacturer}': Apple \
             reserves all-lowercase manufacturer codes"
        );
    }

    let bundle_name = xml_escape(bundle_name);
    let au_type = xml_escape(au_type);
    let subtype = xml_escape(subtype);
    let manufacturer = xml_escape(manufacturer);
    let manufacturer_name = xml_escape(manufacturer_name);
    let bundle_id = xml_escape(bundle_id);
    let description = xml_escape(description);

    let short_version = format!("{major}.{minor}.{patch}");
    let packed_version = au_version(major, minor, patch)?;
    let tags = if tags.is_empty() {
        String::new()
    } else {
        let entries = tags
            .iter()
            .map(|tag| format!("          <string>{}</string>\n", xml_escape(tag)))
            .collect::<String>();
        format!("        <key>tags</key>\n        <array>\n{entries}        </array>\n")
    };

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>English</string>
    <key>CFBundleExecutable</key>
    <string>{bundle_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>{bundle_name}</string>
    <key>CFBundleDisplayName</key>
    <string>{bundle_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>{short_version}</string>
    <key>CFBundleVersion</key>
    <string>{short_version}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>AudioComponents</key>
    <array>
      <dict>
        <!-- "Manufacturer: Plugin"; Logic splits on the colon to group its plugin menu. -->
        <key>name</key>
        <string>{manufacturer_name}: {bundle_name}</string>
        <key>description</key>
        <string>{description}</string>
        <!-- clap-wrapper's `export_auv2!()` exports exactly this symbol; it forwards to
             the first plugin behind our `clap_entry`. -->
        <key>factoryFunction</key>
        <string>GetPluginFactoryAUV2</string>
        <key>type</key>
        <string>{au_type}</string>
        <key>subtype</key>
        <string>{subtype}</string>
        <key>manufacturer</key>
        <string>{manufacturer}</string>
        <!-- AudioUnit packs the version as 0xMMMMmmbb: {short_version} == {packed_version}. -->
        <key>version</key>
        <integer>{packed_version}</integer>
        <!-- Deliberately NO `sandboxSafe` key: without it the AU is treated as not
             sandbox-safe and hosts load it in-process, which is what we want and what
             most third-party AUs get. Claiming sandbox-safety we have not tested only
             buys a stricter hosting path that can fail silently — upstream
             clap-wrapper-rs dropped the flag from its own bundler for the same reason.
             `resourceUsage` matches what clap-wrapper's CMake build-helper emits and is
             what a sandboxed host would consult; `files.all.read-write` is also what
             pointing NIH_LOG at a file would need. -->
        <key>resourceUsage</key>
        <dict>
          <key>network.client</key>
          <true/>
          <key>temporary-exception.files.all.read-write</key>
          <true/>
        </dict>
{tags}
      </dict>
    </array>
  </dict>
</plist>
"#
    ))
}
