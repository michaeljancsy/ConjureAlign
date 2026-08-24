#!/bin/bash
#
# release.sh — build, sign, notarize, staple and package ConjureAlign for distribution.
#
#   ./scripts/release.sh            # full pipeline → dist/ConjureAlign-<version>-macOS.zip
#   ./scripts/release.sh --no-notarize   # build+sign only (e.g. for a local smoke test)
#
# Prerequisites (one-time, already present on this machine):
#   - "Developer ID Application: Michael Jancsy (A4R63LAVLS)" in the login keychain
#   - notarytool keychain profile "ConjureDSP-Notarize" (shared with ConjureDSP; created via
#     `xcrun notarytool store-credentials` — see conjuredsp-application/scripts/notarize.sh)
#
# Run this from the MAIN checkout only: `cargo xtask` inside a .claude/worktrees/ worktree
# silently builds the main checkout's branch instead (see CLAUDE.md, "Known upstream issues").

set -euo pipefail

IDENTITY="Developer ID Application: Michael Jancsy (A4R63LAVLS)"
KEYCHAIN_PROFILE="ConjureDSP-Notarize"
BUNDLES=(ConjureAlign.clap ConjureAlign.vst3 ConjureAlign.component)

cd "$(dirname "$0")/.."
case "$PWD" in */.claude/worktrees/*) echo "ERROR: refusing to release from a worktree (xtask would build the main checkout)"; exit 1;; esac

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NOTARIZE=1
[ "${1:-}" = "--no-notarize" ] && NOTARIZE=0

echo "=== ConjureAlign $VERSION: universal release build ==="
cargo xtask bundle-universal conjure_align --release

echo "=== Signing (hardened runtime) ==="
for b in "${BUNDLES[@]}"; do
    codesign --force --options runtime --timestamp -s "$IDENTITY" "target/bundled/$b"
    codesign --verify --strict "target/bundled/$b"
    echo "  signed $b"
done

if [ "$NOTARIZE" = 1 ]; then
    echo "=== Notarizing (typically 5-15 minutes) ==="
    # notarytool takes one archive; a single zip holding all three bundles works.
    SUBMIT_ZIP=$(mktemp -d)/ConjureAlign-notarize.zip
    (cd target/bundled && zip -q -r -y "$SUBMIT_ZIP" "${BUNDLES[@]}")
    xcrun notarytool submit "$SUBMIT_ZIP" --keychain-profile "$KEYCHAIN_PROFILE" --wait
    rm -f "$SUBMIT_ZIP"

    echo "=== Stapling ==="
    for b in "${BUNDLES[@]}"; do
        xcrun stapler staple "target/bundled/$b"
        xcrun stapler validate "target/bundled/$b"
    done
fi

echo "=== Packaging ==="
STAGE=$(mktemp -d)/ConjureAlign-$VERSION
mkdir -p "$STAGE" dist
for b in "${BUNDLES[@]}"; do cp -R "target/bundled/$b" "$STAGE/"; done
cp LICENSE THIRD-PARTY.md "$STAGE/"
cat > "$STAGE/INSTALL.txt" <<'EOF'
ConjureAlign — installation (macOS)

Copy each bundle to the matching folder (create it if missing):

  ConjureAlign.vst3      -> ~/Library/Audio/Plug-Ins/VST3/
  ConjureAlign.clap      -> ~/Library/Audio/Plug-Ins/CLAP/
  ConjureAlign.component -> ~/Library/Audio/Plug-Ins/Components/   (required for Logic)

Then restart your DAW. If Logic does not list the plugin (Audio FX > ConjureDSP),
open Terminal, run:  killall -9 AudioComponentRegistrar
and restart Logic; it validates under Settings > Plug-in Manager.

Usage: see https://github.com/michaeljancsy/ConjureAlign#how-to-use-it
EOF
OUT="dist/ConjureAlign-$VERSION-macOS.zip"
rm -f "$OUT"
ditto -c -k --keepParent "$STAGE" "$OUT"
rm -rf "$(dirname "$STAGE")"

echo ""
echo "=== Done: $OUT ==="
if [ "$NOTARIZE" = 1 ]; then
    echo "Notarized and stapled — installs cleanly on any Mac."
else
    echo "NOT notarized — fine for this machine, Gatekeeper will block it elsewhere."
fi
