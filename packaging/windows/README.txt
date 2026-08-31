ConjureAlign — Windows
======================

Installed locations
-------------------

  VST3   C:\Program Files\Common Files\VST3\ConjureAlign.vst3
  CLAP   C:\Program Files\Common Files\CLAP\ConjureAlign.clap

There is no Audio Unit build — that format is macOS-only.

Restart your DAW after installing. If ConjureAlign does not appear, rescan
plug-ins; some hosts cache their plug-in list and need it cleared.

The installer removes older copies of ConjureAlign from those folders, from
the per-user plug-in folders under %LOCALAPPDATA%\Programs\Common, and from
the 32-bit Common Files folders, so an old build cannot keep loading
alongside the new one.


Which version am I running?
---------------------------

Settings -> Apps -> Installed apps shows "ConjureAlign" and its version.

Or open the plug-in and click the gear button, which shows the version of the
build that is actually loaded.

If the plug-in will not load at all, ask Windows about the files directly.
Paste this into PowerShell — it lists every copy it can find and the version
of each, which is also how to spot a stale copy in a folder the installer
does not know about:

  Get-ChildItem 'C:\Program Files\Common Files',
                'C:\Program Files (x86)\Common Files',
                "$env:LOCALAPPDATA\Programs\Common" `
    -Recurse -Include ConjureAlign.vst3,ConjureAlign.clap -ErrorAction SilentlyContinue |
    Where-Object { -not $_.PSIsContainer } |
    Select-Object FullName, LastWriteTime,
                  @{n='Version';e={$_.VersionInfo.FileVersion}}

(Right-click -> Properties will NOT show a version for these files. Windows
resolves that per file extension and has no handler for .vst3 or .clap.)


Uninstalling
------------

Settings -> Apps -> Installed apps -> ConjureAlign -> Uninstall.

The uninstaller asks whether to also remove your settings — the privacy and
update-check answers, kept in %APPDATA%\ConjureDSP\ConjureAlign. Answering No
keeps them for a future reinstall.


Beta note
---------

The Windows build passes the full DSP test suite, pluginval at strictness 10
and CLAP validation automatically on every release, and the installer itself
is install/upgrade/uninstall tested on every build. It has still had far less
real-DAW testing than the macOS build, and the plug-in window cannot be
tested automatically at all. Reports of anything odd are genuinely useful:

  https://github.com/michaeljancsy/ConjureAlign/issues

Usage guide:

  https://github.com/michaeljancsy/ConjureAlign#how-to-use-it

ConjureAlign is free software under the GNU General Public License v3 or
later. See LICENSE.txt, and THIRD-PARTY.md for the third-party components.
