; ═══════════════════════════════════════════════════════════════════════════
; Speechek — shared NSIS installer/uninstaller helper macros and functions
;
; Part of a deliberate fork of the Tauri NSIS template for Speechek. The
; template fork lives in src-tauri/nsis/installer.nsi and is based on the
; upstream template shipped with tauri-cli v2.12.0
; (crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi),
; licensed Apache-2.0/MIT, © 2019-2024 Tauri Programme within The Commons
; Conservancy.
;
; At bundle time the build helper copies this file and the generated
; payload.nsh into <target>/<profile>/ (outside nsis/<arch>, which
; tauri-bundler wipes and re-renders on every run). The rendered
; installer.nsi includes them through ${__FILEDIR__}/../../common.nsh and
; ${__FILEDIR__}/../../payload.nsh.
;
; Conventions:
;   * Driver functions are instantiated twice, once for the installer and once
;     for the uninstaller:
;       !insertmacro SpeechekFunctions ""  ""
;       !insertmacro SpeechekFunctions un. un
;     The prefix selects the function names and the tag keeps every generated
;     label unique; the two copies live in separate label spaces anyway.
;   * All state lives in the Speechek* variables below; $0-$9 are used only as
;     immediate destinations of System::Call return values and are saved and
;     restored by every function. $R0-$R9 are never referenced from a call
;     descriptor, so "r0"-style tokens always mean $0..$9.
;   * Function inputs are read with Pop, outputs are Push-ed at the end.
;   * No secret material is involved: the SHA-256 hash object is created with
;     pbHashObject=NULL/cbHashObject=0 and pbSecret=NULL.
; ═══════════════════════════════════════════════════════════════════════════

!ifndef SPEECHEK_COMMON_NSH
!define SPEECHEK_COMMON_NSH 1

!include LogicLib.nsh
!include Win\RestartManager.nsh

; ── Constants ──────────────────────────────────────────────────────────────
; PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE
!define SPEECHEK_PROCESS_ACCESS 0x00101001

!define SPEECHEK_EVENT_MODIFY_STATE 0x0002
!define SPEECHEK_TOKEN_QUERY        0x0008
!define SPEECHEK_TOKEN_USER         1

!define SPEECHEK_ERROR_MORE_DATA  234
!define SPEECHEK_WAIT_TIMEOUT      258

!define SPEECHEK_RM_PROCESS_INFO_SIZE 668
!define SPEECHEK_RM_STARTTIME_OFFSET   4
!define SPEECHEK_HANDLE_RECORD_SIZE    8
!define SPEECHEK_MAX_HANDLES  64
!define SPEECHEK_HANDLE_BYTES 512 ; SPEECHEK_MAX_HANDLES * SPEECHEK_HANDLE_RECORD_SIZE

!define SPEECHEK_GRACE_PERIOD_MS 5000
!define SPEECHEK_KILL_PERIOD_MS  1000
!define SPEECHEK_POLL_MS         100
!define SPEECHEK_GRACE_POLLS     60   ; safety cap: 60 * 100 ms >= grace period
!define SPEECHEK_KILL_POLLS      15   ; safety cap: 15 * 100 ms >= kill period

!define SPEECHEK_SHA_READ_CHUNK 65536

; ── Global runtime state ───────────────────────────────────────────────────
; Shared names; the installer and the uninstaller each have their own copy of
; the values at run time.
Var SpeechekInstallerMutex

Var SpeechekShaPath
Var SpeechekShaDigest
Var SpeechekShaAlg
Var SpeechekShaHash
Var SpeechekShaFile
Var SpeechekShaBuffer
Var SpeechekShaOut
Var SpeechekShaIndex
Var SpeechekShaTemp

Var SpeechekVerValue
Var SpeechekVerResult
Var SpeechekVerDigits
Var SpeechekVerIndex
Var SpeechekVerChar

Var SpeechekProcPid
Var SpeechekProcEntry
Var SpeechekProcExpected
Var SpeechekProcHandle
Var SpeechekProcStatus
Var SpeechekProcFatal
Var SpeechekProcImage
Var SpeechekProcImageSize
Var SpeechekProcTimes
Var SpeechekProcExit
Var SpeechekProcKernel
Var SpeechekProcUser
Var SpeechekProcRmStart
Var SpeechekProcSessPtr
Var SpeechekProcSessA
Var SpeechekProcSessB
Var SpeechekProcTokenA
Var SpeechekProcTokenB
Var SpeechekProcBufA
Var SpeechekProcBufB
Var SpeechekProcRetLen
Var SpeechekProcSidA
Var SpeechekProcSidB
Var SpeechekProcTmp

Var SpeechekClosePath
Var SpeechekCloseSession
Var SpeechekCloseNeeded
Var SpeechekCloseCount
Var SpeechekCloseNeededPtr
Var SpeechekCloseCountPtr
Var SpeechekCloseRebootPtr
Var SpeechekCloseAttempts
Var SpeechekCloseList
Var SpeechekCloseVerified
Var SpeechekCloseHandles
Var SpeechekCloseIndex
Var SpeechekCloseSlot
Var SpeechekCloseEntry
Var SpeechekClosePid
Var SpeechekCloseReason
Var SpeechekCloseDeadline
Var SpeechekClosePolls
Var SpeechekCloseLive
Var SpeechekCloseFailed

; ── Small shared macros ────────────────────────────────────────────────────

; Acquires the per-identity installer/uninstaller mutex with the logical name
; Local\${BUNDLEID}.installer. ${outVar} is set to 1 when this process owns the
; mutex, 0 when another installer/uninstaller of the same identity already runs
; in this session (or when the mutex cannot be created at all).
!macro SpeechekAcquireInstallerMutex outVar
  System::Call 'kernel32::CreateMutexW(p 0, i 0, ws) p .r0' "Local\${BUNDLEID}.installer"
  StrCpy $SpeechekInstallerMutex $0
  ${If} $SpeechekInstallerMutex = 0
    StrCpy ${outVar} 0
  ${Else}
    System::Call 'kernel32::GetLastError() i .r0'
    ${If} $0 = 183 ; ERROR_ALREADY_EXISTS
      StrCpy ${outVar} 0
    ${Else}
      StrCpy ${outVar} 1
    ${EndIf}
  ${EndIf}
!macroend

!macro SpeechekReleaseInstallerMutex
  ${If} $SpeechekInstallerMutex <> 0
    System::Call 'kernel32::CloseHandle(p $SpeechekInstallerMutex)'
    StrCpy $SpeechekInstallerMutex 0
  ${EndIf}
!macroend

; Frees the three 4-byte output cells used by an RmGetList attempt. Called on
; every exit path of the list query so no plug-in allocation is leaked.
!macro SpeechekFreeGetListPtrs
  ${If} $SpeechekCloseNeededPtr <> 0
    System::Free $SpeechekCloseNeededPtr
    StrCpy $SpeechekCloseNeededPtr 0
  ${EndIf}
  ${If} $SpeechekCloseCountPtr <> 0
    System::Free $SpeechekCloseCountPtr
    StrCpy $SpeechekCloseCountPtr 0
  ${EndIf}
  ${If} $SpeechekCloseRebootPtr <> 0
    System::Free $SpeechekCloseRebootPtr
    StrCpy $SpeechekCloseRebootPtr 0
  ${EndIf}
!macroend

; Writes one diagnostic line to the parent console when a console is attached.
; Used for rejected unattended installs, where no user interface may appear.
; ${textVar} is the name of a variable holding the message; the message is
; passed as a trailing parameter and consumed by "ws", never inlined into the
; descriptor (spaces and special characters stay intact that way).
!macro SpeechekWriteParentConsole textVar
  System::Call 'kernel32::GetConsoleWindow() p .r0'
  ${If} $0 = 0
    System::Call 'kernel32::AttachConsole(i -1)' ; ATTACH_PARENT_PROCESS
  ${EndIf}
  System::Call 'kernel32::GetStdHandle(i -12) p .r0' ; STDERR
  ${If} $0 <> 0
    StrLen $1 ${textVar}
    IntOp $1 $1 * 2 ; byte length of the UTF-16 message
    System::Call 'kernel32::WriteFile(p r0, ws, i r1, p 0, *i .r2)' "${textVar}"
  ${EndIf}
!macroend

; Rejects /S (silent) and /P (passive) runs before any side effect: only the
; interactive installer can obtain the consent that is required to close a
; running Speechek instance and to replace installed files. Exit code 2 plus a
; console diagnostic is the contract for these modes.
!macro SpeechekRejectUnattended
  ${If} ${Silent}
    StrCpy $R1 "Speechek: silent (/S) installation is not supported; closing a running Speechek requires explicit user consent. Run the interactive installer."
    !insertmacro SpeechekWriteParentConsole $R1
    SetErrorLevel 2
    Abort
  ${EndIf}
  ClearErrors
  ${GetOptions} $CMDLINE "/P" $R0
  ${IfNot} ${Errors}
    StrCpy $R1 "Speechek: passive (/P) installation is not supported; the required consent cannot be given unattended. Run the interactive installer."
    !insertmacro SpeechekWriteParentConsole $R1
    SetErrorLevel 2
    Abort
  ${EndIf}
!macroend

; Visible fatal error with a non-zero exit code.
!macro SpeechekFatalMessage string
  MessageBox MB_OK|MB_ICONSTOP "${string}"
  SetErrorLevel 2
  Abort
!macroend

; Closes every process handle stored in the verified-handle array. Used on the
; success path and on the abort path of the close flow. ${Site} must be a
; non-empty, per-insertion-site token: makensis treats an empty macro argument
; as "no argument at all", and both call sites live in the same function, so
; the token has to differ between them as well.
!macro SpeechekReleaseVerifiedHandles Site
  StrCpy $SpeechekCloseIndex 0
  speck_${Site}_rel_loop:
    ${If} $SpeechekCloseIndex >= $SpeechekCloseVerified
      Goto speck_${Site}_rel_done
    ${EndIf}
    IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_HANDLE_RECORD_SIZE}
    IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
    System::Call '*$SpeechekCloseEntry(i .r0, i .r1)'
    ${If} $1 <> 0
      System::Call 'kernel32::CloseHandle(p r1)'
    ${EndIf}
    IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
    Goto speck_${Site}_rel_loop
  speck_${Site}_rel_done:
!macroend

; ── SpeechekFunctions ──────────────────────────────────────────────────────
; ${Prefix} names the installer ("") or uninstaller ("un.") copy; ${Tag} keeps
; every generated label unique across both copies.
!macro SpeechekFunctions Prefix Tag

; SHA-256 of a file through the Windows CNG (bcrypt.dll) API, streaming the
; content through one reusable 64 KiB buffer.
;
; Stack input:  <absolute path of the file>
; Stack output: <lowercase 64-character hex digest>; empty plus the error flag
;               when the file is missing, cannot be read, hashed or released.
Function ${Prefix}SpeechekSha256File
  Pop $SpeechekShaPath
  Push $0
  Push $1
  Push $2
  Push $3

  StrCpy $SpeechekShaDigest ""
  StrCpy $SpeechekShaAlg 0
  StrCpy $SpeechekShaHash 0
  StrCpy $SpeechekShaFile 0
  StrCpy $SpeechekShaBuffer 0
  StrCpy $SpeechekShaOut 0
  StrCpy $SpeechekShaIndex 0

  IfFileExists "$SpeechekShaPath" 0 speck_${Tag}_sha_release

  System::Call 'bcrypt::BCryptOpenAlgorithmProvider(*p .r0, w "SHA256", p 0, i 0) i .r1'
  StrCpy $SpeechekShaAlg $0
  ${If} $1 <> 0
    StrCpy $SpeechekShaAlg 0
    Goto speck_${Tag}_sha_release
  ${EndIf}

  System::Call 'bcrypt::BCryptCreateHash(p $SpeechekShaAlg, *p .r0, p 0, i 0, p 0, i 0, i 0) i .r1'
  StrCpy $SpeechekShaHash $0
  ${If} $1 <> 0
    StrCpy $SpeechekShaHash 0
    Goto speck_${Tag}_sha_release
  ${EndIf}

  System::Call 'kernel32::CreateFileW(ws, i 0x80000000, i 0x1, p 0, i 3, i 0x80, p 0) p .r0' "$SpeechekShaPath"
  StrCpy $SpeechekShaFile $0
  ${If} $SpeechekShaFile = -1 ; INVALID_HANDLE_VALUE
    StrCpy $SpeechekShaFile 0
    Goto speck_${Tag}_sha_release
  ${EndIf}
  ${If} $SpeechekShaFile = 0
    Goto speck_${Tag}_sha_release
  ${EndIf}

  System::Alloc ${SPEECHEK_SHA_READ_CHUNK}
  Pop $SpeechekShaBuffer
  System::Alloc 32
  Pop $SpeechekShaOut
  ${If} $SpeechekShaBuffer = 0
    Goto speck_${Tag}_sha_release
  ${EndIf}
  ${If} $SpeechekShaOut = 0
    Goto speck_${Tag}_sha_release
  ${EndIf}

  speck_${Tag}_sha_read:
    ; lpNumberOfBytesRead points into a plug-in temporary and lpOverlapped
    ; stays NULL, which is valid only for a synchronous handle.
    System::Call 'kernel32::ReadFile(p $SpeechekShaFile, p $SpeechekShaBuffer, i ${SPEECHEK_SHA_READ_CHUNK}, *i .r2, p 0) i .r0'
    ${If} $0 = 0
      Goto speck_${Tag}_sha_release ; read failure
    ${EndIf}
    ${If} $2 = 0
      Goto speck_${Tag}_sha_finish    ; end of file
    ${EndIf}
    System::Call 'bcrypt::BCryptHashData(p $SpeechekShaHash, p $SpeechekShaBuffer, i r2, i 0) i .r0'
    ${If} $0 <> 0
      Goto speck_${Tag}_sha_release
    ${EndIf}
    Goto speck_${Tag}_sha_read

  speck_${Tag}_sha_finish:
    System::Call 'bcrypt::BCryptFinishHash(p $SpeechekShaHash, p $SpeechekShaOut, i 32, i 0) i .r0'
    ${If} $0 <> 0
      Goto speck_${Tag}_sha_release
    ${EndIf}
    StrCpy $SpeechekShaIndex 0
    speck_${Tag}_sha_hex:
      ${If} $SpeechekShaIndex >= 32
        Goto speck_${Tag}_sha_release
      ${EndIf}
      IntOp $SpeechekShaTemp $SpeechekShaOut + $SpeechekShaIndex
      System::Call '*$SpeechekShaTemp(&i1 .r0)'
      IntOp $0 $0 & 0xFF
      IntFmt $1 "%02x" $0
      StrCpy $SpeechekShaDigest "$SpeechekShaDigest$1"
      IntOp $SpeechekShaIndex $SpeechekShaIndex + 1
      Goto speck_${Tag}_sha_hex

  speck_${Tag}_sha_release:
    ; DestroyHash before the buffers are released, then the provider and the
    ; file handle; every exit path releases everything it acquired.
    ${If} $SpeechekShaHash <> 0
      System::Call 'bcrypt::BCryptDestroyHash(p $SpeechekShaHash)'
    ${EndIf}
    ${If} $SpeechekShaAlg <> 0
      System::Call 'bcrypt::BCryptCloseAlgorithmProvider(p $SpeechekShaAlg, i 0)'
    ${EndIf}
    ${If} $SpeechekShaFile <> 0
      System::Call 'kernel32::CloseHandle(p $SpeechekShaFile)'
    ${EndIf}
    ${If} $SpeechekShaBuffer <> 0
      System::Free $SpeechekShaBuffer
    ${EndIf}
    ${If} $SpeechekShaOut <> 0
      System::Free $SpeechekShaOut
    ${EndIf}

    Pop $3
    Pop $2
    Pop $1
    Pop $0
    ${If} $SpeechekShaDigest == ""
      SetErrors
    ${Else}
      ClearErrors
    ${EndIf}
    Push $SpeechekShaDigest
FunctionEnd

; Validates a version string: dot-separated decimal components, no empty and no
; non-numeric parts ("0.2.0", "1.2.3.4").
;
; Stack input:  <version string>
; Stack output: <1 valid | 0 invalid>
Function ${Prefix}SpeechekVersionValid
  Pop $SpeechekVerValue
  Push $0

  StrCpy $SpeechekVerResult 0 ; result
  StrCpy $SpeechekVerDigits 0 ; digits in the current component
  StrCpy $SpeechekVerIndex 0  ; index

  ${If} $SpeechekVerValue == ""
    Goto speck_${Tag}_ver_done
  ${EndIf}

  speck_${Tag}_ver_loop:
    StrCpy $SpeechekVerChar $SpeechekVerValue 1 $SpeechekVerIndex
    ${If} $SpeechekVerChar == ""
      ${If} $SpeechekVerDigits > 0
        StrCpy $SpeechekVerResult 1
      ${EndIf}
      Goto speck_${Tag}_ver_done
    ${EndIf}
    ${If} $SpeechekVerChar == "."
      ${If} $SpeechekVerDigits = 0
        Goto speck_${Tag}_ver_done
      ${EndIf}
      StrCpy $SpeechekVerDigits 0
    ${ElseIf} $SpeechekVerChar == "0"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "1"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "2"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "3"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "4"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "5"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "6"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "7"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "8"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${ElseIf} $SpeechekVerChar == "9"
      IntOp $SpeechekVerDigits $SpeechekVerDigits + 1
    ${Else}
      Goto speck_${Tag}_ver_done
    ${EndIf}
    IntOp $SpeechekVerIndex $SpeechekVerIndex + 1
    Goto speck_${Tag}_ver_loop

  speck_${Tag}_ver_done:
    Pop $0
    Push $SpeechekVerResult
FunctionEnd

; Opens and verifies one Restart Manager owner. A process is accepted only when
; all of the following hold:
;   * it can be opened with PROCESS_QUERY_LIMITED_INFORMATION|SYNCHRONIZE|
;     PROCESS_TERMINATE,
;   * its full image path equals the expected executable path,
;   * its creation FILETIME equals RM_UNIQUE_PROCESS.ProcessStartTime for that
;     PID (guards against PID reuse),
;   * it runs in the current session,
;   * its user token SID equals the SID of this installer process.
; A PID that has already exited (ERROR_INVALID_PARAMETER / ERROR_NOT_FOUND) is
; reported as "not verified" and is not fatal. Anything else that is alive but
; cannot be verified is fatal: foreign lockers, Explorer, security software and
; reused PIDs are never signalled and never terminated.
;
; Stack input: <pid> <pointer to the 668-byte RM_PROCESS_INFO entry> <expected EXE path>
; Outputs: $SpeechekProcHandle (verified handle or 0),
;          $SpeechekProcStatus (1 verified | 0 not verified),
;          $SpeechekProcFatal  (1 when the installation must stop)
Function ${Prefix}SpeechekOpenVerifiedProcess
  Pop $SpeechekProcExpected
  Pop $SpeechekProcEntry
  Pop $SpeechekProcPid
  Push $0
  Push $1
  Push $2

  StrCpy $SpeechekProcHandle 0
  StrCpy $SpeechekProcStatus 0
  StrCpy $SpeechekProcFatal 0
  StrCpy $SpeechekProcImage 0
  StrCpy $SpeechekProcImageSize 0
  StrCpy $SpeechekProcTimes 0
  StrCpy $SpeechekProcTokenA 0
  StrCpy $SpeechekProcTokenB 0
  StrCpy $SpeechekProcBufA 0
  StrCpy $SpeechekProcBufB 0
  StrCpy $SpeechekProcRetLen 0
  StrCpy $SpeechekProcSessPtr 0

  System::Call 'kernel32::OpenProcess(i ${SPEECHEK_PROCESS_ACCESS}, i 0, i $SpeechekProcPid) p .r0'
  StrCpy $SpeechekProcHandle $0
  ${If} $SpeechekProcHandle = 0
    System::Call 'kernel32::GetLastError() i .r0'
    ${If} $0 = 87
    ${OrIf} $0 = 1168
      Goto speck_${Tag}_proc_done ; the process is gone
    ${EndIf}
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_done
  ${EndIf}

  ; Full image path of the owning process.
  System::StrAlloc 32768
  Pop $SpeechekProcImage
  System::Alloc 4
  Pop $SpeechekProcImageSize
  ${If} $SpeechekProcImage = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  ${If} $SpeechekProcImageSize = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call '*$SpeechekProcImageSize(i 32768)'
  System::Call 'kernel32::QueryFullProcessImageNameW(p $SpeechekProcHandle, i 0, p $SpeechekProcImage, p $SpeechekProcImageSize) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  ; QueryFullProcessImageNameW wrote a UTF-16 string into the allocated buffer;
  ; it must be copied into an NSIS string before comparing (the buffer address
  ; is not the path). The array length may never exceed an NSIS string
  ; (NSIS_MAX_STRLEN): the System plug-in writes the whole array, so a larger
  ; count overruns the destination string buffer and crashes the installer
  ; (0xC0000005). StrCmp is case-insensitive, matching Windows path semantics.
  System::Call '*$SpeechekProcImage(&t${NSIS_MAX_STRLEN} .r0)'
  StrCpy $SpeechekProcTmp $0 4
  ${If} $SpeechekProcTmp == "\\?\"
    StrCpy $0 $0 "" 4
  ${EndIf}
  StrCmp "$0" "$SpeechekProcExpected" 0 speck_${Tag}_proc_foreign

  ; Creation FILETIME from GetProcessTimes must equal the FILETIME that Restart
  ; Manager captured for this PID.
  System::Alloc 32
  Pop $SpeechekProcTimes
  ${If} $SpeechekProcTimes = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  IntOp $SpeechekProcExit $SpeechekProcTimes + 8
  IntOp $SpeechekProcKernel $SpeechekProcTimes + 16
  IntOp $SpeechekProcUser $SpeechekProcTimes + 24
  System::Call 'kernel32::GetProcessTimes(p $SpeechekProcHandle, p $SpeechekProcTimes, p $SpeechekProcExit, p $SpeechekProcKernel, p $SpeechekProcUser) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  IntOp $SpeechekProcRmStart $SpeechekProcEntry + ${SPEECHEK_RM_STARTTIME_OFFSET}
  System::Call 'ntdll::RtlCompareMemory(p $SpeechekProcTimes, p $SpeechekProcRmStart, i 8) i .r0'
  ${If} $0 <> 8
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}

  ; Same session as this process?
  System::Alloc 4
  Pop $SpeechekProcSessPtr
  ${If} $SpeechekProcSessPtr = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call 'kernel32::ProcessIdToSessionId(i $SpeechekProcPid, p $SpeechekProcSessPtr) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call '*$SpeechekProcSessPtr(i .r0)'
  StrCpy $SpeechekProcSessA $0
  System::Call 'kernel32::GetCurrentProcessId() i .r0'
  System::Call 'kernel32::ProcessIdToSessionId(i r0, p $SpeechekProcSessPtr) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call '*$SpeechekProcSessPtr(i .r0)'
  StrCpy $SpeechekProcSessB $0
  ${If} $SpeechekProcSessA <> $SpeechekProcSessB
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}

  ; Same user SID as this installer process?
  System::Alloc 512
  Pop $SpeechekProcBufA
  System::Alloc 512
  Pop $SpeechekProcBufB
  System::Alloc 4
  Pop $SpeechekProcRetLen
  ${If} $SpeechekProcBufA = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  ${If} $SpeechekProcBufB = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  ${If} $SpeechekProcRetLen = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}

  System::Call 'advapi32::OpenProcessToken(p $SpeechekProcHandle, i ${SPEECHEK_TOKEN_QUERY}, *p .r0) i .r1'
  StrCpy $SpeechekProcTokenA $0
  ${If} $1 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call 'kernel32::GetCurrentProcess() p .r2'
  System::Call 'advapi32::OpenProcessToken(p r2, i ${SPEECHEK_TOKEN_QUERY}, *p .r0) i .r1'
  StrCpy $SpeechekProcTokenB $0
  ${If} $1 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}

  System::Call 'advapi32::GetTokenInformation(p $SpeechekProcTokenA, i ${SPEECHEK_TOKEN_USER}, p $SpeechekProcBufA, i 512, p $SpeechekProcRetLen) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call 'advapi32::GetTokenInformation(p $SpeechekProcTokenB, i ${SPEECHEK_TOKEN_USER}, p $SpeechekProcBufB, i 512, p $SpeechekProcRetLen) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}
  System::Call '*$SpeechekProcBufA(p .r0)'
  StrCpy $SpeechekProcSidA $0
  System::Call '*$SpeechekProcBufB(p .r0)'
  StrCpy $SpeechekProcSidB $0
  System::Call 'advapi32::EqualSid(p $SpeechekProcSidA, p $SpeechekProcSidB) i .r0'
  ${If} $0 = 0
    StrCpy $SpeechekProcFatal 1
    Goto speck_${Tag}_proc_release
  ${EndIf}

  StrCpy $SpeechekProcStatus 1
  Goto speck_${Tag}_proc_release

  speck_${Tag}_proc_foreign:
    StrCpy $SpeechekProcFatal 1

  speck_${Tag}_proc_release:
    ${If} $SpeechekProcTokenA <> 0
      System::Call 'kernel32::CloseHandle(p $SpeechekProcTokenA)'
    ${EndIf}
    ${If} $SpeechekProcTokenB <> 0
      System::Call 'kernel32::CloseHandle(p $SpeechekProcTokenB)'
    ${EndIf}
    ${If} $SpeechekProcBufA <> 0
      System::Free $SpeechekProcBufA
    ${EndIf}
    ${If} $SpeechekProcBufB <> 0
      System::Free $SpeechekProcBufB
    ${EndIf}
    ${If} $SpeechekProcRetLen <> 0
      System::Free $SpeechekProcRetLen
    ${EndIf}
    ${If} $SpeechekProcImage <> 0
      System::Free $SpeechekProcImage ; StrAlloc buffer, released with Free
    ${EndIf}
    ${If} $SpeechekProcTimes <> 0
      System::Free $SpeechekProcTimes
    ${EndIf}
    ${If} $SpeechekProcImageSize <> 0
      System::Free $SpeechekProcImageSize
    ${EndIf}
    ${If} $SpeechekProcSessPtr <> 0
      System::Free $SpeechekProcSessPtr
    ${EndIf}
    ${If} $SpeechekProcStatus = 0
      ${If} $SpeechekProcHandle <> 0
        System::Call 'kernel32::CloseHandle(p $SpeechekProcHandle)'
        StrCpy $SpeechekProcHandle 0
      ${EndIf}
    ${EndIf}
    Pop $2
    Pop $1
    Pop $0

  speck_${Tag}_proc_done:
FunctionEnd

; Closes the installed Speechek instance in front of a replacement or an
; uninstallation.
;
; Stack input: <canonical absolute path of the installed EXE>
; Normal return: no process holds the file, or every verified owner exited
;                after the user confirmed the grace period.
; Otherwise: SetErrorLevel 2 followed by Abort; unverified processes were not
;                signalled and no file was changed.
Function ${Prefix}SpeechekCloseInstalled
  Pop $SpeechekClosePath
  Push $0
  Push $1

  StrCpy $SpeechekCloseSession ""
  StrCpy $SpeechekCloseNeeded 0
  StrCpy $SpeechekCloseCount 0
  StrCpy $SpeechekCloseAttempts 0
  StrCpy $SpeechekCloseList 0
  StrCpy $SpeechekCloseVerified 0
  StrCpy $SpeechekCloseHandles 0
  StrCpy $SpeechekCloseIndex 0
  StrCpy $SpeechekCloseSlot 0
  StrCpy $SpeechekCloseEntry 0
  StrCpy $SpeechekClosePid 0
  StrCpy $SpeechekCloseReason 0
  StrCpy $SpeechekCloseDeadline 0
  StrCpy $SpeechekClosePolls 0

  !insertmacro RestartManager_StartSession $SpeechekCloseSession
  ${If} $SpeechekCloseSession == ""
    Goto speck_${Tag}_close_in_use
  ${EndIf}
  !insertmacro RestartManager_RegisterFile $SpeechekCloseSession "$SpeechekClosePath"
  ${If} $0 <> 0
    Goto speck_${Tag}_close_in_use
  ${EndIf}

  speck_${Tag}_close_attempt:
    IntOp $SpeechekCloseAttempts $SpeechekCloseAttempts + 1
    ${If} $SpeechekCloseAttempts > 3
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ; RmGetList requires non-NULL pointers for pnProcInfoNeeded, pnProcInfo
    ; and lpdwRebootReasons (a NULL argument is reported as
    ; ERROR_BAD_ARGUMENTS). The first pass only asks how many entries are
    ; needed. Every exit path frees the three output cells.
    System::Alloc 4
    Pop $SpeechekCloseNeededPtr
    System::Alloc 4
    Pop $SpeechekCloseCountPtr
    System::Alloc 4
    Pop $SpeechekCloseRebootPtr
    ${If} $SpeechekCloseNeededPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ${If} $SpeechekCloseCountPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ${If} $SpeechekCloseRebootPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    System::Call '*$SpeechekCloseNeededPtr(i 0)'
    System::Call '*$SpeechekCloseCountPtr(i 0)'
    System::Call '*$SpeechekCloseRebootPtr(i 0)'
    System::Call 'RSTRTMGR::RmGetList(p $SpeechekCloseSession, p $SpeechekCloseNeededPtr, p $SpeechekCloseCountPtr, p 0, p $SpeechekCloseRebootPtr) i .r0'
    ; ERROR_SUCCESS means nobody holds the file anymore.
    ${If} $0 = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_done
    ${EndIf}
    ${If} $0 <> ${SPEECHEK_ERROR_MORE_DATA}
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    System::Call '*$SpeechekCloseNeededPtr(i .r0)'
    StrCpy $SpeechekCloseNeeded $0
    !insertmacro SpeechekFreeGetListPtrs
    ${If} $SpeechekCloseNeeded = 0
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ${If} $SpeechekCloseNeeded > ${SPEECHEK_MAX_HANDLES}
      Goto speck_${Tag}_close_in_use
    ${EndIf}

    ; Second pass: allocate one entry per reported process and read the list,
    ; again with valid pointers for every output parameter.
    IntOp $SpeechekCloseSlot $SpeechekCloseNeeded * ${SPEECHEK_RM_PROCESS_INFO_SIZE}
    System::Alloc $SpeechekCloseSlot
    Pop $SpeechekCloseList
    ${If} $SpeechekCloseList = 0
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    System::Alloc 4
    Pop $SpeechekCloseNeededPtr
    System::Alloc 4
    Pop $SpeechekCloseCountPtr
    System::Alloc 4
    Pop $SpeechekCloseRebootPtr
    ${If} $SpeechekCloseNeededPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ${If} $SpeechekCloseCountPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    ${If} $SpeechekCloseRebootPtr = 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    System::Call '*$SpeechekCloseNeededPtr(i $SpeechekCloseNeeded)'
    System::Call '*$SpeechekCloseCountPtr(i $SpeechekCloseNeeded)'
    System::Call '*$SpeechekCloseRebootPtr(i 0)'
    System::Call 'RSTRTMGR::RmGetList(p $SpeechekCloseSession, p $SpeechekCloseNeededPtr, p $SpeechekCloseCountPtr, p $SpeechekCloseList, p $SpeechekCloseRebootPtr) i .r0'
    ${If} $0 = ${SPEECHEK_ERROR_MORE_DATA}
      System::Call '*$SpeechekCloseCountPtr(i .r0)'
      StrCpy $SpeechekCloseNeeded $0
      !insertmacro SpeechekFreeGetListPtrs
      ${If} $SpeechekCloseList <> 0
        System::Free $SpeechekCloseList
        StrCpy $SpeechekCloseList 0
      ${EndIf}
      Goto speck_${Tag}_close_attempt ; the holder list changed: retry
    ${EndIf}
    ${If} $0 <> 0
      !insertmacro SpeechekFreeGetListPtrs
      Goto speck_${Tag}_close_in_use
    ${EndIf}
    System::Call '*$SpeechekCloseCountPtr(i .r0)'
    StrCpy $SpeechekCloseCount $0
    !insertmacro SpeechekFreeGetListPtrs
    ${If} $SpeechekCloseCount = 0
      Goto speck_${Tag}_close_done
    ${EndIf}
    ${If} $SpeechekCloseCount > ${SPEECHEK_MAX_HANDLES}
      Goto speck_${Tag}_close_in_use
    ${EndIf}

  ; ── Verify every reported owner before using it ──────────────────────────
  System::Alloc ${SPEECHEK_HANDLE_BYTES}
  Pop $SpeechekCloseHandles
  ${If} $SpeechekCloseHandles = 0
    Goto speck_${Tag}_close_in_use
  ${EndIf}
  StrCpy $SpeechekCloseVerified 0
  StrCpy $SpeechekCloseIndex 0

  speck_${Tag}_close_verify:
    ${If} $SpeechekCloseIndex >= $SpeechekCloseCount
      Goto speck_${Tag}_close_verified
    ${EndIf}
    IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_RM_PROCESS_INFO_SIZE}
    IntOp $SpeechekCloseEntry $SpeechekCloseList + $SpeechekCloseSlot
    System::Call '*$SpeechekCloseEntry(i .r0)'
    StrCpy $SpeechekClosePid $0

    Push $SpeechekClosePid
    Push $SpeechekCloseEntry
    Push "$SpeechekClosePath"
    Call ${Prefix}SpeechekOpenVerifiedProcess

    ${If} $SpeechekProcFatal = 1
      Goto speck_${Tag}_close_foreign
    ${EndIf}
    ${If} $SpeechekProcStatus = 1
      IntOp $SpeechekCloseSlot $SpeechekCloseVerified * ${SPEECHEK_HANDLE_RECORD_SIZE}
      IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
      System::Call '*$SpeechekCloseEntry(i $SpeechekClosePid, i $SpeechekProcHandle)'
      IntOp $SpeechekCloseVerified $SpeechekCloseVerified + 1
    ${EndIf}
    IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
    Goto speck_${Tag}_close_verify

  speck_${Tag}_close_verified:
    ${If} $SpeechekCloseVerified = 0
      Goto speck_${Tag}_close_done
    ${EndIf}

    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "$(speechekCloseWarning)" IDOK speck_${Tag}_close_consent IDCANCEL speck_${Tag}_close_cancel

  speck_${Tag}_close_consent:
    ; Signal the per-PID quit event of every verified target. An older build
    ; without the event is not an error: the full grace period is still given.
    StrCpy $SpeechekCloseIndex 0
    speck_${Tag}_close_signal:
      ${If} $SpeechekCloseIndex >= $SpeechekCloseVerified
        Goto speck_${Tag}_close_wait_start
      ${EndIf}
      IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_HANDLE_RECORD_SIZE}
      IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
      System::Call '*$SpeechekCloseEntry(i .r0, i .r1)'
      StrCpy $SpeechekClosePid $0
      System::Call 'kernel32::OpenEventW(i ${SPEECHEK_EVENT_MODIFY_STATE}, i 0, ws) p .r0' "Local\Speechek.Quit.$SpeechekClosePid"
      ${If} $0 <> 0
        System::Call 'kernel32::SetEvent(p r0)'
        System::Call 'kernel32::CloseHandle(p r0)'
      ${EndIf}
      IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
      Goto speck_${Tag}_close_signal

  speck_${Tag}_close_wait_start:
    ; One shared deadline for every owner, measured from the monotonic tick
    ; counter; the poll cap is a safety net so the loop can never spin forever
    ; if the clock appears stuck.
    System::Call 'kernel32::GetTickCount() i .r0'
    StrCpy $SpeechekCloseDeadline $0
    StrCpy $SpeechekClosePolls 0

  speck_${Tag}_close_wait:
    StrCpy $SpeechekCloseLive 0
    StrCpy $SpeechekCloseFailed 0
    StrCpy $SpeechekCloseIndex 0
    speck_${Tag}_close_wait_scan:
      ${If} $SpeechekCloseIndex >= $SpeechekCloseVerified
        Goto speck_${Tag}_close_wait_scanned
      ${EndIf}
      IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_HANDLE_RECORD_SIZE}
      IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
      System::Call '*$SpeechekCloseEntry(i .r0, i .r1)'
      StrCpy $0 $1 ; the stored process handle
      System::Call 'kernel32::WaitForSingleObject(p r0, i 0) i .r1'
      ${If} $1 = ${SPEECHEK_WAIT_TIMEOUT}
        IntOp $SpeechekCloseLive $SpeechekCloseLive + 1
      ${ElseIf} $1 <> 0
        StrCpy $SpeechekCloseFailed 1
      ${EndIf}
      IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
      Goto speck_${Tag}_close_wait_scan
    speck_${Tag}_close_wait_scanned:
      ${If} $SpeechekCloseFailed = 1
        Goto speck_${Tag}_close_unconfirmed
      ${EndIf}
      ${If} $SpeechekCloseLive = 0
        Goto speck_${Tag}_close_done
      ${EndIf}
      IntOp $SpeechekClosePolls $SpeechekClosePolls + 1
      ${If} $SpeechekClosePolls >= ${SPEECHEK_GRACE_POLLS}
        Goto speck_${Tag}_close_force
      ${EndIf}
      System::Call 'kernel32::GetTickCount() i .r0'
      IntOp $1 $0 - $SpeechekCloseDeadline ; wrap-safe elapsed milliseconds
      ${If} $1 >= ${SPEECHEK_GRACE_PERIOD_MS}
        Goto speck_${Tag}_close_force
      ${EndIf}
      Sleep ${SPEECHEK_POLL_MS}
      Goto speck_${Tag}_close_wait

  speck_${Tag}_close_force:
    ; The grace period expired: terminate only the handles verified above. The
    ; handles are held open, so the PIDs cannot have been reused meanwhile.
    StrCpy $SpeechekCloseIndex 0
    speck_${Tag}_close_kill:
      ${If} $SpeechekCloseIndex >= $SpeechekCloseVerified
        Goto speck_${Tag}_close_kill_wait_start
      ${EndIf}
      IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_HANDLE_RECORD_SIZE}
      IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
      System::Call '*$SpeechekCloseEntry(i .r0, i .r1)'
      StrCpy $0 $1
      System::Call 'kernel32::WaitForSingleObject(p r0, i 0) i .r1'
      ${If} $1 = ${SPEECHEK_WAIT_TIMEOUT}
        System::Call 'kernel32::TerminateProcess(p r0, i 1)'
      ${EndIf}
      IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
      Goto speck_${Tag}_close_kill

  speck_${Tag}_close_kill_wait_start:
    System::Call 'kernel32::GetTickCount() i .r0'
    StrCpy $SpeechekCloseDeadline $0
    StrCpy $SpeechekClosePolls 0
  speck_${Tag}_close_kill_wait:
    StrCpy $SpeechekCloseLive 0
    StrCpy $SpeechekCloseFailed 0
    StrCpy $SpeechekCloseIndex 0
    speck_${Tag}_close_kill_scan:
      ${If} $SpeechekCloseIndex >= $SpeechekCloseVerified
        Goto speck_${Tag}_close_kill_scanned
      ${EndIf}
      IntOp $SpeechekCloseSlot $SpeechekCloseIndex * ${SPEECHEK_HANDLE_RECORD_SIZE}
      IntOp $SpeechekCloseEntry $SpeechekCloseHandles + $SpeechekCloseSlot
      System::Call '*$SpeechekCloseEntry(i .r0, i .r1)'
      StrCpy $0 $1
      System::Call 'kernel32::WaitForSingleObject(p r0, i 0) i .r1'
      ${If} $1 = ${SPEECHEK_WAIT_TIMEOUT}
        IntOp $SpeechekCloseLive $SpeechekCloseLive + 1
      ${ElseIf} $1 <> 0
        StrCpy $SpeechekCloseFailed 1
      ${EndIf}
      IntOp $SpeechekCloseIndex $SpeechekCloseIndex + 1
      Goto speck_${Tag}_close_kill_scan
    speck_${Tag}_close_kill_scanned:
      ${If} $SpeechekCloseFailed = 1
        Goto speck_${Tag}_close_unconfirmed
      ${EndIf}
      ${If} $SpeechekCloseLive = 0
        Goto speck_${Tag}_close_done
      ${EndIf}
      IntOp $SpeechekClosePolls $SpeechekClosePolls + 1
      ${If} $SpeechekClosePolls >= ${SPEECHEK_KILL_POLLS}
        Goto speck_${Tag}_close_unconfirmed
      ${EndIf}
      System::Call 'kernel32::GetTickCount() i .r0'
      IntOp $1 $0 - $SpeechekCloseDeadline
      ${If} $1 >= ${SPEECHEK_KILL_PERIOD_MS}
        Goto speck_${Tag}_close_unconfirmed
      ${EndIf}
      Sleep ${SPEECHEK_POLL_MS}
      Goto speck_${Tag}_close_kill_wait

  speck_${Tag}_close_cancel:
    StrCpy $SpeechekCloseReason 0 ; user cancelled
    Goto speck_${Tag}_close_abort

  speck_${Tag}_close_unconfirmed:
    StrCpy $SpeechekCloseReason 1 ; exit could not be confirmed
    Goto speck_${Tag}_close_abort

  speck_${Tag}_close_foreign:
  speck_${Tag}_close_in_use:
    StrCpy $SpeechekCloseReason 2 ; held by an unverifiable process
    Goto speck_${Tag}_close_abort

  speck_${Tag}_close_done:
    !insertmacro RestartManager_EndSession $SpeechekCloseSession
    Goto speck_${Tag}_close_release

  speck_${Tag}_close_abort:
    ${If} $SpeechekCloseSession != ""
      !insertmacro RestartManager_EndSession $SpeechekCloseSession
    ${EndIf}
    ${If} $SpeechekCloseList <> 0
      System::Free $SpeechekCloseList
      StrCpy $SpeechekCloseList 0
    ${EndIf}
    ${If} $SpeechekCloseHandles <> 0
      !insertmacro SpeechekReleaseVerifiedHandles ${Prefix}abort
      System::Free $SpeechekCloseHandles
      StrCpy $SpeechekCloseHandles 0
    ${EndIf}
    ${If} $SpeechekCloseReason = 0
      MessageBox MB_OK|MB_ICONEXCLAMATION "$(speechekCloseCancelled)"
    ${ElseIf} $SpeechekCloseReason = 1
      MessageBox MB_OK|MB_ICONSTOP "$(speechekCannotConfirmExit)"
    ${Else}
      MessageBox MB_OK|MB_ICONSTOP "$(speechekFileInUse)"
    ${EndIf}
    SetErrorLevel 2
    Abort

  speck_${Tag}_close_release:
    ${If} $SpeechekCloseList <> 0
      System::Free $SpeechekCloseList
      StrCpy $SpeechekCloseList 0
    ${EndIf}
    ${If} $SpeechekCloseHandles <> 0
      !insertmacro SpeechekReleaseVerifiedHandles ${Prefix}release
      System::Free $SpeechekCloseHandles
      StrCpy $SpeechekCloseHandles 0
    ${EndIf}
    Pop $1
    Pop $0
FunctionEnd

!macroend ; SpeechekFunctions

!endif ; SPEECHEK_COMMON_NSH
