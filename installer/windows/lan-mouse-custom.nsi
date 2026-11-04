; NSIS installer for the unofficial Lan Mouse fork.
;
; Built by .github/workflows/release.yml on the Windows runner after
; `lan-mouse-windows\` was populated with lan-mouse.exe and the GTK dlls.
; The version is passed with /DAPPVERSION=<version>.

!include "MUI2.nsh"

!ifndef APPVERSION
  !define APPVERSION "0.11.0"
!endif

!define APPNAME "Lan Mouse"
!define APPID "de.feschber.LanMouse.Custom"
!define PUBLISHER "Gabriel Arcanjo"
!define EXENAME "lan-mouse.exe"

Name "${APPNAME}"
BrandingText "${APPNAME} (unofficial fork)"
OutFile "..\..\lan-mouse-custom-setup-x86_64.exe"
InstallDir "$PROGRAMFILES64\Lan Mouse"
InstallDirRegKey HKLM "Software\${APPID}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Unicode true

; --- Modern UI -----------------------------------------------------------
!define MUI_ICON "..\..\assets\icon.ico"
!define MUI_UNICON "..\..\assets\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN_TEXT "Launch Lan Mouse"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "PortugueseBR"

Function .onInit
  ; Upgrade in place: close a running instance so the files can be replaced.
  nsExec::ExecToLog 'taskkill /F /IM ${EXENAME}'
FunctionEnd

Section "Lan Mouse" SEC_MAIN
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

  ; Inbound UDP for the DTLS listener, so the peer can reach this device
  ; without a manual firewall prompt.
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="Lan Mouse" dir=in action=allow program="$INSTDIR\${EXENAME}" enable=yes profile=private,domain'
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
  nsExec::ExecToLog 'taskkill /F /IM ${EXENAME}'

  Delete "$SMSTARTUP\Lan Mouse.lnk"
  Delete "$DESKTOP\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Uninstall.lnk"
  RMDir "$SMPROGRAMS\Lan Mouse"

  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="Lan Mouse"'

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}"
  DeleteRegKey HKLM "Software\${APPID}"
  RMDir /r "$INSTDIR"
SectionEnd
