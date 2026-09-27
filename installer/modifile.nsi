; Windows installer for Modifile.
;
; What it is for: a zip you unpack somewhere is a program Windows knows nothing
; about. This puts Modifile where per-user apps live, gives it a Start menu entry
; (which is also what makes it show up when you type "Modifile" into Windows
; search), and registers it under Settings -> Apps so it can be uninstalled like
; anything else.
;
; Per-user on purpose: %LOCALAPPDATA%\Programs\Modifile needs no admin prompt,
; and it stays writable by the user, which is what lets Modifile's self-update
; keep replacing its own binaries in place after this has installed them.
;
; Built by .github/workflows/release.yml, or by hand:
;   makensis /DVERSION=1.2.3 installer\modifile.nsi
; Optional: /DROOT=<repo root> (default: the parent of this folder),
;           /DOUTFILE=<path of the setup exe to write>.

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "Pass the version: makensis /DVERSION=1.2.3 installer\modifile.nsi"
!endif
!ifndef ROOT
  !define ROOT ".."
!endif
!ifndef OUTFILE
  !define OUTFILE "${ROOT}\dist\modifile-v${VERSION}-x86_64-windows-setup.exe"
!endif

!define NAME "Modifile"
!define PUBLISHER "Gamepro5"
!define URL "https://github.com/Gamepro5/modifile"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${NAME}"
!define BIN "${ROOT}\target\release"

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"
!include "WinMessages.nsh"

Name "${NAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${NAME}"
; An upgrade goes wherever the last install went.
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
BrandingText "${NAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "LegalCopyright" "MIT License"

!define MUI_ICON "${ROOT}\crates\modifile-gui\assets\modifile.ico"
!define MUI_UNICON "${ROOT}\crates\modifile-gui\assets\modifile.ico"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_FINISHPAGE_RUN "$INSTDIR\modifile-gui.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Modifile"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------------------
; PATH, for the command-line half.
;
; Done in PowerShell against the registry rather than with NSIS string
; functions, because the user's Path is REG_EXPAND_SZ: reading it through the
; environment expands %USERPROFILE% and friends, and writing that back would
; quietly bake every other entry into a literal path. The install directory is
; handed over in an environment variable rather than spliced into the command,
; so a user name with an apostrophe in it cannot break the quoting.
;
; Everything but Modifile's own entry is left byte for byte as it was — empty
; entries and a trailing ";" included — so install then uninstall gives back
; exactly the Path you started with.
; ---------------------------------------------------------------------------

!macro EditUserPath ACTION
  System::Call 'Kernel32::SetEnvironmentVariable(t "MODIFILE_INSTDIR", t "$INSTDIR") i'
  System::Call 'Kernel32::SetEnvironmentVariable(t "MODIFILE_PATH_ACTION", t "${ACTION}") i'
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$d = $$env:MODIFILE_INSTDIR.TrimEnd('\'); $$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); $$p = [string]$$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames'); $$new = @($$p -split ';' | Where-Object { $$_.TrimEnd('\') -ne $$d }) -join ';'; if ($$env:MODIFILE_PATH_ACTION -eq 'add') { if ($$new -eq '') { $$new = $$d } elseif ($$new.EndsWith(';')) { $$new += $$d + ';' } else { $$new += ';' + $$d } }; if ($$new -cne $$p) { $$k.SetValue('Path', $$new, 'ExpandString') }"`
  Pop $0
  ; Tell Explorer, so terminals opened from now on see the change without
  ; signing out and back in.
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
!macroend

; ---------------------------------------------------------------------------
; A running copy.
;
; Windows will not overwrite a running executable, but it will rename one. So
; the old binary is moved aside and the new one written in its place — the same
; dance self-update does, and the leftover uses a name Modifile's own startup
; cleanup already sweeps (anything "modifile*.old-*").
; ---------------------------------------------------------------------------

!macro ParkRunning FILE
  ${If} ${FileExists} "$INSTDIR\${FILE}"
    Delete "$INSTDIR\${FILE}"
    ${If} ${FileExists} "$INSTDIR\${FILE}"
      Rename "$INSTDIR\${FILE}" "$INSTDIR\${FILE}.old-installer"
    ${EndIf}
  ${EndIf}
!macroend

; ---------------------------------------------------------------------------
; Sections
; ---------------------------------------------------------------------------

Section "Modifile" SecCore
  SectionIn RO
  SetShellVarContext current
  SetOutPath "$INSTDIR"

  ; A leftover from an earlier install that could not be removed at the time.
  Delete "$INSTDIR\*.old-installer"

  !insertmacro ParkRunning "modifile-gui.exe"
  !insertmacro ParkRunning "modifile.exe"
  File "${BIN}\modifile-gui.exe"
  File "${BIN}\modifile.exe"
  File "${ROOT}\README.md"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; The Start menu entry is also what Windows search finds.
  CreateShortcut "$SMPROGRAMS\${NAME}.lnk" "$INSTDIR\modifile-gui.exe" "" "$INSTDIR\modifile-gui.exe" 0 SW_SHOWNORMAL "" "Mod manager for games"

  ; Settings -> Apps (and the old Add or Remove Programs).
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\modifile-gui.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTKEY}" "URLInfoAbout" "${URL}"
  WriteRegStr HKCU "${UNINSTKEY}" "HelpLink" "${URL}"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"
SectionEnd

Section "Add modifile to PATH" SecPath
  !insertmacro EditUserPath "add"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  SetShellVarContext current
  CreateShortcut "$DESKTOP\${NAME}.lnk" "$INSTDIR\modifile-gui.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore} "Modifile itself: the app, the modifile command, a Start menu entry, and an entry in Settings > Apps to uninstall it."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecPath} "Lets you type modifile in any new terminal."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "A Modifile icon on your desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
; Uninstall
;
; Your profiles, downloads and settings live in %APPDATA%\modifile, not here,
; and are left alone — as are mods already deployed into game folders, which
; belong to those games now. Uninstalling the program is not a request to
; delete anyone's data.
; ---------------------------------------------------------------------------

Section "Uninstall"
  SetShellVarContext current

  ; The one thing that can stop an uninstall is Modifile still being open.
  retry:
  ClearErrors
  Delete "$INSTDIR\modifile-gui.exe"
  Delete "$INSTDIR\modifile.exe"
  ${If} ${Errors}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Modifile is still running. Close it, then press Retry." /SD IDCANCEL IDRETRY retry
    Abort "Modifile is still running."
  ${EndIf}

  ; Leftovers from self-updates and from installing over a running copy.
  Delete "$INSTDIR\modifile*.old-*"
  Delete "$INSTDIR\modifile*.new"
  Delete "$INSTDIR\.modifile-write-test-*"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${NAME}.lnk"
  Delete "$DESKTOP\${NAME}.lnk"
  !insertmacro EditUserPath "remove"
  DeleteRegKey HKCU "${UNINSTKEY}"
SectionEnd
