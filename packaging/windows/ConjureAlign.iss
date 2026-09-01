; ConjureAlign — Windows installer (Inno Setup 6).
;
;   export CONJUREALIGN_VERSION=1.3.0
;   iscc packaging/windows/ConjureAlign.iss
;
; Expects a staging tree at <repo>/dist/stage, built by .github/workflows/windows.yml:
;   ConjureAlign.vst3\      the VST3 *bundle directory* from target\bundled
;   ConjureAlign.clap       the plain CLAP file from target\bundled
;   LICENSE.txt  THIRD-PARTY.md  README.txt   (CRLF)
; and writes <repo>/dist/ConjureAlign-<version>-Windows-Setup.exe
;
; This replaced a zip that users unpacked and hand-copied into
; C:\Program Files\Common Files\{VST3,CLAP}\. Every way that went wrong was
; silent — Explorer skipping a file a running DAW had locked, a user without
; admin rights dropping it somewhere else while the old copy stayed, a nested
; ConjureAlign-<version>\ folder left by dragging the wrong thing — and the
; result was always an old build still loading with nobody aware of it.
;
; The installer is UNSIGNED. SmartScreen warns on first download ("More info →
; Run anyway"); that is accepted, since an OV/EV code-signing certificate is the
; only fix and is not worth it here.
;
; The version arrives through the ENVIRONMENT rather than ISCC's /DName=value,
; because CI drives ISCC from `shell: bash` — i.e. Git Bash, whose MSYS layer
; rewrites any argument starting with a slash into a Windows path, so
; /DAppVersion=1.3.0 would reach ISCC as D:\AppVersion=1.3.0. GetEnv has no such
; hazard and needs no quoting.

#define AppVersion GetEnv("CONJUREALIGN_VERSION")
#if AppVersion == ""
  #error CONJUREALIGN_VERSION is not set. Export it before running ISCC.
#endif

#define AppName      "ConjureAlign"
#define AppPublisher "ConjureDSP"
#define AppURL       "https://github.com/michaeljancsy/ConjureAlign"

[Setup]
; ---------------------------------------------------------------------------
; Identity
; ---------------------------------------------------------------------------
; NEVER CHANGE THIS GUID. AppId is how a later installer recognises an existing
; install: it names the uninstall registry key ({AppId}_is1) and is recorded in
; unins000.dat. Change it and 1.4.0 will not see 1.3.0 — two entries in
; Add/Remove Programs, two uninstallers, and no upgrade path, which is the exact
; failure this installer exists to end. It is the same kind of permanent
; identity as the AudioUnit subtype in bundler.toml, and pinned for the same
; reason. The doubled leading brace escapes Inno's own {constant} syntax.
AppId={{3447632D-7F44-44AD-9A1E-E4C24A33E998}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases/latest

; Version resource of Setup.exe and unins000.exe themselves. Must be numeric, so
; a pre-release version string would fail the compile here — consistent with
; src/update.rs, which refuses to parse such tags anyway.
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} {#AppVersion} Setup
VersionInfoCopyright=Copyright (C) Michael Jancsy - GPL-3.0-or-later

; ---------------------------------------------------------------------------
; Platform
; ---------------------------------------------------------------------------
; The plugin is a 64-bit DLL, so 64-bit install mode — which is also what makes
; {commoncf64} legal and points HKLM at the 64-bit registry view.
; "x64compatible" (Inno 6.3+) also covers ARM64 Windows, where an emulated x64
; DAW loads this DLL and looks in the same folders.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; rustc dropped Windows 7/8.1 from tier-1 x86_64-pc-windows-msvc in 1.78, so the
; binary genuinely will not run below Windows 10. Refusing here beats a mystery
; load failure inside the DAW.
MinVersion=10.0

; The destinations are machine-wide, so admin is mandatory and there is no
; per-user mode to fall back to.
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=

; ---------------------------------------------------------------------------
; Layout
; ---------------------------------------------------------------------------
; Relative paths in LicenseFile, OutputDir and [Files] Source all resolve
; against SourceDir, which itself resolves against this script's directory.
SourceDir=..\..\dist\stage
OutputDir=..
OutputBaseFilename={#AppName}-{#AppVersion}-Windows-Setup

; {app} is NOT where the plugin goes — the plugin goes into the Common Files
; scan folders below. {app} holds the licence texts, the readme and the
; uninstaller, and is what Add/Remove Programs points at.
;
; Deliberately NOT nested under a {#AppPublisher} folder. Inno removes {app}
; itself once it is empty, but never retries its parent, and no [Code] hook
; runs late enough to do it by hand: at usPostUninstall — the last step there
; is — {app} still holds unins000.exe and unins000.dat, which are deleted
; afterwards. A publisher folder would therefore be left behind empty on every
; uninstall, which reads as "the uninstaller did not finish".
DefaultDirName={commonpf64}\{#AppName}
DisableDirPage=yes
DisableProgramGroupPage=yes
UninstallFilesDir={app}
UninstallDisplayName={#AppName} {#AppVersion}

Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
LicenseFile=LICENSE.txt

; Writes %TEMP%\Setup Log*.txt on every run — the first thing to ask a bug
; reporter for.
SetupLogging=yes

; ---------------------------------------------------------------------------
; Files a DAW is holding open
; ---------------------------------------------------------------------------
; A host with the plugin loaded holds the DLL open, and neither the
; [InstallDelete] sweep nor the overwrite can succeed against it. Restart
; Manager finds the holders and offers to close them.
;
; CloseApplicationsFilter is LOAD-BEARING: its default is "*.exe,*.dll,*.chm",
; which matches NEITHER .vst3 nor .clap. Left at the default, CloseApplications
; would silently check nothing at all.
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll,*.chm,*.vst3,*.clap
; Do not relaunch: a DAW coming back up mid-install would rescan and re-lock.
RestartApplications=no

; The per-user sweep entries below reference {localappdata} from an admin-mode
; installer. That is deliberate (see [InstallDelete]); this acknowledges the
; compiler warning about it rather than hiding it.
UsedUserAreasWarning=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Dirs]
; Shared scan folders. uninsneveruninstall stops the uninstaller from removing a
; directory other vendors' plugins also live in.
Name: "{commoncf64}\VST3"; Flags: uninsneveruninstall
Name: "{commoncf64}\CLAP"; Flags: uninsneveruninstall

[Files]
; VST3 on Windows is a BUNDLE — a directory tree whose payload is the renamed
; DLL at Contents\x86_64-win\ConjureAlign.vst3. recursesubdirs +
; createallsubdirs copy and recreate that tree.
;
; ignoreversion is REQUIRED now that the DLL carries a version resource. Inno's
; default is to skip a file whose installed copy has an equal-or-newer version,
; and to skip when the existing file has version info and the incoming one does
; not — so without this flag, reinstalling the same version, or rolling back to
; a build predating the resource, would silently install nothing. That is
; precisely the failure this installer exists to end.
Source: "ConjureAlign.vst3\*"; DestDir: "{commoncf64}\VST3\ConjureAlign.vst3"; \
    Flags: ignoreversion recursesubdirs createallsubdirs uninsremovereadonly

; CLAP on Windows is a PLAIN FILE, not a bundle.
Source: "ConjureAlign.clap"; DestDir: "{commoncf64}\CLAP"; \
    Flags: ignoreversion uninsremovereadonly

; GPL-3.0-or-later: the licence and third-party notices ship beside the binaries.
Source: "LICENSE.txt";    DestDir: "{app}"; Flags: ignoreversion
Source: "THIRD-PARTY.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.txt";     DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; Runs as the FIRST step of installation, before any file is copied, and its
; entries are covered by the Restart Manager check above.
;
; Three shapes of stale copy, all of them reachable from the zip era:
;
;   1. A previous install's bundle directory. Removing it wholesale rather than
;      overwriting in place guarantees no orphaned file survives inside it.
;   2. A LOOSE ConjureAlign.vst3 DLL sitting directly in the VST3 folder — the
;      classic mis-install, where the inner file was copied out of the bundle
;      instead of the bundle itself. `filesandordirs` deletes a match whether it
;      is a file or a directory, so one entry covers both 1 and 2.
;   3. Bad-extraction leftovers. The old zip staged everything under a top-level
;      ConjureAlign-<version> folder, so dragging the FOLDER rather than its
;      contents leaves C:\...\VST3\ConjureAlign-1.2.0\. Hosts scan these folders
;      RECURSIVELY, so that nested copy really does get loaded. The wildcard
;      cannot match ConjureAlign.vst3 — no hyphen.
;
; Note Inno treats an [InstallDelete] failure as non-fatal and silent, so a
; locked file can still leave a shadow copy behind. That is what
; CloseApplications is there to prevent.
Type: filesandordirs; Name: "{commoncf64}\VST3\ConjureAlign.vst3"
Type: filesandordirs; Name: "{commoncf64}\VST3\ConjureAlign-*"
Type: filesandordirs; Name: "{commoncf64}\CLAP\ConjureAlign.clap"
Type: filesandordirs; Name: "{commoncf64}\CLAP\ConjureAlign-*"

; Per-user plugin folders from the VST3 and CLAP specs. Nothing is ever
; installed there; these are sweep-only, so a hand-copied user-scope copy cannot
; shadow the machine-wide one.
;
; CAVEAT: under an admin-mode installer {localappdata} is the profile of the
; account Setup runs as. When the user is themselves an admin (a UAC consent
; prompt) that is the right profile; when a standard user typed a DIFFERENT
; admin's credentials, it is not, and their own stale copy survives. Accepted —
; the README's diagnostic command finds it.
Type: filesandordirs; Name: "{localappdata}\Programs\Common\VST3\ConjureAlign.vst3"
Type: filesandordirs; Name: "{localappdata}\Programs\Common\VST3\ConjureAlign-*"
Type: filesandordirs; Name: "{localappdata}\Programs\Common\CLAP\ConjureAlign.clap"
Type: filesandordirs; Name: "{localappdata}\Programs\Common\CLAP\ConjureAlign-*"

; 32-bit Common Files. This is a 64-bit DLL, so anything of ours there is dead
; weight that only slows a 32-bit host's scan and produces a scan error.
Type: filesandordirs; Name: "{commoncf32}\VST3\ConjureAlign.vst3"
Type: filesandordirs; Name: "{commoncf32}\VST3\ConjureAlign-*"
Type: filesandordirs; Name: "{commoncf32}\CLAP\ConjureAlign.clap"
Type: filesandordirs; Name: "{commoncf32}\CLAP\ConjureAlign-*"

[UninstallDelete]
; Belt and braces: [Files] entries are removed automatically, but this also
; takes the bundle's directory shell if anything unexpected is left inside.
Type: filesandordirs; Name: "{commoncf64}\VST3\ConjureAlign.vst3"
Type: filesandordirs; Name: "{commoncf64}\CLAP\ConjureAlign.clap"
; Belt and braces only: Inno removes {app} itself at the end. This entry runs
; while unins000.* are still in there, so it cannot fire — and that is exactly
; why there is no publisher folder above {app} for it to leave behind.
Type: dirifempty;     Name: "{app}"

[Messages]
FinishedLabelNoIcons=Setup has finished installing [name].%n%nRestart your DAW and rescan plug-ins if ConjureAlign does not appear. Some hosts cache their plug-in list and need it cleared.

[Code]
{ Preferences live outside the install: src/config.rs writes
  %APPDATA%\ConjureDSP\ConjureAlign\analytics.json plus a sessions\ directory,
  holding the two consent answers, the analytics device id and the crash-session
  markers. Someone reinstalling almost certainly wants those kept; someone
  leaving may well want them gone. So ask, and default to keeping.

  SuppressibleMsgBox, not MsgBox: under /VERYSILENT /SUPPRESSMSGBOXES it returns
  the default without prompting, and the default here is IDNO. A silent
  uninstall therefore never destroys user data, which is also what makes the CI
  smoke test safe to run.

  Only the ConjureAlign child is touched. The parent ConjureDSP\ folder is
  shared with the other ConjureDSP products, so it is removed with RemoveDir,
  which succeeds only when it is already empty.

  LIMITATION, and the reason the message says "for the account running this
  uninstaller": settings are per-user, but this runs elevated. {userappdata}
  is therefore the profile of whoever's credentials Windows accepted, which is
  NOT the invoking user when a standard user typed a separate administrator's
  password. In that case DirExists is false below, no prompt appears, and that
  user's own settings survive untouched. An admin-mode installer cannot reach
  another profile, so the honest fix is to say so rather than silently
  under-report: README.txt names the path so it can be removed by hand.

  Formatting trap when editing the message below: ISPP reads any line whose
  first non-blank character is '#' as a preprocessor directive, so a wrapped
  continuation starting with #13#10 fails the compile with "Unknown
  preprocessor directive". Keep those character codes mid-line. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir, ParentDir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    ParentDir := ExpandConstant('{userappdata}') + '\ConjureDSP';
    DataDir   := ParentDir + '\ConjureAlign';
    if DirExists(DataDir) then
    begin
      if SuppressibleMsgBox(
           'Also remove ConjureAlign''s settings?' + #13#10#13#10 +
           DataDir + #13#10#13#10 +
           'This holds your privacy and update-check answers and the crash-report ' +
           'bookkeeping. Choose No to keep them for a future reinstall.' + #13#10#13#10 +
           'Settings are per-user, and this covers only the account running ' +
           'this uninstaller. Other Windows accounts keep their own copy under ' +
           '%APPDATA%\ConjureDSP\ConjureAlign.',
           mbConfirmation, MB_YESNO, IDNO) = IDYES then
      begin
        DelTree(DataDir, True, True, True);
        { Succeeds only if no other ConjureDSP product left anything behind. }
        RemoveDir(ParentDir);
      end;
    end;
  end;
end;
