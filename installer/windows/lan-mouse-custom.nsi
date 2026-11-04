; NSIS installer for the unofficial Lan Mouse fork.
;
; Built by .github/workflows/release.yml on the Windows runner after
; `lan-mouse-windows\` was populated with lan-mouse.exe and the GTK dlls.

!define APPNAME "Lan Mouse"
!define APPID "de.feschber.LanMouse.Custom"
!define APPVERSION "0.11.0"
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

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File /r "..\..\lan-mouse-windows\*.*"

  ; Shortcuts: desktop + start menu.
  CreateShortCut "$DESKTOP\Lan Mouse.lnk" "$INSTDIR\${EXENAME}"
  CreateDirectory "$SMPROGRAMS\Lan Mouse"
  CreateShortCut "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk" "$INSTDIR\${EXENAME}"
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

Section "Uninstall"
  Delete "$DESKTOP\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Lan Mouse.lnk"
  Delete "$SMPROGRAMS\Lan Mouse\Uninstall.lnk"
  RMDir "$SMPROGRAMS\Lan Mouse"

  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="Lan Mouse"'

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}"
  DeleteRegKey HKLM "Software\${APPID}"
  RMDir /r "$INSTDIR"
SectionEnd
