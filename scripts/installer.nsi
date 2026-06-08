; BetterWheel NSIS installer
; Per-user install (no UAC), runs the WebView2 bootstrapper if the runtime
; isn't already present. The bootstrapper itself is bundled; it downloads
; the actual WebView2 runtime from Microsoft at install time.
;
; Build via scripts/build-windows.sh installer; do not invoke directly.
;
; Variables provided on the command line:
;   APP_VERSION   semver string (e.g. 2.0.0-pre1)
;   APP_EXE_PATH  absolute path to betterwheel-desktop.exe (x64)
;   WEBVIEW2_BOOTSTRAPPER  absolute path to MicrosoftEdgeWebview2Setup.exe
;   ICON_PATH     absolute path to icon.ico
;   OUT_FILE      absolute path of the installer .exe to produce

!define APP_NAME "BetterWheel"
!define APP_PUBLISHER "Luca Gibelli"
!define APP_REG_ROOT "HKCU"
!define APP_REG_UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\BetterWheel"

!ifndef APP_VERSION
  !define APP_VERSION "0.0.0"
!endif

RequestExecutionLevel user
SetCompressor /SOLID lzma
Unicode true

Name "${APP_NAME} ${APP_VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\BetterWheel"
InstallDirRegKey ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "InstallLocation"
ShowInstDetails show
ShowUninstDetails show

!include "MUI2.nsh"

!define MUI_ICON "${ICON_PATH}"
!define MUI_UNICON "${ICON_PATH}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\betterwheel-desktop.exe"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "BetterWheel" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"

  ; Kill a running instance so the exe can be overwritten. Auto-updates run
  ; this installer silently while the old app is exiting (the Tauri updater
  ; spawns us then exits the app — there can be a brief overlap). A graceful
  ; close won't do: the app intercepts WM_CLOSE to hide instead of quit.
  nsExec::Exec 'taskkill /F /IM betterwheel-desktop.exe'
  Pop $0
  Sleep 400

  File "/oname=betterwheel-desktop.exe" "${APP_EXE_PATH}"

  ; --- WebView2 runtime check ---
  ; Probe both machine-wide and per-user install locations.
  ClearErrors
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${If} $0 == ""
    ReadRegStr $0 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${EndIf}
  ${If} $0 == ""
    ReadRegStr $0 HKCU "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${EndIf}

  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    DetailPrint "Installing Microsoft WebView2 Runtime..."
    File "/oname=MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2_BOOTSTRAPPER}"
    ExecWait '"$INSTDIR\MicrosoftEdgeWebview2Setup.exe" /silent /install' $1
    Delete "$INSTDIR\MicrosoftEdgeWebview2Setup.exe"
    ${If} $1 != 0
      MessageBox MB_ICONEXCLAMATION|MB_OK "WebView2 install returned exit code $1.$\nThe app may not run until WebView2 is installed manually."
    ${EndIf}
  ${Else}
    DetailPrint "WebView2 already present (version $0)"
  ${EndIf}

  ; --- Start Menu shortcut ---
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\betterwheel-desktop.exe" "" "$INSTDIR\betterwheel-desktop.exe"

  ; --- Uninstaller ---
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\betterwheel-desktop.exe"
  WriteRegDWORD ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}" "NoRepair" 1

  ; Relaunch the app after a silent install. The Tauri updater invokes us with
  ; "/S /R /UPDATE ..." — vanilla NSIS only understands /S and ignores the
  ; rest, so implement the /R (relaunch) behavior ourselves. Interactive
  ; installs get the MUI finish-page "run" checkbox instead.
  IfSilent 0 +2
    Exec '"$INSTDIR\betterwheel-desktop.exe"'
SectionEnd

Section "Uninstall"
  ; Note: launch-at-login registry entry (created by the app's autostart
  ; plugin, HKCU\...\Run\BetterWheel) is left alone here; if the app is
  ; running it will re-create on next launch. Could be cleaned explicitly
  ; if desired.
  Delete "$INSTDIR\betterwheel-desktop.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  DeleteRegKey ${APP_REG_ROOT} "${APP_REG_UNINSTALL_KEY}"
SectionEnd
