#!/bin/bash
#
# release.sh — build, sign, notarize, staple and package ConjureAlign for distribution.
#
#   ./scripts/release.sh            # full pipeline → dist/ConjureAlign-<version>-macOS.pkg
#   ./scripts/release.sh --no-notarize   # build+sign+package only (e.g. for a local smoke test;
#                                        # the pkg is left unsigned if the Installer cert is absent)
#
# The deliverable is a signed + notarized + stapled .pkg installer: double-click, choose
# formats (all three preselected), authenticate, done. The component packages install to
# /Library/Audio/Plug-Ins/{VST3,CLAP,Components} and the AU package's postinstall clears
# the AudioComponentRegistrar cache so Logic picks the plugin up without Terminal surgery.
#
# Prerequisites (one-time, in the login keychain):
#   - "Developer ID Application: Michael Jancsy (A4R63LAVLS)"  — signs the plugin bundles
#   - "Developer ID Installer: Michael Jancsy (A4R63LAVLS)"    — signs the .pkg (a DIFFERENT
#     cert: Xcode → Settings → Accounts → Manage Certificates → + → Developer ID Installer)
#   - notarytool keychain profile "ConjureDSP-Notarize" (shared with ConjureDSP; created via
#     `xcrun notarytool store-credentials` — see conjuredsp-application/scripts/notarize.sh)
#
# Run this from the MAIN checkout only: `cargo xtask` inside a .claude/worktrees/ worktree
# silently builds the main checkout's branch instead (see CLAUDE.md, "Known upstream issues").

set -euo pipefail

IDENTITY_APP="Developer ID Application: Michael Jancsy (A4R63LAVLS)"
IDENTITY_PKG="Developer ID Installer: Michael Jancsy (A4R63LAVLS)"
KEYCHAIN_PROFILE="ConjureDSP-Notarize"
BUNDLES=(ConjureAlign.clap ConjureAlign.vst3 ConjureAlign.component)
PKG_ID_BASE="com.michaeljancsy.conjure-align"

cd "$(dirname "$0")/.."
case "$PWD" in */.claude/worktrees/*) echo "ERROR: refusing to release from a worktree (xtask would build the main checkout)"; exit 1;; esac

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NOTARIZE=1
[ "${1:-}" = "--no-notarize" ] && NOTARIZE=0

# Fail on missing certs BEFORE the multi-minute build. Notarization requires a signed pkg,
# so the Installer cert is a hard requirement unless --no-notarize.
SIGN_PKG=1
if ! security find-identity -v | grep -qF "$IDENTITY_PKG"; then
    if [ "$NOTARIZE" = 1 ]; then
        echo "ERROR: \"$IDENTITY_PKG\" is not in the keychain."
        echo "Create it in Xcode: Settings → Accounts → Manage Certificates → + → Developer ID Installer"
        echo "(or developer.apple.com → Certificates). Then re-run. For an unsigned local"
        echo "smoke-test package, run with --no-notarize."
        exit 1
    fi
    SIGN_PKG=0
fi

echo "=== ConjureAlign $VERSION: universal release build ==="
cargo xtask bundle-universal conjure_align --release

# Sentry needs the debug files to turn a shipped crash report into function names:
# [profile.release] keeps `strip = "symbols"`, so the binaries themselves carry none, and
# `split-debuginfo = "packed"` leaves a .dSYM per architecture beside each slice. The
# Mach-O UUID survives stripping, which is what lets Sentry match the two up.
#
# Never fatal. A release that ships is worth more than a symbolicated crash report, and
# the upload can be repeated afterwards from the same build tree.
echo "=== Uploading debug symbols to Sentry ==="
if ! command -v sentry-cli >/dev/null 2>&1; then
    echo "  WARNING: sentry-cli not installed; skipping."
    echo "  Install it with: brew install getsentry/tools/sentry-cli"
elif ! sentry-cli info >/dev/null 2>&1; then
    # Deliberately NOT a test for SENTRY_AUTH_TOKEN. sentry-cli reads its token from the
    # environment OR from ~/.sentryclirc, and `sentry-cli login` only ever writes the
    # latter — so an env-var check reports a perfectly authenticated machine as
    # unconfigured and silently drops the symbols. `info` exits non-zero only when no
    # token is available by any route, which is the question actually being asked.
    echo "  WARNING: sentry-cli is not authenticated; skipping."
    echo "  Run: sentry-cli login"
    echo "  Release $VERSION will report crashes as bare addresses until these are uploaded."
else
    # Both slices, and ONLY our own dylib: bundle-universal builds each arch separately
    # before lipo, so each has its own dSYM and its own UUID. Naming the files rather
    # than the release directory is load-bearing — pointing sentry-cli at a whole
    # release tree sweeps up every dependency's build script, the proc-macro dylibs and
    # the test binaries, none of which can appear in a plugin crash report, and
    # `--include-sources` then bundles their source as well.
    #
    # Org and project come from either the environment or ~/.sentryclirc's [defaults];
    # `sentry-cli login` sets neither, so that is the likeliest way a correctly
    # authenticated machine still fails the upload below.
    SYMS=""
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        for f in "target/$t/release/libconjure_align.dylib.dSYM" \
                 "target/$t/release/libconjure_align.dylib"; do
            [ -e "$f" ] && SYMS="$SYMS $f"
        done
    done
    if [ -z "$SYMS" ]; then
        echo "  WARNING: no libconjure_align dSYM found under target/*/release; skipping."
    elif sentry-cli debug-files upload --include-sources $SYMS; then
        echo "  uploaded (release conjure_align@$VERSION)"
    else
        echo "  WARNING: symbol upload failed; continuing with the release."
        echo "  Check SENTRY_ORG / SENTRY_PROJECT, or the [defaults] in ~/.sentryclirc"
        echo "  (\`sentry-cli info\` prints what it resolved them to) — and the token's"
        echo "  scopes: uploads need project:releases, which a read-only token lacks"
        echo "  (\`sentry-cli info\` lists scopes; an org auth token from sentry.io →"
        echo "  Settings → Auth Tokens carries it)."
    fi
fi

echo "=== Signing bundles (hardened runtime) ==="
for b in "${BUNDLES[@]}"; do
    codesign --force --options runtime --timestamp -s "$IDENTITY_APP" "target/bundled/$b"
    codesign --verify --strict "target/bundled/$b"
    echo "  signed $b"
done

echo "=== Building component packages ==="
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# One component pkg per format, each rooted at its /Library/Audio/Plug-Ins destination.
# The component plist pins BundleIsRelocatable=false: without it Installer "helpfully"
# updates any copy of the bundle Spotlight can find (e.g. a manual install in ~/Library)
# instead of installing to the destination.
build_component() { # $1 bundle  $2 dest subdir  $3 pkg suffix  $4... extra pkgbuild args
    local bundle=$1 dest=$2 suffix=$3; shift 3
    local root="$WORK/root-$suffix"
    mkdir -p "$root"
    cp -R "target/bundled/$bundle" "$root/"
    pkgbuild --analyze --root "$root" "$WORK/$suffix.plist" >/dev/null
    # --analyze omits BundleIsRelocatable (defaulting it to TRUE), hence Add not Set.
    /usr/libexec/PlistBuddy \
        -c "Add :0:BundleIsRelocatable bool false" \
        -c "Set :0:BundleIsVersionChecked false" \
        "$WORK/$suffix.plist"
    pkgbuild --root "$root" \
        --component-plist "$WORK/$suffix.plist" \
        --identifier "$PKG_ID_BASE.$suffix.pkg" \
        --version "$VERSION" \
        --install-location "/Library/Audio/Plug-Ins/$dest" \
        "$@" \
        "$WORK/$suffix.pkg" >/dev/null
    echo "  built $suffix.pkg → /Library/Audio/Plug-Ins/$dest"
}

# Logic caches AU registrations; a stale cache is the #1 "where is the plugin?" support
# question, so the AU package clears it itself. AudioComponentRegistrar is an on-demand
# daemon — killall failing because it is not running is success, hence the || true.
mkdir -p "$WORK/au-scripts"
cat > "$WORK/au-scripts/postinstall" <<'EOF'
#!/bin/sh
killall -9 AudioComponentRegistrar 2>/dev/null || true
exit 0
EOF
chmod +x "$WORK/au-scripts/postinstall"

build_component ConjureAlign.vst3      VST3       vst3
build_component ConjureAlign.clap      CLAP       clap
build_component ConjureAlign.component Components au --scripts "$WORK/au-scripts"

# The uninstaller. Not a checkbox and not optional: a plugin that installs into
# /Library with installer receipts needs a way out that is not a support email
# full of `sudo`.
#
# Deliberately NOT built through build_component(): the payload is a shell
# script, not a bundle, so `pkgbuild --analyze` finds nothing and emits an empty
# array — on which build_component's `PlistBuddy -c "Add :0:..."` fails and,
# under `set -e`, takes the whole release with it. `--component-plist` is
# optional (pkgbuild(1): "If you specify --root, you can use --component-plist"),
# and with no bundles in the root there is nothing for it to configure.
#
# --install-location is the leaf /Applications/ConjureDSP, not /Applications:
# pkgbuild puts a "." entry in the BOM carrying the root directory's own mode,
# and "." maps to the install-location. Aiming it at /Applications would make
# that entry describe /Applications itself (root:admin 0775 here), and packages
# that reset it to root:wheel 0755 are a known class of bug. This way
# /Applications never appears in the BOM at all.
UNROOT="$WORK/root-uninstall"
mkdir -p "$UNROOT"
cp scripts/uninstall-macos.sh "$UNROOT/Uninstall ConjureAlign.command"
# The execute bit is what makes Finder hand a .command to Terminal on
# double-click, and pkgbuild archives the mode it finds on disk. mktemp -d is
# 0700, so the root directory needs widening too or the installed directory
# inherits it.
chmod 755 "$UNROOT/Uninstall ConjureAlign.command"
chmod 755 "$UNROOT"
pkgbuild --root "$UNROOT" \
    --identifier "$PKG_ID_BASE.uninstall.pkg" \
    --version "$VERSION" \
    --install-location "/Applications/ConjureDSP" \
    "$WORK/uninstall.pkg" >/dev/null
echo "  built uninstall.pkg → /Applications/ConjureDSP"

echo "=== Building installer ==="
RES="$WORK/resources"
mkdir -p "$RES"
cp LICENSE "$RES/license.txt"
# Installer's HTML renderer assumes Latin-1 without an explicit charset, which turns any
# UTF-8 byte into mojibake ("—" became "â€""). Both files therefore declare the charset AND
# spell non-ASCII as HTML entities, so they render correctly either way.
cat > "$RES/welcome.html" <<EOF
<html><head><meta charset="utf-8"></head>
<body style="font-family: -apple-system, sans-serif; font-size: 13px;">
<p><b>ConjureAlign $VERSION</b> time-aligns a mic signal to a reference mic with sub-sample
precision and automatic polarity detection.</p>
<p>This installer places the plugin into the system plug-in folders
(<tt>/Library/Audio/Plug-Ins</tt>) for all users. All three formats are installed by
default; click Customize to pick specific ones.</p>
<ul>
<li><b>Audio Unit</b> &mdash; Logic Pro, GarageBand</li>
<li><b>VST3</b> &mdash; REAPER, Ableton Live, Cubase, Studio One</li>
<li><b>CLAP</b> &mdash; Bitwig, REAPER</li>
</ul>
</body></html>
EOF
cat > "$RES/conclusion.html" <<'EOF'
<html><head><meta charset="utf-8"></head>
<body style="font-family: -apple-system, sans-serif; font-size: 13px;">
<p><b>ConjureAlign is installed.</b> Restart your DAW to pick it up.</p>
<p>In Logic Pro it appears under Audio FX &rarr; ConjureDSP &rarr; ConjureAlign
(first launch may revalidate plugins; check Settings &rarr; Plug-in Manager if it is
missing).</p>
<p>If you previously installed ConjureAlign by hand into
<tt>~/Library/Audio/Plug-Ins</tt>, delete those copies so your DAW does not load the old
version.</p>
<p>To remove ConjureAlign later, open <tt>/Applications/ConjureDSP</tt> and double-click
<b>Uninstall ConjureAlign</b>.</p>
<p>Usage guide: <a href="https://github.com/michaeljancsy/ConjureAlign#how-to-use-it">github.com/michaeljancsy/ConjureAlign</a></p>
</body></html>
EOF

cat > "$WORK/distribution.xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="1">
    <title>ConjureAlign $VERSION</title>
    <welcome file="welcome.html" mime-type="text/html"/>
    <license file="license.txt" mime-type="text/plain"/>
    <conclusion file="conclusion.html" mime-type="text/html"/>
    <options customize="allow" require-scripts="false" hostArchitectures="arm64,x86_64"/>
    <domains enable_localSystem="true"/>
    <choices-outline>
        <line choice="au"/>
        <line choice="vst3"/>
        <line choice="clap"/>
        <line choice="uninstaller"/>
    </choices-outline>
    <choice id="au" title="Audio Unit" description="For Logic Pro and GarageBand.">
        <pkg-ref id="$PKG_ID_BASE.au.pkg"/>
    </choice>
    <choice id="vst3" title="VST3" description="For REAPER, Ableton Live, Cubase, Studio One and most other DAWs.">
        <pkg-ref id="$PKG_ID_BASE.vst3.pkg"/>
    </choice>
    <choice id="clap" title="CLAP" description="For Bitwig and REAPER.">
        <pkg-ref id="$PKG_ID_BASE.clap.pkg"/>
    </choice>
    <!-- Hidden and always installed. Not a customer-facing option: an
         uninstaller that only exists when someone remembered to tick it is an
         uninstaller that is missing exactly when it is needed. `visible` is the
         dynamic attribute (re-evaluated as choices change), so nothing
         downstream can flip it back on; `start_enabled="false"` leaves no
         checkbox state to toggle. It must still appear in choices-outline — a
         choice the outline does not reference is inert, and the package would
         silently never install. It carries no scripts, so its position there is
         free. -->
    <choice id="uninstaller" title="Uninstaller"
            visible="false" start_selected="true" start_enabled="false">
        <pkg-ref id="$PKG_ID_BASE.uninstall.pkg"/>
    </choice>
    <pkg-ref id="$PKG_ID_BASE.au.pkg" version="$VERSION">au.pkg</pkg-ref>
    <pkg-ref id="$PKG_ID_BASE.vst3.pkg" version="$VERSION">vst3.pkg</pkg-ref>
    <pkg-ref id="$PKG_ID_BASE.clap.pkg" version="$VERSION">clap.pkg</pkg-ref>
    <pkg-ref id="$PKG_ID_BASE.uninstall.pkg" version="$VERSION">uninstall.pkg</pkg-ref>
</installer-gui-script>
EOF

mkdir -p dist
OUT="dist/ConjureAlign-$VERSION-macOS.pkg"
rm -f "$OUT"
# ${arr[@]+...} guard: /bin/bash is 3.2, where expanding an empty array trips `set -u`.
PRODUCT_SIGN=()
[ "$SIGN_PKG" = 1 ] && PRODUCT_SIGN=(--sign "$IDENTITY_PKG")
productbuild \
    --distribution "$WORK/distribution.xml" \
    --package-path "$WORK" \
    --resources "$RES" \
    ${PRODUCT_SIGN[@]+"${PRODUCT_SIGN[@]}"} \
    "$OUT"

if [ "$NOTARIZE" = 1 ]; then
    # One submission covers the pkg and every signed bundle nested inside it.
    echo "=== Notarizing (typically 5-15 minutes) ==="
    xcrun notarytool submit "$OUT" --keychain-profile "$KEYCHAIN_PROFILE" --wait
    echo "=== Stapling ==="
    xcrun stapler staple "$OUT"
    xcrun stapler validate "$OUT"
fi

echo ""
echo "=== Done: $OUT ==="
if [ "$NOTARIZE" = 1 ]; then
    echo "Signed, notarized and stapled — double-click installs cleanly on any Mac."
elif [ "$SIGN_PKG" = 1 ]; then
    echo "Signed but NOT notarized — fine for this machine, Gatekeeper will block it elsewhere."
else
    echo "UNSIGNED and NOT notarized — local smoke test only (right-click → Open to run it)."
fi
