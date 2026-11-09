; NSIS installer for the unofficial Lan Mouse fork.
;
; Built by .github/workflows/release.yml on the Windows runner after
; `lan-mouse-windows\` was populated with lan-mouse.exe, lan-mouse-gpui.exe
; and the MinGW runtime DLLs they need. The version is passed with
; /DAPPVERSION=<version> (keep the fallback below in sync with Cargo.toml).
;
; ---------------------------------------------------------------------------
; Per-user data / clean install
; ---------------------------------------------------------------------------
; Lan Mouse stores everything it persists for one user in
;   %LOCALAPPDATA%\lan-mouse   config.toml (clients, devices, permissions,
;                              address rules, denied fingerprints), the
;                              certificate/private key and the audit log.
; Portable or older builds may also have left %APPDATA%\lan-mouse behind.
; Nothing outside those two directories is ever deleted by this installer.
;
; Install:
;   * interactive: if old data exists, ask before cleaning it (default: Yes,
;     so a fresh install never resurrects old PCs). The data is MOVED to a
;     timestamped `lan-mouse.bak-<time>` backup next to it, not deleted, so the
;     clean install can be undone by renaming the backup back;
;   * silent (`/S`): clean (move to backup) by default unless /KEEPUSERDATA is
;     passed; /CLEANUSERDATA forces the clean install and /PURGEUSERDATA
;     deletes the old data permanently instead of backing it up;
;   * before extracting, the previous install directory is wiped so binaries
;     from an older build cannot linger next to the new ones.
;
; Uninstall:
;   * the GUI and the daemon it spawned are stopped first (taskkill /T /F);
;   * interactive: ask whether to move the per-user data to a backup;
;   * silent: move it to a backup unless /KEEPUSERDATA (keep in place) or
;     /PURGEUSERDATA (delete permanently) is passed.
;
; Keep the directory names in sync with src/config.rs (`default_path`) and
; src/reset.rs (`lan-mouse --reset-config`).

!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef APPVERSION
  !define APPVERSION "0.2.7"
!endif

!define APPNAME "Lan Mouse"
!define APPID "de.feschber.LanMouse.Custom"
!define PUBLISHER "Gabriel Arcanjo"
!define EXENAME "lan-mouse.exe"
!define FRONTENDEXENAME "lan-mouse-gpui.exe"

; The only directories this installer/uninstaller may touch.
!define LOCAL_USERDATA "$LOCALAPPDATA\lan-mouse"
!define ROAMING_USERDATA "$APPDATA\lan-mouse"

Name "${APPNAME}"
BrandingText "${APPNAME} (unofficial fork)"
OutFile "..\..\lan-mouse-custom-setup-x86_64.exe"
InstallDir "$PROGRAMFILES64\Lan Mouse"
InstallDirRegKey HKLM "Software\${APPID}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Unicode true

Var /GLOBAL KeepUserData    ; 1 = /KEEPUSERDATA was passed
Var /GLOBAL CleanUserData   ; 1 = /CLEANUSERDATA was passed
Var /GLOBAL PurgeUserData   ; 1 = /PURGEUSERDATA was passed
Var /GLOBAL HasOldUserData  ; 1 = per-user data from an older build exists
Var /GLOBAL BackupStamp     ; yyyyMMdd-HHmmss suffix for the backup directory

; --- Diagnostics ---------------------------------------------------------
; Best-effort installer/uninstaller log. The release workflow uploads it when a
; silent install/uninstall fails, so field failures are diagnosable. A %TEMP%
; that cannot be written to must never abort the install (logging is silent).
!define INSTALL_LOG "$TEMP\lan-mouse-install.log"

; Append the line in $0 to the install log. Installer-side copy.
Function LogInstallLine
  FileOpen $R2 "${INSTALL_LOG}" a
  IfErrors log_install_line_done
  FileWrite $R2 "$0$\r$\n"
  FileClose $R2
  log_install_line_done:
FunctionEnd

; Uninstaller-side copy (uninstaller code cannot call installer functions).
Function un.LogInstallLine
  FileOpen $R2 "${INSTALL_LOG}" a
  IfErrors un_log_install_line_done
  FileWrite $R2 "$0$\r$\n"
  FileClose $R2
  un_log_install_line_done:
FunctionEnd

; --- Modern UI -----------------------------------------------------------
!define MUI_ICON "..\..\assets\icon.ico"
!define MUI_UNICON "..\..\assets\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN_TEXT "Launch Lan Mouse"
; The installer runs elevated, and a process launched from here inherits the
; elevated token - the GUI *and* the daemon would then run as administrator
; without the user asking for it (the autostart shortcut is not elevated).
; Keep the option, but leave it unchecked by default.
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!define MUI_FINISHPAGE_RUN_NOTCHECKED
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "PortugueseBR"

; Stop the GUI (lan-mouse-gpui.exe) and the service/daemon it spawned
; (lan-mouse.exe); /T also terminates child processes.
;
; Only processes of the user that started the installer are stopped: a
; per-machine install must not kill another logged-in user's daemon (that
; would drop their input mid-gesture).
;
; Stop order:
;   1. ask the running service to shut down gracefully via the local IPC
;      (lan-mouse cli shutdown): the daemon then releases pressed keys / the
;      pointer, flushes state and removes its IPC socket. This works when the
;      installed build understands the command; an older daemon logs the
;      unknown request and never answers, so the CLI exits non-zero after its
;      5 s IPC timeout - that is the expected fallback path;
;   2. wait up to ~3 s for the process to disappear;
;   3. only then force-kill with taskkill (/F /T), filtered to the installing
;      user, and remove the GPUI frontend process too.
; Every step is logged to %TEMP%\lan-mouse-install.log by the caller-supplied
; logging function (LogInstallLine for the installer, un.LogInstallLine for the
; uninstaller).
!macro StopLanMouse logfn
  ReadEnvStr $R1 "USERNAME"

  ; 1. graceful shutdown over the local IPC (Windows loopback TCP).
  nsExec::ExecToStack '"$INSTDIR\${EXENAME}" cli shutdown'
  Pop $0 ; exit code
  Pop $1 ; combined output
  StrCpy $R3 "$0"
  StrCpy $0 "graceful shutdown attempt: exit=$R3 $1"
  Call ${logfn}

  ; 2. wait up to 6 x 500 ms for the daemon to exit. tasklist CSV output starts
  ; with a quote only when a process matched (the "no tasks" message is
  ; localized, so a prefix test on the message would not be portable).
  StrCpy $R4 "0"
  stop_lan_mouse_wait:
  IntCmp $R4 6 stop_lan_mouse_kill
  Sleep 500
  IntOp $R4 $R4 + 1
  StrCmp $R1 "" stop_lan_mouse_plain_check
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${EXENAME}" /FI "USERNAME eq $R1" /NH /FO CSV'
  Goto stop_lan_mouse_check_result
  stop_lan_mouse_plain_check:
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${EXENAME}" /NH /FO CSV'
  stop_lan_mouse_check_result:
  Pop $0
  Pop $1
  StrCpy $R5 "$1" 1
  StrCmp $R5 "$\"" stop_lan_mouse_wait
  StrCpy $0 "lan-mouse is no longer running (checked $R4 x 500 ms)"
  Call ${logfn}
  Goto stop_lan_mouse_done

  ; 3. force-kill (filtered to the installing user when known).
  stop_lan_mouse_kill:
  StrCpy $0 "graceful shutdown did not complete; forcing taskkill"
  Call ${logfn}
  StrCmp $R1 "" stop_lan_mouse_plain
  nsExec::ExecToStack 'taskkill /F /T /IM "${EXENAME}" /FI "USERNAME eq $R1"'
  Pop $0
  Pop $1
  StrCpy $0 "taskkill ${EXENAME} (filtered): exit=$0 $1"
  Call ${logfn}
  nsExec::ExecToStack 'taskkill /F /T /IM "${FRONTENDEXENAME}" /FI "USERNAME eq $R1"'
  Pop $0
  Pop $1
  StrCpy $0 "taskkill ${FRONTENDEXENAME} (filtered): exit=$0 $1"
  Call ${logfn}
  Goto stop_lan_mouse_done

  stop_lan_mouse_plain:
  nsExec::ExecToStack 'taskkill /F /T /IM "${EXENAME}"'
  Pop $0
  Pop $1
  StrCpy $0 "taskkill ${EXENAME}: exit=$0 $1"
  Call ${logfn}
  nsExec::ExecToStack 'taskkill /F /T /IM "${FRONTENDEXENAME}"'
  Pop $0
  Pop $1
  StrCpy $0 "taskkill ${FRONTENDEXENAME}: exit=$0 $1"
  Call ${logfn}

  stop_lan_mouse_done:
  Sleep 500
!macroend

; Timestamp used for the backup directory name (yyyyMMdd-HHmmss).
!macro ComputeBackupStamp
  ${GetTime} "" "L" $R0 $R1 $R2 $R3 $R4 $R5 $R6
  StrCpy $BackupStamp "$R2$R1$R0-$R3$R4$R5"
!macroend

; Move the per-user data directories to timestamped sibling backups.
;
; The macro keeps its historical name (the clean-install change originally
; deleted the data); it is deliberately a MOVE now: no old device is visible
; after a clean install, but the user can restore everything by renaming
; `%LOCALAPPDATA%\lan-mouse.bak-<time>` back to `%LOCALAPPDATA%\lan-mouse`.
; Never add a shared location ($LOCALAPPDATA, $APPDATA, $INSTDIR root, ...).
!macro RemoveLanMouseUserData
  !insertmacro ComputeBackupStamp

  ; --- %LOCALAPPDATA%\lan-mouse ---
  IfFileExists "${LOCAL_USERDATA}" 0 backup_local_done
    StrCpy $R7 "${LOCAL_USERDATA}.bak-$BackupStamp"
    StrCpy $R9 "1"
    backup_local_unique:
      IfFileExists "$R7" 0 backup_local_move
      StrCpy $R7 "${LOCAL_USERDATA}.bak-$BackupStamp-$R9"
      IntOp $R9 $R9 + 1
      Goto backup_local_unique
    backup_local_move:
      ClearErrors
      Rename "${LOCAL_USERDATA}" "$R7"
      IfErrors 0 backup_local_done
        ; Never continue with the old configuration still in place: the whole
        ; point of the clean install is that old devices do not come back.
        StrCpy $0 "Could not move the old configuration:$\r$\n$\r$\n    ${LOCAL_USERDATA}$\r$\n$\r$\nQuit Lan Mouse (including its daemon) and run the installer again."
        IfSilent backup_local_failed
        MessageBox MB_ICONEXCLAMATION|MB_OK "$0"
      backup_local_failed:
        SetErrorLevel 1
        Abort
  backup_local_done:

  ; --- %APPDATA%\lan-mouse (only portable/older builds write it) ---
  IfFileExists "${ROAMING_USERDATA}" 0 backup_roaming_done
    StrCpy $R8 "${ROAMING_USERDATA}.bak-$BackupStamp"
    StrCpy $R9 "1"
    backup_roaming_unique:
      IfFileExists "$R8" 0 backup_roaming_move
      StrCpy $R8 "${ROAMING_USERDATA}.bak-$BackupStamp-$R9"
      IntOp $R9 $R9 + 1
      Goto backup_roaming_unique
    backup_roaming_move:
      ClearErrors
      Rename "${ROAMING_USERDATA}" "$R8"
      IfErrors 0 backup_roaming_done
        StrCpy $0 "Could not move the old roaming configuration:$\r$\n$\r$\n    ${ROAMING_USERDATA}$\r$\n$\r$\nQuit Lan Mouse (including its daemon) and run the installer again."
        IfSilent backup_roaming_failed
        MessageBox MB_ICONEXCLAMATION|MB_OK "$0"
      backup_roaming_failed:
        SetErrorLevel 1
        Abort
  backup_roaming_done:
!macroend

; Permanent deletion, only used with --purge / /PURGEUSERDATA.
!macro PurgeLanMouseUserData
  RMDir /r "${LOCAL_USERDATA}"
  RMDir /r "${ROAMING_USERDATA}"
!macroend

Function .onInit
  StrCpy $0 "=== Lan Mouse installer ${APPVERSION} ==="
  Call LogInstallLine
  !insertmacro StopLanMouse LogInstallLine

  ; --- command line flags (interactive/silent deployments) ---
  StrCpy $KeepUserData "0"
  ${GetOptions} $CMDLINE "/KEEPUSERDATA" $R0
  IfErrors keep_flag_absent
    StrCpy $KeepUserData "1"
  keep_flag_absent:

  StrCpy $CleanUserData "0"
  ${GetOptions} $CMDLINE "/CLEANUSERDATA" $R0
  IfErrors clean_flag_absent
    StrCpy $CleanUserData "1"
  clean_flag_absent:

  StrCpy $PurgeUserData "0"
  ${GetOptions} $CMDLINE "/PURGEUSERDATA" $R0
  IfErrors purge_flag_absent
    StrCpy $PurgeUserData "1"
  purge_flag_absent:

  ; --- leftover data from an older/portable build? ---
  StrCpy $HasOldUserData "0"
  IfFileExists "${LOCAL_USERDATA}\*.*" old_user_data_found
  IfFileExists "${ROAMING_USERDATA}\*.*" old_user_data_found
  Goto old_user_data_done
  old_user_data_found:
    StrCpy $HasOldUserData "1"
  old_user_data_done:

  StrCmp $HasOldUserData "0" on_init_done
  StrCmp $KeepUserData "1" on_init_done
  StrCmp $CleanUserData "1" on_init_clean
  IfSilent on_init_clean
  !insertmacro ComputeBackupStamp
  MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON1 \
    "An existing Lan Mouse configuration was found:$\r$\n    ${LOCAL_USERDATA}$\r$\n$\r$\nStart fresh? The old configuration is MOVED to a backup (not deleted), so it can be restored by renaming the backup back:$\r$\n    ${LOCAL_USERDATA}.bak-$BackupStamp$\r$\n$\r$\nYes: move it to the backup and start fresh (recommended)$\r$\nNo:  keep it in place (upgrade)" \
    IDYES on_init_clean IDNO on_init_done
  on_init_clean:
    StrCmp $PurgeUserData "1" on_init_purge
    !insertmacro RemoveLanMouseUserData
    Goto on_init_done
  on_init_purge:
    !insertmacro PurgeLanMouseUserData
  on_init_done:
FunctionEnd

Section "Lan Mouse" SEC_MAIN
  ; Upgrading in place: wipe the previous install directory (only when the
  ; registry confirms it is ours) so files from an older build cannot mix
  ; with the new ones. A custom/foreign directory is left alone.
  ReadRegStr $0 HKLM "Software\${APPID}" "InstallDir"
  StrCmp $0 "$INSTDIR" 0 skip_clean_installdir
  RMDir /r "$INSTDIR"
  skip_clean_installdir:

  SetOutPath "$INSTDIR"
  File /r "..\..\lan-mouse-windows\*.*"
  File "..\..\assets\icon.ico"

  ; Shortcuts carry the icon explicitly so they look right even if the shell
  ; has not refreshed its icon cache yet.
  CreateShortCut "$DESKTOP\Lan Mouse.lnk" "$INSTDIR\${EXENAME}" "" "$INSTDIR\icon.ico" 0
  CreateDirectory "$SMPROGRAMS\Lan Mouse"
  CreateShortCut "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk" "$INSTDIR\${EXENAME}" "" "$INSTDIR\icon.ico" 0
  CreateShortCut "$SMPROGRAMS\Lan Mouse\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\${APPID}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}" "DisplayName" "${APPNAME} (unofficial fork)"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}" "DisplayVersion" "${APPVERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}" "DisplayIcon" "$INSTDIR\${EXENAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}" "UninstallString" "$INSTDIR\uninstall.exe"

  ; Inbound traffic for the DTLS transport (4242/udp) and the discovery
  ; beacons (4245/udp), so a peer can reach this device without a manual
  ; firewall prompt. The rule is program-scoped and protocol-agnostic, so both
  ; UDP ports are covered (and stay covered if the ports are reconfigured in
  ; config.toml). Remove stale rules first: upgrades would otherwise accumulate
  ; duplicate rules pointing at old install paths. Public is included
  ; deliberately: Windows often marks a Wi-Fi LAN as a "Public" network, and a
  ; private/domain-only rule silently drops the peer's handshake (the UI then
  ; shows "connecting/waiting" on the other side with no authorization prompt
  ; ever appearing here).
  ;
  ; netsh failures are logged and surfaced as a warning instead of aborting the
  ; install: the app still works and a policy-managed or manual rule can be
  ; added later. The log path is %TEMP%\lan-mouse-install.log.
  nsExec::ExecToStack 'netsh advfirewall firewall delete rule name="Lan Mouse"'
  Pop $0 ; exit code ("0" on success)
  Pop $1 ; combined output
  StrCpy $R3 "$0"
  StrCpy $0 "firewall delete: exit=$R3 $1"
  Call LogInstallLine

  nsExec::ExecToStack 'netsh advfirewall firewall add rule name="Lan Mouse" dir=in action=allow program="$INSTDIR\${EXENAME}" enable=yes profile=private,domain,public'
  Pop $0
  Pop $1
  StrCmp $0 "0" lan_mouse_firewall_ok
    StrCpy $R3 "$0"
    StrCpy $0 "firewall add FAILED: exit=$R3 $1"
    Call LogInstallLine
    DetailPrint "Warning: could not add the Windows firewall rule (netsh exit $R3); inbound connections may be blocked."
    IfSilent lan_mouse_firewall_ok
    MessageBox MB_ICONEXCLAMATION|MB_OK "Could not add the Windows firewall rule for Lan Mouse.$\r$\n$\r$\nInbound connections may be blocked; allow $INSTDIR\${EXENAME} manually in Windows Defender Firewall.$\r$\n$\r$\nDetails were written to ${INSTALL_LOG}."
  lan_mouse_firewall_ok:
SectionEnd

; Checked by default: a KVM is only useful if it is running.
Section "Start with Windows" SEC_STARTUP
  CreateShortCut "$SMSTARTUP\Lan Mouse.lnk" "$INSTDIR\${EXENAME}" "" "$INSTDIR\icon.ico" 0
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN} "Lan Mouse itself, its icons and shortcuts."
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_STARTUP} "Start Lan Mouse automatically on login (recommended)."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function .onInstSuccess
  ; Refresh the shell so the new shortcuts get their icon immediately.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'
FunctionEnd

Section "Uninstall"
  !insertmacro StopLanMouse un.LogInstallLine

  ; --- per-user data ---
  ; Silent uninstall moves it to a timestamped backup; interactive uninstall
  ; asks first. /KEEPUSERDATA keeps it in place, /PURGEUSERDATA deletes it.
  StrCpy $KeepUserData "0"
  ${GetOptions} $CMDLINE "/KEEPUSERDATA" $R0
  IfErrors un_keep_flag_absent
    StrCpy $KeepUserData "1"
  un_keep_flag_absent:

  StrCpy $PurgeUserData "0"
  ${GetOptions} $CMDLINE "/PURGEUSERDATA" $R0
  IfErrors un_purge_flag_absent
    StrCpy $PurgeUserData "1"
  un_purge_flag_absent:

  StrCmp $KeepUserData "1" un_user_data_done
  IfSilent un_user_data_backup
  !insertmacro ComputeBackupStamp
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON1 \
    "Also move this user's Lan Mouse configuration, certificates, permissions and audit log to a backup?$\r$\n    ${LOCAL_USERDATA}.bak-$BackupStamp$\r$\n$\r$\nChoose No to leave them in place for a later reinstall." \
    IDYES un_user_data_backup IDNO un_user_data_done
  un_user_data_backup:
    StrCmp $PurgeUserData "1" un_user_data_purge
    !insertmacro RemoveLanMouseUserData
    Goto un_user_data_done
  un_user_data_purge:
    !insertmacro PurgeLanMouseUserData
  un_user_data_done:

  Delete "$SMSTARTUP\Lan Mouse.lnk"
  Delete "$DESKTOP\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Uninstall.lnk"
  RMDir "$SMPROGRAMS\Lan Mouse"

  nsExec::ExecToStack 'netsh advfirewall firewall delete rule name="Lan Mouse"'
  Pop $0 ; exit code ("0" on success)
  Pop $1 ; combined output
  StrCmp $0 "0" un_firewall_ok
    StrCpy $R3 "$0"
    StrCpy $0 "uninstall firewall delete FAILED: exit=$R3 $1"
    Call un.LogInstallLine
  un_firewall_ok:

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}"
  DeleteRegKey HKLM "Software\${APPID}"
  RMDir /r "$INSTDIR"
SectionEnd
