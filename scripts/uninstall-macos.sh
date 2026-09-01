#!/bin/bash
#
# Uninstall ConjureAlign.command
#
# Shipped inside ConjureAlign-<version>-macOS.pkg and installed to
# /Applications/ConjureDSP/. Double-click it: Finder hands a .command to
# Terminal, which runs it as the logged-in user.
#
# Two phases. Phase 1 runs as you and touches only your own home directory.
# Phase 2 is a single `sudo` re-exec of this same file, covering everything that
# needs root: /Library, other users' plug-in folders, the installer receipts,
# and this file itself.
#
# Why `sudo` re-exec and not `osascript … with administrator privileges`: the
# osascript route attributes the authentication dialog to the calling process,
# so a shipped product asks for your password as "osascript", which reads like
# malware. It also splits the interaction across a Terminal transcript and a
# floating dialog, and fails outright with no window server.
#
# `sudo "$0"` is safe HERE specifically because the .pkg installs this file
# root:wheel 0755 inside /Applications/ConjureDSP, itself root:wheel 0755, so no
# unprivileged user can rewrite it. Do NOT relocate it somewhere user-writable;
# that would turn this into a local privilege escalation.
#
# The whole body is one { … } group deliberately. bash reads a script lazily
# from the file, so a script that deletes itself mid-run can be left executing a
# truncated file. A brace group is parsed in full before any of it runs, which
# is what makes the self-delete at the end safe.
#
# Usage (all optional, for scripted runs and the /reinstall skill):
#   --yes               skip the "Continue?" confirmation
#   --remove-settings   remove preferences without asking
#   --keep-settings     keep preferences without asking
{
# `set -u` only. Deliberately no `set -e`: every removal here is best-effort,
# and one failed test must not abandon an uninstall halfway through.
set -u

PKG_IDS="com.michaeljancsy.conjure-align.au.pkg
com.michaeljancsy.conjure-align.vst3.pkg
com.michaeljancsy.conjure-align.clap.pkg
com.michaeljancsy.conjure-align.uninstall.pkg"

# subdir:bundle — the three install locations, named once.
FORMATS="VST3:ConjureAlign.vst3 CLAP:ConjureAlign.clap Components:ConjureAlign.component"

INSTALL_DIR="/Applications/ConjureDSP"
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

# Every real user's home directory. Under `sudo`, $HOME is /var/root and ~ is
# useless, so ask the directory service instead. `_`-prefixed accounts are macOS
# service accounts; a home outside /Users is a network or mobile account that is
# not ours to reach into. `read -r u h` rather than `awk '{print $2}'` keeps a
# home directory containing a space in one piece.
#
# KEEP IN SYNC with the preinstall sweep generated in scripts/release.sh.
user_homes() {
    dscl . -list /Users NFSHomeDirectory 2>/dev/null | while read -r u h; do
        case "$u" in _*|"") continue ;; esac
        case "$h" in /Users/*) printf '%s\n' "$h" ;; esac
    done
}

# Every path the plugin could occupy, system domain first. The user domain is
# included because the .pkg installs for every user — and because a copy
# hand-installed into ~/Library is the exact thing that keeps an old build
# loading, which is most of why this script exists.
all_bundle_paths() {
    for f in $FORMATS; do
        printf '/Library/Audio/Plug-Ins/%s/%s\n' "${f%%:*}" "${f##*:}"
    done
    user_homes | while read -r h; do
        for f in $FORMATS; do
            printf '%s/Library/Audio/Plug-Ins/%s/%s\n' "$h" "${f%%:*}" "${f##*:}"
        done
    done
}

existing_bundles() {
    all_bundle_paths | while read -r p; do
        if [ -e "$p" ] || [ -L "$p" ]; then printf '%s\n' "$p"; fi
    done
}

existing_receipts() {
    printf '%s\n' "$PKG_IDS" | while read -r id; do
        [ -n "$id" ] || continue
        if pkgutil --pkg-info "$id" >/dev/null 2>&1; then printf '%s\n' "$id"; fi
    done
}

# ---------------------------------------------------------------------------
# Phase 2 — everything needing root. Never reads $HOME: it is /var/root here.
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--privileged-phase" ]; then
    existing_bundles | while read -r p; do
        echo "  removing $p"
        rm -rf "$p"
    done

    # The AU cache is a cache: an entry surviving a component we just deleted is
    # how Logic keeps listing a plugin that is gone, and the cost of clearing it
    # is one slower plug-in scan. `;` not `&&` — AudioComponentRegistrar is an
    # on-demand daemon, so killall exits non-zero whenever it happens to be idle.
    killall -9 AudioComponentRegistrar 2>/dev/null
    user_homes | while read -r h; do
        rm -rf "$h/Library/Caches/AudioUnitCache"
    done

    existing_receipts | while read -r id; do
        echo "  forgetting $id"
        pkgutil --forget "$id" >/dev/null 2>&1
    done

    # Ourselves, last, and only from the location the .pkg installs to — run
    # from a source checkout this must not delete the checkout's copy.
    case "$SELF" in
        "$INSTALL_DIR"/*)
            echo "  removing $SELF"
            rm -f "$SELF"
            # Every pkg payload carries an AppleDouble sidecar for each file —
            # macOS stamps com.apple.provenance on everything and it cannot be
            # stripped, so pkgbuild always emits one. Installer is expected to
            # merge it back onto the file rather than leave it on disk, but if
            # it ever does not, the leftover would keep the rmdir below from
            # succeeding and the directory would linger forever.
            rm -f "$(dirname "$SELF")/._$(basename "$SELF")"
            # Non-recursive on purpose: /Applications/ConjureDSP is shared with
            # any other ConjureDSP uninstaller, so it goes only if already empty.
            rmdir "$INSTALL_DIR" 2>/dev/null
            ;;
    esac
    exit 0
fi

# ---------------------------------------------------------------------------
# Phase 1 — runs as you.
# ---------------------------------------------------------------------------
ASSUME_YES=0
SETTINGS_ANSWER=""
for a in "$@"; do
    case "$a" in
        --yes)             ASSUME_YES=1 ;;
        --remove-settings) SETTINGS_ANSWER=y ;;
        --keep-settings)   SETTINGS_ANSWER=n ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

SETTINGS="$HOME/Library/Application Support/ConjureDSP/ConjureAlign"

printf '\n=== Uninstall ConjureAlign ===\n\n'
printf 'Quit your DAWs first. A host that already has the plug-in loaded keeps\n'
printf 'running the old code until you relaunch it.\n\n'

BUNDLES=$(existing_bundles)
RECEIPTS=$(existing_receipts)

if [ -z "$BUNDLES" ] && [ -z "$RECEIPTS" ]; then
    printf 'No installed copy of ConjureAlign found.\n'
else
    printf 'This will remove:\n'
    [ -n "$BUNDLES" ]  && printf '%s\n' "$BUNDLES"  | sed 's/^/  /'
    [ -n "$RECEIPTS" ] && printf '%s\n' "$RECEIPTS" | sed 's/^/  receipt /'
    printf '  the Audio Unit cache (rebuilt automatically on the next scan)\n'
fi
# This list is built as you, and ~/Library is mode 0700, so it cannot see into
# another account. The privileged phase runs as root and DOES remove their
# copies too — say so rather than quietly under-reporting what is about to go.
# The removals are echoed there, so the full set ends up on screen either way.
printf '  the same files in every other user account on this Mac\n'
printf '    (not listed above: only root can look inside another user Library)\n'
printf '  %s\n\n' "$SELF"

confirm() { # $1 = prompt
    [ "$ASSUME_YES" = 1 ] && return 0
    if [ ! -t 0 ]; then
        printf '%s no (not interactive; pass --yes to proceed)\n' "$1"
        return 1
    fi
    printf '%s [y/N] ' "$1"
    read -r a
    case "$a" in [yY]|[yY][eE][sS]) return 0 ;; *) return 1 ;; esac
}

if ! confirm "Continue?"; then
    printf '\nCancelled. Nothing was removed.\n\n'
    exit 0
fi

# The settings question. A Terminal prompt rather than an osascript dialog: the
# window is already open and in front of the user, one channel is easier to
# follow than two, and `read` still works over ssh or under a test script where
# `display dialog` would hang. This is the one path here that destroys something
# the user cannot rebuild, so with no tty the answer is no.
if [ -e "$SETTINGS" ]; then
    printf '\nYour ConjureAlign settings:\n  %s\n' "$SETTINGS"
    printf 'That holds the analytics and update-check answers and a random install id.\n'
    printf 'Removing it means the first-run privacy prompt is asked again and a later\n'
    printf 'install counts as a new one. Removing the plug-in does not require it.\n'
    if [ -z "$SETTINGS_ANSWER" ]; then
        if [ -t 0 ]; then
            printf 'Remove settings too? [y/N] '
            read -r SETTINGS_ANSWER
        else
            SETTINGS_ANSWER=n
        fi
    fi
    # The answer is only RECORDED here. Removing settings is irreversible and
    # the sudo below can still fail — a cancelled password prompt used to leave
    # the plug-in installed with the consent answers and install id already
    # destroyed, which silently re-asks the privacy prompt and mints a new
    # device id. Ask early, act only once the rest has actually succeeded.
    case "$SETTINGS_ANSWER" in
        [yY]|[yY][eE][sS]) printf 'Settings will be removed at the end.\n' ;;
        *) printf 'Settings kept.\n' ;;
    esac
fi

printf '\nThe rest needs administrator rights: /Library, the installer receipts\n'
printf 'and this file itself are root-owned. You will be asked for your password.\n\n'

if sudo "$SELF" --privileged-phase; then
    # Now, and as you rather than as root, so $HOME is the right home.
    if [ -e "$SETTINGS" ]; then
        case "$SETTINGS_ANSWER" in
            [yY]|[yY][eE][sS])
                # ONLY the ConjureAlign child. The parent
                # ~/Library/Application Support/ConjureDSP/ is shared with the
                # ConjureDSP app and every other ConjureDSP plug-in — exports,
                # caches, a vendored Python runtime. Never rm -rf the parent.
                rm -rf "$SETTINGS"
                printf '  removed %s\n' "$SETTINGS"
                ;;
        esac
    fi
    printf '\nConjureAlign has been removed. Restart your DAW.\n\n'
    exit 0
fi

printf '\nCould not get administrator rights, so nothing was removed — including\n'
printf 'your settings, which are still at:\n  %s\n\n' "$SETTINGS"
printf 'Either re-run this as an admin user, or run these in Terminal:\n\n'
[ -n "$BUNDLES" ]  && printf '%s\n' "$BUNDLES"  | sed 's|.*|  sudo rm -rf "&"|'
[ -n "$RECEIPTS" ] && printf '%s\n' "$RECEIPTS" | sed 's|.*|  sudo pkgutil --forget &|'
printf '  sudo rm -rf "%s"\n' "$INSTALL_DIR"
# Same blind spot as the preview above: this list was built as you, so it names
# no other account's copies. Re-running the uninstaller as an admin is the only
# route that reaches those.
printf '\nThose paths cover this account only. Copies belonging to other users on\n'
printf 'this Mac need the uninstaller itself, run by an administrator.\n\n'
exit 1
}
