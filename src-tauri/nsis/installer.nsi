; ═══════════════════════════════════════════════════════════════════════════
; Speechek — NSIS installer/uninstaller template (custom Tauri template)
;
; Forked from the Tauri NSIS template of tauri-cli v2.12.0:
;   https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.12.0/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi
; Upstream copyright © 2019-2024 Tauri Programme within The Commons
; Conservancy; upstream licensed under Apache-2.0 OR MIT (see the
; LICENSE/third-party notices of this repository). The Handlebars placeholders,
; the VIProductVersion/VIAddVersionKey block, the MUI page macros, the WebView2
; bootstrap and the helper macros from utils.nsh are preserved so that
; tauri-bundler 2.12.0 can render this file unchanged into
; <target>/<profile>/nsis/<arch>/installer.nsi.
;
; Deliberate deviations from the upstream template (each required by the
; Speechek release plan §3):
;   * /S (silent) and /P (passive) runs are refused in .onInit/un.onInit with
;     exit code 2 and a console diagnostic, before any side effect: only the
;     interactive installer can obtain the consent that is needed to close a
;     running Speechek instance and to replace installed files;
;   * the maintenance page, the old-uninstaller-before-upgrade flow and the
;     silent/passive shortcut and /R paths are removed;
;   * the installed instance is closed with a warning and a shared five second
;     grace period, then force-terminated only through handles that were
;     verified (image path, creation time, session, user SID) beforehand;
;     no taskkill, no RmShutdown(RmForceShutdown) of arbitrary processes;
;   * the embedded payload is staged and verified (SHA-256 + file version)
;     before the installed files are touched, and a failed replacement is
;     rolled back to the previous files and registry values;
;   * fixed per-user install directory, one Start Menu shortcut, and opt-in
;     desktop shortcut / HKCU Run entry only on a fresh installation;
;   * the uninstaller deletes only the known installed files and asks with an
;     unchecked checkbox before removing settings and API keys.
; ═══════════════════════════════════════════════════════════════════════════

Unicode true
ManifestDPIAware true
; PerMonitorV2 was added for Windows 10 1607+; older systems ignore it and keep
; ManifestDPIAware true.
; https://github.com/tauri-apps/tauri/pull/10106
ManifestDPIAwareness PerMonitorV2

!if "{{compression}}" == "none"
  SetCompress off
!else
  ; Set the compression algorithm. We default to LZMA.
  SetCompressor /SOLID "{{compression}}"
!endif

; Keep above !include to stay ahead of any plugin command
; see https://github.com/tauri-apps/tauri/pull/15422#discussion_r3289239624
{{#if signed_plugins_path}}
!addplugindir "{{signed_plugins_path}}"
{{/if}}

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "utils.nsh"
!include "FileAssociation.nsh"
!include "Win\COM.nsh"
!include "Win\Propkey.nsh"
!include "Win\RestartManager.nsh"
!include "StrFunc.nsh"
${StrCase}
${StrLoc}

{{#if installer_hooks}}
!include "{{installer_hooks}}"
{{/if}}

; Speechek helpers and the generated payload description. Both files are placed
; next to the rendered installer.nsi, one level above nsis/<arch> (that
; directory is wiped by tauri-bundler before every bundle run). The separator
; must be a backslash: makensis does not collapse a forward-slash "/.." and
; fails with "could not find" on the resolved path.
!include "${__FILEDIR__}\..\..\common.nsh"
!include "${__FILEDIR__}\..\..\payload.nsh"


!define WEBVIEW2APPGUID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define HOMEPAGE "{{homepage}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define UNINSTALLERICON "{{uninstaller_icon}}"
!define UNINSTALLERHEADERIMAGE "{{uninstaller_header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define BUNDLEID "{{bundle_id}}"
!define COPYRIGHT "{{copyright}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define ADDITIONALPLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define INSTALLWEBVIEW2MODE "{{install_webview2_mode}}"
!define WEBVIEW2INSTALLERARGS "{{webview2_installer_args}}"
!define WEBVIEW2BOOTSTRAPPERPATH "{{webview2_bootstrapper_path}}"
!define WEBVIEW2INSTALLERPATH "{{webview2_installer_path}}"
!define MINIMUMWEBVIEW2VERSION "{{minimum_webview2_version}}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUKEY "Software\${MANUFACTURER}"
!define MANUPRODUCTKEY "${MANUKEY}\${PRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"
!define STARTMENUFOLDER "{{start_menu_folder}}"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"
; ── Payload contract ───────────────────────────────────────────────────────
; payload.nsh is generated by scripts/build-release.ps1 (release) and
; scripts/build-test-installer.ps1 (test fixture) and defines at least:
;   SPEECHEK_PAYLOAD_EXE, SPEECHEK_PAYLOAD_EXE_SHA256
;   SPEECHEK_PAYLOAD_VERSION, SPEECHEK_PAYLOAD_VERSIONWITHBUILD
;   SPEECHEK_PAYLOAD_LICENSE, SPEECHEK_PAYLOAD_LICENSE_SHA256
;   SPEECHEK_PAYLOAD_NOTICES, SPEECHEK_PAYLOAD_NOTICES_SHA256
;   SPEECHEK_TEST_INSTALLER (1 only for the app.speechek.test identity)
!ifndef SPEECHEK_PAYLOAD_EXE
  !error "payload.nsh does not define SPEECHEK_PAYLOAD_EXE: generate it with scripts/build-release.ps1"
!endif
!ifndef SPEECHEK_PAYLOAD_LICENSE
  !error "payload.nsh does not define SPEECHEK_PAYLOAD_LICENSE: generate it with scripts/build-release.ps1"
!endif
!ifndef SPEECHEK_PAYLOAD_NOTICES
  !error "payload.nsh does not define SPEECHEK_PAYLOAD_NOTICES: generate it with scripts/build-release.ps1"
!endif
!ifndef SPEECHEK_TEST_INSTALLER
  !define SPEECHEK_TEST_INSTALLER 0
!endif
!if ${SPEECHEK_TEST_INSTALLER} = 1
  !if "${BUNDLEID}" != "app.speechek.test"
    !error "SPEECHEK_TEST_INSTALLER=1 is only allowed for the BUNDLEID app.speechek.test"
  !endif
!endif
!if "${VERSION}" != "${SPEECHEK_PAYLOAD_VERSION}"
  !error "the installer version ${VERSION} does not match the payload version ${SPEECHEK_PAYLOAD_VERSION}"
!endif
!if "${VERSIONWITHBUILD}" != "${SPEECHEK_PAYLOAD_VERSIONWITHBUILD}"
  !error "the installer version ${VERSIONWITHBUILD} does not match the payload version ${SPEECHEK_PAYLOAD_VERSIONWITHBUILD}"
!endif

; This fork only ships a per-user installer with a root Start Menu shortcut.
!if "${INSTALLMODE}" != "currentUser"
  !error "the Speechek installer template supports the currentUser install mode only"
!endif
!if "${STARTMENUFOLDER}" != ""
  !error "the Speechek installer template creates the root Start Menu shortcut; startMenuFolder must not be set"
!endif

; ── Speechek paths and thresholds ─────────────────────────────────────────
!define SPEECHEK_INSTALL_DIR "$LOCALAPPDATA\${PRODUCTNAME}"
!define SPEECHEK_INSTALLED_EXE "$INSTDIR\${MAINBINARYNAME}.exe"
!define SPEECHEK_LEGACY_LINK "$LOCALAPPDATA\${MAINBINARYNAME}.exe"
; The data-deletion target must match the real runtime profile directory, which
; is not derived from the display product name: production keeps its profile in
; %APPDATA%\Speechek while the app.speechek.test identity uses
; %APPDATA%\Speechek-Test (see src-tauri/src/profile.rs).
!if ${SPEECHEK_TEST_INSTALLER} = 1
  !define SPEECHEK_PROFILE_DIR "$APPDATA\Speechek-Test"
!else
  !define SPEECHEK_PROFILE_DIR "$APPDATA\Speechek"
!endif
!define SPEECHEK_WEBVIEW_ROAMING "$APPDATA\${BUNDLEID}"
!define SPEECHEK_WEBVIEW_LOCAL "$LOCALAPPDATA\${BUNDLEID}"
!define SPEECHEK_BACKUP_DIR "$PLUGINSDIR\speechek-backup"
!define SPEECHEK_BACKUP_INI "$PLUGINSDIR\speechek-backup.ini"
!define SPEECHEK_STAGE_DIR "$PLUGINSDIR\speechek-stage"
!define SPEECHEK_FILE_ATTRIBUTE_REPARSE_POINT 0x400
!define SPEECHEK_FILE_ATTRIBUTE_DIRECTORY 0x10
!define SPEECHEK_MIN_WINDOWS_BUILD 22000
!define SPEECHEK_VER_NT_WORKSTATION 1
!define SPEECHEK_IMAGE_FILE_MACHINE_AMD64 0x8664
!define SPEECHEK_OSVERSIONINFOEXW_SIZE 284
!define SPEECHEK_RUN_MAX_LENGTH 260

; ── Registry transaction helpers ───────────────────────────────────────────
; The registry values owned by this installer are saved before they are
; written. An empty saved value is treated as "the value did not exist", which
; is accurate for this fixed value set (none of these values is legitimately
; empty).
!macro SpeechekBackupRegStr name
  ReadRegStr $0 HKCU "${UNINSTKEY}" "${name}"
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "regstr" "${name}" "$0"
!macroend

!macro SpeechekBackupRegDword name
  ClearErrors
  ReadRegDWORD $0 HKCU "${UNINSTKEY}" "${name}"
  ${If} ${Errors}
    StrCpy $0 ""
  ${EndIf}
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "regdword" "${name}" "$0"
!macroend

!macro SpeechekRestoreRegStr name
  ReadINIStr $0 "${SPEECHEK_BACKUP_INI}" "regstr" "${name}"
  ${If} $0 == ""
    DeleteRegValue HKCU "${UNINSTKEY}" "${name}"
  ${Else}
    WriteRegStr HKCU "${UNINSTKEY}" "${name}" "$0"
  ${EndIf}
!macroend

!macro SpeechekRestoreRegDword name
  ReadINIStr $0 "${SPEECHEK_BACKUP_INI}" "regdword" "${name}"
  ${If} $0 == ""
    DeleteRegValue HKCU "${UNINSTKEY}" "${name}"
  ${Else}
    WriteRegDWORD HKCU "${UNINSTKEY}" "${name}" $0
  ${EndIf}
!macroend

; Aborts the current transaction when the immediately preceding operation set
; the error flag. ${label} is a function-local failure label.
!macro SpeechekOnError label
  ${If} ${Errors}
    Goto ${label}
  ${EndIf}
!macroend

Name "${PRODUCTNAME}"
BrandingText "${COPYRIGHT}"
OutFile "${OUTFILE}"

; We don't actually use this value as default install path,
; it's just for nsis to append the product name folder in the directory selector
; https://nsis.sourceforge.io/Reference/InstallDir
!define PLACEHOLDER_INSTALL_DIR "placeholder\${PRODUCTNAME}"
InstallDir "${PLACEHOLDER_INSTALL_DIR}"

; The installer is per-user: no privilege elevation is requested.
RequestExecutionLevel user

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${PRODUCTNAME}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

; additional plugins
!addplugindir "${ADDITIONALPLUGINSPATH}"

; Installer icon
!if "${INSTALLERICON}" != ""
  !define MUI_ICON "${INSTALLERICON}"
!endif

; Installer sidebar image
!if "${SIDEBARIMAGE}" != ""
  !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; Enable header images for installer and uninstaller pages when either image is configured.
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!else if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!endif

; Installer header image
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_BITMAP "${HEADERIMAGE}"
!endif

; Uninstaller header image
!if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_UNBITMAP "${UNINSTALLERHEADERIMAGE}"
!endif

; Uninstaller icon
!if "${UNINSTALLERICON}" != ""
  !define MUI_UNICON "${UNINSTALLERICON}"
!endif

; Define registry key to store installer language
!define MUI_LANGDLL_REGISTRY_ROOT "HKCU"
!define MUI_LANGDLL_REGISTRY_KEY "${MANUPRODUCTKEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Installer Language"

; ── Variables ──────────────────────────────────────────────────────────────
Var SpeechekMutexAcquired
Var SpeechekFreshInstall
Var SpeechekInstalledVersion
Var SpeechekVersionValid
Var SpeechekNativeMachine
Var SpeechekOsBuild
Var SpeechekOsProductType
Var SpeechekOsInfo
Var SpeechekOptionDesktop
Var SpeechekOptionStartup
Var SpeechekOptionDesktopState
Var SpeechekOptionStartupState
Var SpeechekOptionDesktopHwnd
Var SpeechekOptionStartupHwnd
Var SpeechekExeExisted
Var SpeechekLicenseExisted
Var SpeechekNoticesExisted
Var SpeechekUninstallerExisted
Var SpeechekStartMenuExisted
Var SpeechekDesktopExisted
Var SpeechekActualSha
Var SpeechekRollbackFlag
Var SpeechekRollbackMessage
Var SpeechekUninstallLocation
Var SpeechekLegacyAttr
Var SpeechekDataDir
Var SpeechekDataAllowed
Var SpeechekDataFailed
Var SpeechekDataUnsafe
Var SpeechekDataFailedPath
Var SpeechekDeleteDataState
Var SpeechekDeleteDataCheckbox
Var SpeechekRunValue
Var SpeechekValue
Var SpeechekTrimmed
Var SpeechekStagedVersion
Var SpeechekCounter
Var SpeechekPrecheckText
Var SpeechekBackupFailed
Var SpeechekBackupSrc
Var SpeechekBackupDst
Var SpeechekBackupKey
Var SpeechekTreeMode
Var SpeechekTreeStatus
Var SpeechekUninstallFailed

; ── Installer pages ────────────────────────────────────────────────────────
; 1. Welcome page
!insertmacro MUI_PAGE_WELCOME

; 2. License page (rendered from bundle.licenseFile)
!if "${LICENSE}" != ""
  !insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!endif

; 3. Existing installation check (skipped for a fresh installation)
Page custom PagePrecheck

; 4. First installation options (skipped for updates)
Page custom PageOptions PageOptionsLeave

; 5. Installation page
!insertmacro MUI_PAGE_INSTFILES

; 6. Finish page
; Don't auto jump to finish page after installation page,
; because the installation page has useful info that can be used debug any issues with the installer.
!define MUI_FINISHPAGE_NOAUTOCLOSE
; A single opt-out checkbox to start the application, run only after the whole
; installation succeeded.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "$(speechekRunAfterInstall)"
!define MUI_FINISHPAGE_RUN_FUNCTION SpeechekRunMainBinary
!insertmacro MUI_PAGE_FINISH

; ── Uninstaller pages ──────────────────────────────────────────────────────
; 1. Confirmation page with the opt-in data deletion checkbox
!define /ifndef WS_EX_LAYOUTRTL         0x00400000
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.SpeechekConfirmShow
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.SpeechekConfirmLeave
!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling page
!insertmacro MUI_UNPAGE_INSTFILES

; ── Languages ──────────────────────────────────────────────────────────────
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
  !include "{{this}}"
{{/each}}

; ── Driver functions (installer and uninstaller copies) ───────────────────
!insertmacro SpeechekFunctions "" ""
!insertmacro SpeechekFunctions un. un

; ── Pre-installation check page ────────────────────────────────────────────
Function PagePrecheck
  ; The page only exists to state the detected version; a fresh installation
  ; continues directly to the options page.
  ${If} $SpeechekFreshInstall = 1
    Abort
  ${EndIf}

  !insertmacro MUI_HEADER_TEXT "$(speechekPrecheckHeader)" "$(speechekPrecheckTitle)"

  nsDialogs::Create 1018
  Pop $SpeechekValue
  ${If} $SpeechekValue == error
    Abort
  ${EndIf}
  ; The text is stored in a variable because it contains the version number
  ; that was read from the registry (a $R4 reference inside the LangString).
  ${NSD_CreateLabel} 0 0 100% 60u "$SpeechekPrecheckText"
  Pop $SpeechekValue
  nsDialogs::Show
FunctionEnd

; ── First installation options page ────────────────────────────────────────
Function PageOptions
  ; On an in-place update neither the desktop shortcut nor the autostart entry
  ; is created or rewritten again.
  ${If} $SpeechekFreshInstall = 0
    Abort
  ${EndIf}

  !insertmacro MUI_HEADER_TEXT "$(speechekOptionsHeader)" "$(speechekOptionsTitle)"

  nsDialogs::Create 1018
  Pop $SpeechekValue
  ${If} $SpeechekValue == error
    Abort
  ${EndIf}

  ${NSD_CreateCheckbox} 0 0 100% 12u "$(speechekDesktopShortcutOption)"
  Pop $SpeechekOptionDesktopHwnd
  ${NSD_CreateCheckbox} 0 20u 100% 12u "$(speechekStartupOption)"
  Pop $SpeechekOptionStartupHwnd

  ; Both checkboxes are unchecked by default; when the user navigates back to
  ; this page the choices made so far are restored from memory.
  ${If} $SpeechekOptionDesktop = 1
    ${NSD_Check} $SpeechekOptionDesktopHwnd
  ${EndIf}
  ${If} $SpeechekOptionStartup = 1
    ${NSD_Check} $SpeechekOptionStartupHwnd
  ${EndIf}

  nsDialogs::Show
FunctionEnd

Function PageOptionsLeave
  ${NSD_GetState} $SpeechekOptionDesktopHwnd $SpeechekOptionDesktopState
  ${NSD_GetState} $SpeechekOptionStartupHwnd $SpeechekOptionStartupState
  StrCpy $SpeechekOptionDesktop 0
  StrCpy $SpeechekOptionStartup 0
  ${If} $SpeechekOptionDesktopState = ${BST_CHECKED}
    StrCpy $SpeechekOptionDesktop 1
  ${EndIf}
  ${If} $SpeechekOptionStartupState = ${BST_CHECKED}
    StrCpy $SpeechekOptionStartup 1
  ${EndIf}
FunctionEnd

Function SpeechekRunMainBinary
  ; Launches the freshly installed application in the current user session.
  ; Only reached after the whole installation succeeded.
  nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" ""
FunctionEnd

; ── Installer initialisation ───────────────────────────────────────────────
Function .onInit
  ; Unattended modes are refused before anything else happens.
  !insertmacro SpeechekRejectUnattended

  !insertmacro SetContext
  InitPluginsDir

  !if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif

  ; ── Platform gate: Windows 11 (build >= 22000) on native x64 ─────────────
  ; ntdll::RtlGetVersion reports the real OS version; the documented
  ; GetVersionEx path is subject to the compatibility shim and is not used.
  ; https://learn.microsoft.com/en-us/windows/win32/devnotes/rtlgetversion
  System::Alloc ${SPEECHEK_OSVERSIONINFOEXW_SIZE}
  Pop $SpeechekOsInfo
  ${If} $SpeechekOsInfo = 0
    !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
  ${EndIf}
  System::Call '*$SpeechekOsInfo(&i4 ${SPEECHEK_OSVERSIONINFOEXW_SIZE})'
  System::Call 'ntdll::RtlGetVersion(p $SpeechekOsInfo) i .r0'
  ${If} $0 <> 0
    System::Free $SpeechekOsInfo
    !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
  ${EndIf}
  IntOp $SpeechekCounter $SpeechekOsInfo + 12 ; dwBuildNumber
  System::Call '*$SpeechekCounter(&i4 .r0)'
  StrCpy $SpeechekOsBuild $0
  IntOp $SpeechekCounter $SpeechekOsInfo + 282 ; wProductType
  System::Call '*$SpeechekCounter(&i1 .r0)'
  StrCpy $SpeechekOsProductType $0
  System::Free $SpeechekOsInfo

  ; Native architecture: a 32-bit NSIS bootstrapper running on Windows on ARM
  ; under x64 emulation must not be treated as a supported x64 system.
  System::Call 'kernel32::GetCurrentProcess() p .r0'
  System::Call 'kernel32::IsWow64Process2(p r0, *i .r1, *i .r2) i .r0'
  ${If} $0 = 0
    !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
  ${EndIf}
  StrCpy $SpeechekNativeMachine $2
  ${If} $SpeechekNativeMachine <> ${SPEECHEK_IMAGE_FILE_MACHINE_AMD64}
    !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
  ${EndIf}

  !if ${SPEECHEK_TEST_INSTALLER} = 0
    ; Production installations are Windows 11 client systems only. The test
    ; installer (identity app.speechek.test, CI fixtures only) relaxes this.
    ${If} $SpeechekOsBuild < ${SPEECHEK_MIN_WINDOWS_BUILD}
      !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
    ${EndIf}
    ${If} $SpeechekOsProductType <> ${SPEECHEK_VER_NT_WORKSTATION}
      !insertmacro SpeechekFatalMessage $(speechekPlatformUnsupported)
    ${EndIf}
  !endif

  ; ── One installer/uninstaller of this identity at a time ────────────────
  !insertmacro SpeechekAcquireInstallerMutex $SpeechekMutexAcquired
  ${If} $SpeechekMutexAcquired = 0
    !insertmacro SpeechekFatalMessage $(speechekInstallerRunning)
  ${EndIf}

  ; ── Fixed per-user install directory ────────────────────────────────────
  StrCpy $INSTDIR "${SPEECHEK_INSTALL_DIR}"

  ; ── Existing installation: same version or newer, else refuse ───────────
  StrCpy $SpeechekFreshInstall 1
  ReadRegStr $SpeechekInstalledVersion HKCU "${UNINSTKEY}" "DisplayVersion"
  ${If} $SpeechekInstalledVersion != ""
    Push $SpeechekInstalledVersion
    Call SpeechekVersionValid
    Pop $SpeechekVersionValid
    ${If} $SpeechekVersionValid = 0
      ; A damaged or foreign version record is never guessed at or removed.
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    ; The owner record must describe this fixed installation: the recorded
    ; directory and binary name, and the installed EXE must carry the recorded
    ; version. A stale or damaged record is never guessed at or updated.
    ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "InstallLocation"
    ${If} $SpeechekValue != "$\"$INSTDIR$\""
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "MainBinaryName"
    ${If} $SpeechekValue != "${MAINBINARYNAME}.exe"
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    ${IfNot} ${FileExists} "${SPEECHEK_INSTALLED_EXE}"
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    ClearErrors
    ${GetFileVersion} "${SPEECHEK_INSTALLED_EXE}" $SpeechekValue
    ${If} ${Errors}
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    Push $SpeechekValue
    Call SpeechekTrimTrailingZeros
    Pop $SpeechekValue
    Push $SpeechekInstalledVersion
    Call SpeechekTrimTrailingZeros
    Pop $SpeechekTrimmed
    ${If} $SpeechekValue != $SpeechekTrimmed
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
    ${VersionCompare} "${VERSION}" "$SpeechekInstalledVersion" $SpeechekVersionValid
    ${If} $SpeechekVersionValid = 0
      StrCpy $SpeechekFreshInstall 0 ; same version: in-place reinstall
      StrCpy $SpeechekPrecheckText "$(speechekPrecheckReinstall)"
    ${ElseIf} $SpeechekVersionValid = 1
      StrCpy $SpeechekFreshInstall 0 ; newer installer: in-place upgrade
      StrCpy $R4 "$SpeechekInstalledVersion"
      StrCpy $SpeechekPrecheckText "$(speechekPrecheckUpgrade)"
    ${Else}
      StrCpy $R4 "$SpeechekInstalledVersion"
      StrCpy $R5 "${VERSION}"
      !insertmacro SpeechekFatalMessage $(speechekDowngradeBlocked)
    ${EndIf}
  ${Else}
    ; Without a record that names the installed version nothing is deleted or
    ; overwritten: an existing file in the install directory requires a manual
    ; repair with a current installer.
    ${If} ${FileExists} "${SPEECHEK_INSTALLED_EXE}"
      !insertmacro SpeechekFatalMessage $(speechekRepairRequired)
    ${EndIf}
  ${EndIf}
FunctionEnd

; ── WebView2 runtime (official bootstrapper download) ─────────────────────
Section WebView2
  ; Check if Webview2 is already installed and skip this section
  ${If} ${RunningX64}
    ReadRegStr $4 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${Else}
    ReadRegStr $4 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}
  ${If} $4 == ""
    ReadRegStr $4 HKCU "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}

  ${If} $4 == ""
    ; Webview2 installation
    !if "${INSTALLWEBVIEW2MODE}" == "downloadBootstrapper"
      Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
      DetailPrint "$(webview2Downloading)"
      NSISdl::download "https://go.microsoft.com/fwlink/p/?LinkId=2124703" "$TEMP\MicrosoftEdgeWebview2Setup.exe"
      Pop $0
      ${If} $0 == "success"
        DetailPrint "$(webview2DownloadSuccess)"
      ${Else}
        DetailPrint "$(webview2DownloadError)"
        Abort "$(webview2AbortError)"
      ${EndIf}
      StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
      Goto install_webview2
    !endif

    !if "${INSTALLWEBVIEW2MODE}" == "embedBootstrapper"
      Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
      File "/oname=$TEMP\MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2BOOTSTRAPPERPATH}"
      DetailPrint "$(installingWebview2)"
      StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
      Goto install_webview2
    !endif

    !if "${INSTALLWEBVIEW2MODE}" == "offlineInstaller"
      Delete "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
      File "/oname=$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe" "${WEBVIEW2INSTALLERPATH}"
      DetailPrint "$(installingWebview2)"
      StrCpy $6 "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
      Goto install_webview2
    !endif

    Goto webview2_done

    install_webview2:
      DetailPrint "$(installingWebview2)"
      ; $6 holds the path to the webview2 installer
      ExecWait "$6 ${WEBVIEW2INSTALLERARGS} /install" $1
      ${If} $1 = 0
        DetailPrint "$(webview2InstallSuccess)"
      ${Else}
        DetailPrint "$(webview2InstallError)"
        Abort "$(webview2AbortError)"
      ${EndIf}
    webview2_done:
  ${Else}
    !if "${MINIMUMWEBVIEW2VERSION}" != ""
      ${VersionCompare} "${MINIMUMWEBVIEW2VERSION}" "$4" $R0
      ${If} $R0 = 1
        update_webview:
          DetailPrint "$(installingWebview2)"
          ${If} ${RunningX64}
            ReadRegStr $R1 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate" "path"
          ${Else}
            ReadRegStr $R1 HKLM "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 == ""
            ReadRegStr $R1 HKCU "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 != ""
            ; Chromium updater docs: https://source.chromium.org/chromium/chromium/src/+/main:docs/updater/user_manual.md
            ExecWait `"$R1" /install appguid=${WEBVIEW2APPGUID}&needsadmin=true` $1
            ${If} $1 = 0
              DetailPrint "$(webview2InstallSuccess)"
            ${Else}
              MessageBox MB_ICONEXCLAMATION|MB_ABORTRETRYIGNORE "$(webview2InstallError)" IDIGNORE ignore IDRETRY update_webview
              Quit
              ignore:
            ${EndIf}
          ${EndIf}
      ${EndIf}
    !endif
  ${EndIf}
SectionEnd

; ── Installation ───────────────────────────────────────────────────────────
Section "Install" SEC_INSTALL
  ; The staging and backup directories live in the plug-in directory, which is
  ; created by the installer and removed again when the installer exits.
  InitPluginsDir
  SetOutPath $PLUGINSDIR
  !ifmacrodef NSIS_HOOK_PREINSTALL
    !insertmacro NSIS_HOOK_PREINSTALL
  !endif

  ; Stage the payload and verify it before any installed file is touched.
  SetOutPath "${SPEECHEK_STAGE_DIR}"
  File "/oname=${MAINBINARYNAME}.exe" "${SPEECHEK_PAYLOAD_EXE}"
  File "/oname=LICENSE" "${SPEECHEK_PAYLOAD_LICENSE}"
  File "/oname=THIRD-PARTY-NOTICES.txt" "${SPEECHEK_PAYLOAD_NOTICES}"
  Call SpeechekVerifyStagedPayload

  ; Close the installed instance: user confirmation, then the shared grace
  ; period and, if needed, a force close of the verified process handles.
  Push "${SPEECHEK_INSTALLED_EXE}"
  Call SpeechekCloseInstalled

  ; Remember the current files, shortcuts and values for a possible rollback.
  Call SpeechekBackupInstalled

  ; Replace the payload and verify the written copy.
  Call SpeechekCommitPayload

  ; Registry metadata, Start Menu shortcut and the opt-in desktop/autostart
  ; entries. Any failure rolls the installation back.
  Call SpeechekWriteMetadata

  ; Additional configured payload (resources, external binaries, file
  ; associations, deep links). The Speechek configuration defines none of
  ; these; they are installed after the transaction commit above and are
  ; therefore not part of its rollback.
  {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\{{this}}"
  {{/each}}

  {{#each resources}}
    File /a "/oname={{this.[1]}}" "{{no-escape @key}}"
  {{/each}}

  {{#each binaries}}
    File /a "/oname={{this}}" "{{no-escape @key}}"
  {{/each}}

  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
       !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
    {{/each}}
  {{/each}}

  {{#each deep_link_protocols as |protocol| ~}}
    WriteRegStr HKCU "Software\Classes\{{protocol}}" "URL Protocol" ""
    WriteRegStr HKCU "Software\Classes\{{protocol}}" "" "URL:${BUNDLEID} protocol"
    WriteRegStr HKCU "Software\Classes\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
    WriteRegStr HKCU "Software\Classes\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  {{/each}}

  !ifmacrodef NSIS_HOOK_POSTINSTALL
    !insertmacro NSIS_HOOK_POSTINSTALL
  !endif
SectionEnd

; ── Legacy compatibility link (fresh installations only) ───────────────────
; Tools that still call the old portable path %LOCALAPPDATA%\speechek.exe keep
; working when that path is free. A user file at that path is never deleted or
; overwritten, and this section never runs for an update. Only a symbolic link
; is created (never a hard link, which cannot be told apart from a regular file
; later); ownership of that link is recorded so the uninstaller can remove the
; exact link this installer made.
Section "-SpeechekLegacyLink"
  ${If} $SpeechekFreshInstall <> 1
    Return
  ${EndIf}
  IfFileExists "$INSTDIR\${MAINBINARYNAME}.exe" 0 schk_link_done
  IfFileExists "${SPEECHEK_LEGACY_LINK}" schk_link_done 0
  ; CreateSymbolicLinkW(lpSymlinkFileName, lpTargetFileName, 0)
  System::Call 'kernel32::CreateSymbolicLinkW(ws, ws, i 0) i .r0' "${SPEECHEK_LEGACY_LINK}" "$INSTDIR\${MAINBINARYNAME}.exe"
  ${If} $0 <> 0
    ; Best effort: the link is optional. Its removal is recorded so the
    ; uninstaller removes exactly this link; if the marker cannot be written
    ; the link is removed again instead of leaving an unowned link behind.
    ClearErrors
    WriteRegStr HKCU "${MANUPRODUCTKEY}" "LegacyLink" "symlink"
    ${If} ${Errors}
      Delete "${SPEECHEK_LEGACY_LINK}"
      DetailPrint "Speechek: the legacy compatibility link could not be recorded."
    ${EndIf}
  ${EndIf}
  schk_link_done:
SectionEnd

; ═══ Installer helper functions ═══════════════════════════════════════════

; Verifies the staged payload: file version of the executable and SHA-256 of
; all staged files must match payload.nsh. Aborts the installation on any
; mismatch; nothing installed has been touched at this point.
Function SpeechekVerifyStagedPayload
  ; File version of the staged executable (additional to the hash, never a
  ; replacement for it).
  ${GetFileVersion} "${SPEECHEK_STAGE_DIR}\${MAINBINARYNAME}.exe" $SpeechekStagedVersion
  ${If} ${Errors}
    Goto schk_verify_failed
  ${EndIf}
  Push "$SpeechekStagedVersion"
  Call SpeechekTrimTrailingZeros
  Pop $SpeechekTrimmed
  StrCpy $SpeechekValue "${SPEECHEK_PAYLOAD_VERSIONWITHBUILD}"
  Push "$SpeechekValue"
  Call SpeechekTrimTrailingZeros
  Pop $SpeechekValue
  StrCmp "$SpeechekTrimmed" "$SpeechekValue" 0 schk_verify_failed

  Push "${SPEECHEK_STAGE_DIR}\${MAINBINARYNAME}.exe"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_verify_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_EXE_SHA256}" 0 schk_verify_failed

  Push "${SPEECHEK_STAGE_DIR}\LICENSE"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_verify_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_LICENSE_SHA256}" 0 schk_verify_failed

  Push "${SPEECHEK_STAGE_DIR}\THIRD-PARTY-NOTICES.txt"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_verify_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_NOTICES_SHA256}" 0 schk_verify_failed
  Return

  schk_verify_failed:
    !insertmacro SpeechekFatalMessage $(speechekHashMismatch)
FunctionEnd

; Removes trailing ".0" components so that "0.2.0" and "0.2.0.0" compare equal.
; Stack input: <version>; stack output: <trimmed version>.
Function SpeechekTrimTrailingZeros
  Pop $SpeechekTrimmed
  Push $R0
  schk_trim_loop:
    StrCpy $R0 $SpeechekTrimmed "" -2
    ${If} $R0 == ".0"
      StrCpy $SpeechekTrimmed $SpeechekTrimmed -2
      Goto schk_trim_loop
    ${EndIf}
  Pop $R0
  Push $SpeechekTrimmed
FunctionEnd

; Copies one existing installed file into the transaction backup directory and
; records its SHA-256 digest. Sets $SpeechekBackupFailed when the copy or the
; hash fails, so the caller can abort before replacing anything.
; Stack input: <source path> <backup path> <INI key>.
Function SpeechekBackupFile
  Pop $SpeechekBackupKey
  Pop $SpeechekBackupDst
  Pop $SpeechekBackupSrc
  ClearErrors
  CopyFiles /SILENT "$SpeechekBackupSrc" "$SpeechekBackupDst"
  ${If} ${Errors}
    StrCpy $SpeechekBackupFailed 1
    Return
  ${EndIf}
  IfFileExists "$SpeechekBackupDst" 0 schk_backupfile_failed
  Push "$SpeechekBackupDst"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_backupfile_failed
  ${EndIf}
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "hashes" "$SpeechekBackupKey" "$SpeechekActualSha"
  Return
  schk_backupfile_failed:
    StrCpy $SpeechekBackupFailed 1
FunctionEnd

; Copies every installed file, shortcut and registry value this installer
; manages into the transaction backup directory / INI file and records a
; SHA-256 for each copied file. Aborts before any replacement when a required
; backup could not be taken, so a rollback can always restore the old files.

Function SpeechekBackupInstalled
  CreateDirectory "${SPEECHEK_BACKUP_DIR}"
  StrCpy $SpeechekExeExisted 0
  StrCpy $SpeechekLicenseExisted 0
  StrCpy $SpeechekNoticesExisted 0
  StrCpy $SpeechekUninstallerExisted 0
  StrCpy $SpeechekStartMenuExisted 0
  StrCpy $SpeechekDesktopExisted 0
  StrCpy $SpeechekRollbackMessage ""
  StrCpy $SpeechekRollbackFlag 0
  StrCpy $SpeechekBackupFailed 0

  IfFileExists "$INSTDIR\${MAINBINARYNAME}.exe" 0 schk_backup_exe_done
    StrCpy $SpeechekExeExisted 1
    Push "$INSTDIR\${MAINBINARYNAME}.exe"
    Push "${SPEECHEK_BACKUP_DIR}\main.exe"
    Push "exe"
    Call SpeechekBackupFile
  schk_backup_exe_done:

  IfFileExists "$INSTDIR\LICENSE" 0 schk_backup_license_done
    StrCpy $SpeechekLicenseExisted 1
    Push "$INSTDIR\LICENSE"
    Push "${SPEECHEK_BACKUP_DIR}\LICENSE"
    Push "license"
    Call SpeechekBackupFile
  schk_backup_license_done:

  IfFileExists "$INSTDIR\THIRD-PARTY-NOTICES.txt" 0 schk_backup_notices_done
    StrCpy $SpeechekNoticesExisted 1
    Push "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    Push "${SPEECHEK_BACKUP_DIR}\THIRD-PARTY-NOTICES.txt"
    Push "notices"
    Call SpeechekBackupFile
  schk_backup_notices_done:

  IfFileExists "$INSTDIR\uninstall.exe" 0 schk_backup_uninstaller_done
    StrCpy $SpeechekUninstallerExisted 1
    Push "$INSTDIR\uninstall.exe"
    Push "${SPEECHEK_BACKUP_DIR}\uninstall.exe"
    Push "uninstaller"
    Call SpeechekBackupFile
  schk_backup_uninstaller_done:

  IfFileExists "$SMPROGRAMS\${PRODUCTNAME}.lnk" 0 schk_backup_startmenu_done
    StrCpy $SpeechekStartMenuExisted 1
    Push "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    Push "${SPEECHEK_BACKUP_DIR}\startmenu.lnk"
    Push "startmenu"
    Call SpeechekBackupFile
  schk_backup_startmenu_done:

  IfFileExists "$DESKTOP\${PRODUCTNAME}.lnk" 0 schk_backup_desktop_done
    StrCpy $SpeechekDesktopExisted 1
    Push "$DESKTOP\${PRODUCTNAME}.lnk"
    Push "${SPEECHEK_BACKUP_DIR}\desktop.lnk"
    Push "desktop"
    Call SpeechekBackupFile
  schk_backup_desktop_done:

  ; Registry values owned by this installer. An empty value is treated as a
  ; value that did not exist, which is accurate for this fixed value set.
  !insertmacro SpeechekBackupRegStr "DisplayName"
  !insertmacro SpeechekBackupRegStr "DisplayIcon"
  !insertmacro SpeechekBackupRegStr "DisplayVersion"
  !insertmacro SpeechekBackupRegStr "Publisher"
  !insertmacro SpeechekBackupRegStr "InstallLocation"
  !insertmacro SpeechekBackupRegStr "UninstallString"
  !insertmacro SpeechekBackupRegStr "MainBinaryName"
  !insertmacro SpeechekBackupRegStr "URLInfoAbout"
  !insertmacro SpeechekBackupRegStr "URLUpdateInfo"
  !insertmacro SpeechekBackupRegStr "HelpLink"
  !insertmacro SpeechekBackupRegDword "NoModify"
  !insertmacro SpeechekBackupRegDword "NoRepair"
  !insertmacro SpeechekBackupRegDword "EstimatedSize"

  ReadRegStr $SpeechekValue HKCU "${MANUPRODUCTKEY}" ""
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "values" "manudir" "$SpeechekValue"
  ReadRegStr $SpeechekValue HKCU "${RUNKEY}" "${PRODUCTNAME}"
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "values" "run" "$SpeechekValue"
  ReadRegStr $SpeechekValue HKCU "${MANUPRODUCTKEY}" "LegacyLink"
  WriteINIStr "${SPEECHEK_BACKUP_INI}" "values" "legacylink" "$SpeechekValue"

  ; A required backup that could not be taken must abort before any installed
  ; file is replaced: the rollback data is what protects the old version.
  ${If} $SpeechekBackupFailed = 1
    !insertmacro SpeechekFatalMessage $(speechekBackupFailed)
  ${EndIf}
FunctionEnd

; Copies the staged payload into the installation directory and verifies the
; written executable.
Function SpeechekCommitPayload
  CreateDirectory "$INSTDIR"
  SetOutPath "$INSTDIR"
  ClearErrors
  CopyFiles /SILENT "${SPEECHEK_STAGE_DIR}\${MAINBINARYNAME}.exe" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SpeechekOnError schk_commit_failed
  ClearErrors
  CopyFiles /SILENT "${SPEECHEK_STAGE_DIR}\LICENSE" "$INSTDIR\LICENSE"
  !insertmacro SpeechekOnError schk_commit_failed
  ClearErrors
  CopyFiles /SILENT "${SPEECHEK_STAGE_DIR}\THIRD-PARTY-NOTICES.txt" "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  !insertmacro SpeechekOnError schk_commit_failed
  ClearErrors
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ${If} ${Errors}
    Goto schk_commit_failed
  ${EndIf}
  IfFileExists "$INSTDIR\uninstall.exe" 0 schk_commit_failed

  ; Verify every installed payload file byte for byte, not only the executable:
  ; a failed or partial notice copy must not be reported as success.
  Push "$INSTDIR\${MAINBINARYNAME}.exe"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_commit_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_EXE_SHA256}" 0 schk_commit_failed

  Push "$INSTDIR\LICENSE"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_commit_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_LICENSE_SHA256}" 0 schk_commit_failed

  Push "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_commit_failed
  ${EndIf}
  StrCmp "$SpeechekActualSha" "${SPEECHEK_PAYLOAD_NOTICES_SHA256}" 0 schk_commit_failed
  Return

  schk_commit_failed:
    Call SpeechekRollbackInstall
FunctionEnd

; Writes the registry metadata, the Start Menu shortcut and the opt-in desktop
; shortcut / autostart entry. Rolls the installation back on failure.
Function SpeechekWriteMetadata
  ; Start Menu shortcut: created on a fresh install and refreshed in place on
  ; an update, always at the same stable path. It is a required output; a
  ; failure rolls the whole transaction back.
  ClearErrors
  CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SpeechekOnError schk_metadata_failed
  IfFileExists "$SMPROGRAMS\${PRODUCTNAME}.lnk" 0 schk_metadata_failed
  !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"

  ; Desktop shortcut: fresh installation and opt-in only. Never created,
  ; rewritten or restored again on an update.
  ${If} $SpeechekFreshInstall = 1
  ${AndIf} $SpeechekOptionDesktop = 1
    ClearErrors
    CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SpeechekOnError schk_metadata_failed
    IfFileExists "$DESKTOP\${PRODUCTNAME}.lnk" 0 schk_metadata_failed
    !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}

  ; Autostart: fresh installation and opt-in only, HKCU Run with a quoted
  ; command line. No RunOnce, no scheduled task, no service.
  ${If} $SpeechekFreshInstall = 1
  ${AndIf} $SpeechekOptionStartup = 1
    StrCpy $SpeechekRunValue "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
    StrLen $SpeechekValue "$SpeechekRunValue"
    ${If} $SpeechekValue > ${SPEECHEK_RUN_MAX_LENGTH}
      StrCpy $SpeechekRollbackMessage "$(speechekRunValueTooLong)"
      Goto schk_metadata_failed
    ${EndIf}
    ClearErrors
    WriteRegStr HKCU "${RUNKEY}" "${PRODUCTNAME}" "$SpeechekRunValue"
    !insertmacro SpeechekOnError schk_metadata_failed
    ReadRegStr $SpeechekValue HKCU "${RUNKEY}" "${PRODUCTNAME}"
    ${If} $SpeechekValue != $SpeechekRunValue
      Goto schk_metadata_failed
    ${EndIf}
  ${EndIf}

  ; Registry information for add/remove programs. Every required value is
  ; checked right after it is written.
  ClearErrors
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegStr HKCU "${UNINSTKEY}" "MainBinaryName" "${MAINBINARYNAME}.exe"
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" "1"
  !insertmacro SpeechekOnError schk_metadata_failed
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" "1"
  !insertmacro SpeechekOnError schk_metadata_failed

  ; Independently confirm the ownership-critical values landed.
  ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "InstallLocation"
  ${If} $SpeechekValue != "$\"$INSTDIR$\""
    Goto schk_metadata_failed
  ${EndIf}
  ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "MainBinaryName"
  ${If} $SpeechekValue != "${MAINBINARYNAME}.exe"
    Goto schk_metadata_failed
  ${EndIf}
  ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "DisplayVersion"
  ${If} $SpeechekValue != "${VERSION}"
    Goto schk_metadata_failed
  ${EndIf}
  ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "UninstallString"
  ${If} $SpeechekValue != "$\"$INSTDIR\uninstall.exe$\""
    Goto schk_metadata_failed
  ${EndIf}

  ${GetSize} "$INSTDIR" "/M=uninstall.exe /S=0K /G=0" $0 $1 $2
  IntOp $0 $0 + ${ESTIMATEDSIZE}
  IntFmt $0 "0x%08X" $0
  ClearErrors
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"
  !insertmacro SpeechekOnError schk_metadata_failed

  !if "${HOMEPAGE}" != ""
    ClearErrors
    WriteRegStr HKCU "${UNINSTKEY}" "URLInfoAbout" "${HOMEPAGE}"
    !insertmacro SpeechekOnError schk_metadata_failed
    WriteRegStr HKCU "${UNINSTKEY}" "URLUpdateInfo" "${HOMEPAGE}"
    !insertmacro SpeechekOnError schk_metadata_failed
    WriteRegStr HKCU "${UNINSTKEY}" "HelpLink" "${HOMEPAGE}"
    !insertmacro SpeechekOnError schk_metadata_failed
  !endif

  ; Saved for the uninstaller, which reads the installation directory from here.
  ClearErrors
  WriteRegStr HKCU "${MANUPRODUCTKEY}" "" "$INSTDIR"
  !insertmacro SpeechekOnError schk_metadata_failed
  Return

  schk_metadata_failed:
    Call SpeechekRollbackInstall
FunctionEnd

; Restores one file from the transaction backup directory and verifies that the
; restored bytes match the digest recorded when the backup was taken. Sets
; $SpeechekRollbackFlag when the file could not be restored exactly.
; Stack input: <backup path> <destination path> <INI key>.
Function SpeechekRestoreFile
  Pop $SpeechekBackupKey
  Pop $SpeechekBackupDst
  Pop $SpeechekBackupSrc
  ClearErrors
  CopyFiles /SILENT "$SpeechekBackupSrc" "$SpeechekBackupDst"
  ${If} ${Errors}
    StrCpy $SpeechekRollbackFlag 1
    Return
  ${EndIf}
  IfFileExists "$SpeechekBackupDst" 0 schk_restorefile_failed
  Push "$SpeechekBackupDst"
  Call SpeechekSha256File
  Pop $SpeechekActualSha
  ${If} ${Errors}
    Goto schk_restorefile_failed
  ${EndIf}
  ReadINIStr $SpeechekValue "${SPEECHEK_BACKUP_INI}" "hashes" "$SpeechekBackupKey"
  ${If} $SpeechekActualSha != $SpeechekValue
    Goto schk_restorefile_failed
  ${EndIf}
  Return
  schk_restorefile_failed:
    StrCpy $SpeechekRollbackFlag 1
FunctionEnd

; Restores the previous files, shortcuts and registry values and reports the
; result: $SpeechekRollbackFlag is 0 when the previous state was restored and 1
; when it could not be restored completely. The caller never continues after a
; failure.
Function SpeechekRollbackInstall
  StrCpy $SpeechekRollbackFlag 0

  ; Executable
  ${If} $SpeechekExeExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\main.exe"
    Push "$INSTDIR\${MAINBINARYNAME}.exe"
    Push "exe"
    Call SpeechekRestoreFile
  ${Else}
    ClearErrors
    Delete "$INSTDIR\${MAINBINARYNAME}.exe"
    ${If} ${Errors}
      StrCpy $SpeechekRollbackFlag 1
    ${EndIf}
  ${EndIf}

  ; License and third-party notices
  ${If} $SpeechekLicenseExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\LICENSE"
    Push "$INSTDIR\LICENSE"
    Push "license"
    Call SpeechekRestoreFile
  ${Else}
    ClearErrors
    Delete "$INSTDIR\LICENSE"
    ${If} ${Errors}
      StrCpy $SpeechekRollbackFlag 1
    ${EndIf}
  ${EndIf}
  ${If} $SpeechekNoticesExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\THIRD-PARTY-NOTICES.txt"
    Push "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    Push "notices"
    Call SpeechekRestoreFile
  ${Else}
    ClearErrors
    Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    ${If} ${Errors}
      StrCpy $SpeechekRollbackFlag 1
    ${EndIf}
  ${EndIf}

  ; Uninstaller
  ${If} $SpeechekUninstallerExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\uninstall.exe"
    Push "$INSTDIR\uninstall.exe"
    Push "uninstaller"
    Call SpeechekRestoreFile
  ${Else}
    ClearErrors
    Delete "$INSTDIR\uninstall.exe"
    ${If} ${Errors}
      StrCpy $SpeechekRollbackFlag 1
    ${EndIf}
  ${EndIf}

  ; Shortcuts
  ${If} $SpeechekStartMenuExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\startmenu.lnk"
    Push "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    Push "startmenu"
    Call SpeechekRestoreFile
  ${Else}
    Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  ${EndIf}
  ${If} $SpeechekDesktopExisted = 1
    Push "${SPEECHEK_BACKUP_DIR}\desktop.lnk"
    Push "$DESKTOP\${PRODUCTNAME}.lnk"
    Push "desktop"
    Call SpeechekRestoreFile
  ${Else}
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}

  ; Registry values
  !insertmacro SpeechekRestoreRegStr "DisplayName"
  !insertmacro SpeechekRestoreRegStr "DisplayIcon"
  !insertmacro SpeechekRestoreRegStr "DisplayVersion"
  !insertmacro SpeechekRestoreRegStr "Publisher"
  !insertmacro SpeechekRestoreRegStr "InstallLocation"
  !insertmacro SpeechekRestoreRegStr "UninstallString"
  !insertmacro SpeechekRestoreRegStr "MainBinaryName"
  !insertmacro SpeechekRestoreRegStr "URLInfoAbout"
  !insertmacro SpeechekRestoreRegStr "URLUpdateInfo"
  !insertmacro SpeechekRestoreRegStr "HelpLink"
  !insertmacro SpeechekRestoreRegDword "NoModify"
  !insertmacro SpeechekRestoreRegDword "NoRepair"
  !insertmacro SpeechekRestoreRegDword "EstimatedSize"

  ReadINIStr $SpeechekValue "${SPEECHEK_BACKUP_INI}" "values" "manudir"
  ${If} $SpeechekValue == ""
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" ""
  ${Else}
    WriteRegStr HKCU "${MANUPRODUCTKEY}" "" "$SpeechekValue"
  ${EndIf}

  ReadINIStr $SpeechekValue "${SPEECHEK_BACKUP_INI}" "values" "run"
  ${If} $SpeechekValue == ""
    DeleteRegValue HKCU "${RUNKEY}" "${PRODUCTNAME}"
  ${Else}
    WriteRegStr HKCU "${RUNKEY}" "${PRODUCTNAME}" "$SpeechekValue"
  ${EndIf}

  ReadINIStr $SpeechekValue "${SPEECHEK_BACKUP_INI}" "values" "legacylink"
  ${If} $SpeechekValue == ""
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" "LegacyLink"
  ${Else}
    WriteRegStr HKCU "${MANUPRODUCTKEY}" "LegacyLink" "$SpeechekValue"
  ${EndIf}

  ; The installation directory only goes away when it was created by this run
  ; and no foreign files were placed in it (RMDir is not recursive on purpose).
  ${If} $SpeechekFreshInstall = 1
    RMDir "$INSTDIR"
  ${EndIf}

  ${If} $SpeechekRollbackFlag = 1
    MessageBox MB_OK|MB_ICONSTOP "$(speechekRollbackFailed)"
  ${ElseIf} $SpeechekRollbackMessage != ""
    MessageBox MB_OK|MB_ICONSTOP "$SpeechekRollbackMessage"
  ${Else}
    MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekRollbackDone)"
  ${EndIf}
  SetErrorLevel 2
  Abort
FunctionEnd

; ═══ Uninstaller ══════════════════════════════════════════════════════════

; The uninstaller deliberately has no Windows/architecture gate: an existing
; installation must remain removable even if the machine no longer matches the
; installation requirements.
Function un.onInit
  !insertmacro SpeechekRejectUnattended
  !insertmacro SetContext
  InitPluginsDir
  !insertmacro MUI_UNGETLANGUAGE

  !insertmacro SpeechekAcquireInstallerMutex $SpeechekMutexAcquired
  ${If} $SpeechekMutexAcquired = 0
    !insertmacro SpeechekFatalMessage $(speechekInstallerRunning)
  ${EndIf}

  ; The uninstall target is always the fixed canonical install directory. A
  ; recorded directory that points somewhere else is a damaged or foreign
  ; record; the uninstall is refused instead of deleting another directory.
  StrCpy $INSTDIR "${SPEECHEK_INSTALL_DIR}"
  ReadRegStr $SpeechekUninstallLocation HKCU "${MANUPRODUCTKEY}" ""
  ${If} $SpeechekUninstallLocation != ""
  ${AndIf} $SpeechekUninstallLocation != "$INSTDIR"
    !insertmacro SpeechekFatalMessage $(speechekUninstallEntryKept)
  ${EndIf}
FunctionEnd

; Adds the opt-in "Delete settings and API keys" checkbox to the confirmation
; page. It is unchecked by default; the previous choice is shown when the user
; navigates away and back.
Function un.SpeechekConfirmShow
  ${If} $SpeechekDeleteDataState = 1
    StrCpy $1 1
  ${Else}
    StrCpy $1 0
  ${EndIf}
  FindWindow $0 "#32770" "" $HWNDPARENT ; inner dialog
  System::Call "user32::GetDpiForWindow(p r0) i .r2"
  ${If} $(^RTL) = 1
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE} | ${WS_EX_LAYOUTRTL}"
    IntOp $4 50 * $2
  ${Else}
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE}"
    IntOp $4 0 * $2
  ${EndIf}
  IntOp $5 100 * $2
  IntOp $6 400 * $2
  IntOp $7 25 * $2
  IntOp $4 $4 / 96
  IntOp $5 $5 / 96
  IntOp $6 $6 / 96
  IntOp $7 $7 / 96
  System::Call 'user32::CreateWindowEx(i r3, w "${__NSD_CheckBox_CLASS}", w "$(speechekDeleteData)", i ${__NSD_CheckBox_STYLE}, i r4, i r5, i r6, i r7, p r0, i0, i0, i0) i .s'
  Pop $SpeechekDeleteDataCheckbox
  ${If} $1 = 1
    SendMessage $SpeechekDeleteDataCheckbox ${BM_SETCHECK} ${BST_CHECKED} 0
  ${EndIf}
  SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $0
  SendMessage $SpeechekDeleteDataCheckbox ${WM_SETFONT} $0 1
FunctionEnd

Function un.SpeechekConfirmLeave
  SendMessage $SpeechekDeleteDataCheckbox ${BM_GETCHECK} 0 0 $SpeechekDeleteDataState
FunctionEnd

Section Uninstall
  !ifmacrodef NSIS_HOOK_PREUNINSTALL
    !insertmacro NSIS_HOOK_PREUNINSTALL
  !endif

  ; Same warning and grace/force sequence as in the installer, scoped to the
  ; installed executable.
  Push "${SPEECHEK_INSTALLED_EXE}"
  Call un.SpeechekCloseInstalled

  ; Known files of the installed version only; foreign files in the directory
  ; are left in place (RMDir below is deliberately not recursive). A failure
  ; to delete a managed file keeps the uninstall record so the installation
  ; can still be repaired or removed later.
  StrCpy $SpeechekUninstallFailed 0
  ClearErrors
  Delete "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SpeechekOnError schk_un_delete_error
  ClearErrors
  Delete "$INSTDIR\LICENSE"
  !insertmacro SpeechekOnError schk_un_delete_error
  ClearErrors
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  !insertmacro SpeechekOnError schk_un_delete_error
  ClearErrors
  Delete "$INSTDIR\uninstall.exe"
  !insertmacro SpeechekOnError schk_un_delete_error

  ; Additional configured payload; no Speechek configuration uses these.
  {{#each resources}}
    ClearErrors
    Delete "$INSTDIR\{{this.[1]}}"
    !insertmacro SpeechekOnError schk_un_delete_error
  {{/each}}
  {{#each binaries}}
    ClearErrors
    Delete "$INSTDIR\{{this}}"
    !insertmacro SpeechekOnError schk_un_delete_error
  {{/each}}
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
      !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
    {{/each}}
  {{/each}}
  {{#each deep_link_protocols as |protocol| ~}}
    ReadRegStr $R7 HKCU "Software\Classes\{{protocol}}\shell\open\command" ""
    ${If} $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
      DeleteRegKey HKCU "Software\Classes\{{protocol}}"
    ${EndIf}
  {{/each}}
  {{#each resources_ancestors}}
    RMDir /REBOOTOK "$INSTDIR\{{this}}"
  {{/each}}
  RMDir "$INSTDIR"
  Goto schk_un_files_done
  schk_un_delete_error:
    StrCpy $SpeechekUninstallFailed 1
  schk_un_files_done:

  ; Shortcuts and the autostart entry, independently of the data checkbox.
  !insertmacro DeleteAppUserModelId
  !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $0
  ${If} $0 = 1
    !insertmacro UnpinShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  ${EndIf}
  !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $0
  ${If} $0 = 1
    !insertmacro UnpinShortcut "$DESKTOP\${PRODUCTNAME}.lnk"
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}
  DeleteRegValue HKCU "${RUNKEY}" "${PRODUCTNAME}"

  ; Legacy compatibility link: only the symbolic link this installer recorded
  ; is removed; a regular file the user placed there is never deleted.
  ReadRegStr $SpeechekValue HKCU "${MANUPRODUCTKEY}" "LegacyLink"
  ${If} $SpeechekValue == "symlink"
    System::Call 'kernel32::GetFileAttributesW(ws) i .r0' "${SPEECHEK_LEGACY_LINK}"
    StrCpy $SpeechekLegacyAttr $0
    ${If} $SpeechekLegacyAttr <> -1
      IntOp $SpeechekLegacyAttr $SpeechekLegacyAttr & ${SPEECHEK_FILE_ATTRIBUTE_REPARSE_POINT}
      ${If} $SpeechekLegacyAttr <> 0
        ClearErrors
        Delete "${SPEECHEK_LEGACY_LINK}"
        ${If} ${Errors}
          StrCpy $SpeechekUninstallFailed 1
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}

  ; Uninstall registry entry: removed only when every managed file was deleted
  ; and the record still describes this installation; otherwise it is kept so
  ; the installation can be repaired or removed again.
  ReadRegStr $SpeechekValue HKCU "${UNINSTKEY}" "InstallLocation"
  ${If} $SpeechekUninstallFailed = 1
    MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekUninstallIncomplete)"
  ${ElseIf} $SpeechekValue == "$\"$INSTDIR$\""
    DeleteRegKey HKCU "${UNINSTKEY}"
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
    DeleteRegKey HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"
  ${Else}
    MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekUninstallEntryKept)"
  ${EndIf}

  ; Opt-in data deletion. Missing paths are an idempotent success; reparse
  ; points and unexpected paths are refused without following them.
  ${If} $SpeechekDeleteDataState = 1
    StrCpy $SpeechekDataFailed 0
    StrCpy $SpeechekDataUnsafe 0
    StrCpy $SpeechekDataFailedPath ""
    Push "${SPEECHEK_PROFILE_DIR}"
    Call un.SpeechekDeleteDataDir
    Push "${SPEECHEK_WEBVIEW_ROAMING}"
    Call un.SpeechekDeleteDataDir
    Push "${SPEECHEK_WEBVIEW_LOCAL}"
    Call un.SpeechekDeleteDataDir
    ${If} $SpeechekDataUnsafe = 1
      StrCpy $R0 "$SpeechekDataFailedPath"
      MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekDataDeleteUnsafe)"
    ${EndIf}
    ${If} $SpeechekDataFailed = 1
      StrCpy $R0 "$SpeechekDataFailedPath"
      MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekDataDeleteFailed)"
    ${EndIf}
  ${EndIf}

  !ifmacrodef NSIS_HOOK_POSTUNINSTALL
    !insertmacro NSIS_HOOK_POSTUNINSTALL
  !endif
SectionEnd

; Recursively walks one directory tree for the uninstaller.
;
; $SpeechekTreeMode = 0 verifies the tree only; $SpeechekTreeMode = 1 also
; deletes files and empty directories. Every directory entry is checked for
; FILE_ATTRIBUTE_REPARSE_POINT before it is touched: a nested junction or
; symlink is never followed and stops the walk. NSIS's own RMDir /r recurses
; through reparse points, so it is deliberately not used here.
;
; Stack input: <absolute directory path>.
; Output: $SpeechekTreeStatus (0 clean, 1 reparse point found, 2 delete failure).
; Local registers $R4-$R9 are saved and restored, which keeps the recursion
; safe.
Function un.SpeechekTreeWalk
  Exch $R9
  Push $R8
  Push $R7
  Push $R6
  Push $R5
  Push $R4

  StrCpy $R4 0
  ClearErrors
  FindFirst $R8 $R7 "$R9\*.*"
  ${If} $R7 == ""
    Goto schk_walk_finish
  ${EndIf}

  schk_walk_loop:
    ${If} $R7 == "."
      Goto schk_walk_next
    ${EndIf}
    ${If} $R7 == ".."
      Goto schk_walk_next
    ${EndIf}
    StrCpy $R6 "$R9\$R7"
    System::Call 'kernel32::GetFileAttributesW(ws) i .r0' "$R6"
    ${If} $0 = -1 ; vanished between enumeration and inspection
      Goto schk_walk_next
    ${EndIf}
    StrCpy $R5 $0
    IntOp $R5 $R5 & ${SPEECHEK_FILE_ATTRIBUTE_REPARSE_POINT}
    ${If} $R5 <> 0
      StrCpy $R4 1 ; refuse: do not follow the reparse point
      Goto schk_walk_abort
    ${EndIf}
    ; Recurse into subdirectories in both modes (verification and deletion).
    IntOp $R5 $0 & ${SPEECHEK_FILE_ATTRIBUTE_DIRECTORY}
    ${If} $R5 <> 0
      Push $R9
      Push $R8
      Push $R7
      Push "$R6"
      Call un.SpeechekTreeWalk
      StrCpy $R4 $SpeechekTreeStatus
      Pop $R7
      Pop $R8
      Pop $R9
      ${If} $R4 <> 0
        Goto schk_walk_abort
      ${EndIf}
    ${ElseIf} $SpeechekTreeMode = 1
      ClearErrors
      Delete "$R6"
      ${If} ${Errors}
        StrCpy $R4 2
        Goto schk_walk_abort
      ${EndIf}
    ${EndIf}
  schk_walk_next:
    FindNext $R8 $R7
    ${If} $R7 == ""
      Goto schk_walk_finish
    ${EndIf}
    Goto schk_walk_loop

  schk_walk_finish:
    ${If} $R8 != ""
      FindClose $R8
      StrCpy $R8 ""
    ${EndIf}
    ${If} $SpeechekTreeMode = 1
      ClearErrors
      RMDir "$R9" ; non-recursive: only removes the directory when empty
      ${If} ${Errors}
        StrCpy $R4 2
      ${EndIf}
    ${EndIf}
    Goto schk_walk_end

  schk_walk_abort:
    ${If} $R8 != ""
      FindClose $R8
      StrCpy $R8 ""
    ${EndIf}

  schk_walk_end:
    StrCpy $SpeechekTreeStatus $R4
    Pop $R4
    Pop $R5
    Pop $R6
    Pop $R7
    Pop $R8
    Pop $R9
FunctionEnd

; Deletes one known Speechek data directory.
; Stack input: <absolute directory path>; must be one of the three constants
; defined above. Sets $SpeechekDataFailed / $SpeechekDataUnsafe and records the
; affected path in $SpeechekDataFailedPath. The tree is pre-scanned and then
; deleted with per-level checks; a nested reparse point makes the whole tree
; unsafe and nothing of it is deleted.
Function un.SpeechekDeleteDataDir
  Pop $SpeechekDataDir
  StrCpy $SpeechekDataAllowed 0
  ${If} $SpeechekDataDir == "${SPEECHEK_PROFILE_DIR}"
    StrCpy $SpeechekDataAllowed 1
  ${ElseIf} $SpeechekDataDir == "${SPEECHEK_WEBVIEW_ROAMING}"
    StrCpy $SpeechekDataAllowed 1
  ${ElseIf} $SpeechekDataDir == "${SPEECHEK_WEBVIEW_LOCAL}"
    StrCpy $SpeechekDataAllowed 1
  ${EndIf}
  ${If} $SpeechekDataAllowed = 0
    IfFileExists "$SpeechekDataDir" 0 schk_data_unknown_done
      StrCpy $SpeechekDataUnsafe 1
      StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
    schk_data_unknown_done:
    Return
  ${EndIf}

  System::Call 'kernel32::GetFileAttributesW(ws) i .r0' "$SpeechekDataDir"
  ${If} $0 = -1 ; missing: nothing to delete
    Return
  ${EndIf}
  IntOp $1 $0 & ${SPEECHEK_FILE_ATTRIBUTE_REPARSE_POINT}
  ${If} $1 <> 0
    StrCpy $SpeechekDataUnsafe 1
    StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
    Return
  ${EndIf}

  ; Pre-scan: refuse the whole tree if any level is a reparse point.
  StrCpy $SpeechekTreeMode 0
  Push "$SpeechekDataDir"
  Call un.SpeechekTreeWalk
  ${If} $SpeechekTreeStatus = 1
    StrCpy $SpeechekDataUnsafe 1
    StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
    Return
  ${EndIf}
  ${If} $SpeechekTreeStatus <> 0
    StrCpy $SpeechekDataFailed 1
    StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
    Return
  ${EndIf}

  ; Delete the tree, re-checking every level; never RMDir /r.
  StrCpy $SpeechekTreeMode 1
  Push "$SpeechekDataDir"
  Call un.SpeechekTreeWalk
  ${If} $SpeechekTreeStatus = 1
    StrCpy $SpeechekDataUnsafe 1
    StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
    Return
  ${EndIf}
  ${If} $SpeechekTreeStatus <> 0
    StrCpy $SpeechekDataFailed 1
    StrCpy $SpeechekDataFailedPath "$SpeechekDataDir"
  ${EndIf}
FunctionEnd
