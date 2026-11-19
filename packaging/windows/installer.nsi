Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!ifndef VERSION
  !error "Pass /DVERSION=<Cargo version>"
!endif
!ifndef SOURCE
  !define SOURCE "..\.."
!endif
!ifndef BINARY
  !define BINARY "${SOURCE}\target\release\quikscope.exe"
!endif
!ifndef OUTPUT
  !define OUTPUT "${SOURCE}/dist/quikscope-${VERSION}-windows-x86_64-installer.exe"
!endif

!macro OpenWith EXT
  WriteRegStr HKCU "Software\Classes\${EXT}\OpenWithProgids" "quikscope.media" ""
  WriteRegStr HKCU "Software\Classes\Applications\quikscope.exe\SupportedTypes" "${EXT}" ""
!macroend
!macro RemoveOpenWith EXT
  DeleteRegValue HKCU "Software\Classes\${EXT}\OpenWithProgids" "quikscope.media"
!macroend
Name "quikscope"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\quikscope"
InstallDirRegKey HKCU "Software\quikscope" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "${SOURCE}\assets\branding\quikscope.ico"
UninstallIcon "${SOURCE}\assets\branding\quikscope.ico"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "quikscope"
VIAddVersionKey "FileDescription" "quikscope per-user installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "quikscope contributors"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${SOURCE}\LICENSE-MIT"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

!macro RemoveStoppedExecutable
  # quit acknowledges IPC before the daemon has fully unmapped its image.
  # Wait on Windows' file lock rather than leaving an executable behind.
  StrCpy $0 0
  ${Do}
    ClearErrors
    Delete "$INSTDIR\quikscope.exe"
    ${IfNot} ${Errors}
      ${ExitDo}
    ${EndIf}
    IntOp $0 $0 + 1
    ${If} $0 >= 100
      SetErrorLevel 1
      Abort "quikscope is still running. Close it and try again."
    ${EndIf}
    Sleep 100
  ${Loop}
!macroend

Section "quikscope (required)" Main
  SectionIn RO
  SetShellVarContext current
  # Stop the previous version before replacing its executable on upgrade.
  IfFileExists "$INSTDIR\quikscope.exe" 0 stopped
  ExecWait '"$INSTDIR\quikscope.exe" quit'
  !insertmacro RemoveStoppedExecutable
  stopped:
  SetOutPath "$INSTDIR"
  File /oname=quikscope.exe "${BINARY}"
  File "${SOURCE}\assets\branding\quikscope.ico"
  File "${SOURCE}\LICENSE-MIT"
  File "${SOURCE}\LICENSE-APACHE"
  File "${SOURCE}\THIRD-PARTY-LICENSES.html"
  File "${SOURCE}\README.md"
  File "${SOURCE}\assets\fonts\INTER-LICENSE.txt"
  File /oname=LUCIDE-LICENSE.txt "${SOURCE}\assets\icons\LICENSE"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\quikscope.lnk" "$INSTDIR\quikscope.exe" "daemon" "$INSTDIR\quikscope.ico"
  # Explorer's "Open with" list: images, animations and videos. Defaults
  # stay with the user's existing apps; Windows decides those itself.
  WriteRegStr HKCU "Software\Classes\quikscope.media" "" "quikscope media"
  WriteRegStr HKCU "Software\Classes\quikscope.media\DefaultIcon" "" "$INSTDIR\quikscope.ico"
  WriteRegStr HKCU "Software\Classes\quikscope.media\shell\open\command" "" '"$INSTDIR\quikscope.exe" open "%1"'
  WriteRegStr HKCU "Software\Classes\Applications\quikscope.exe\shell\open\command" "" '"$INSTDIR\quikscope.exe" open "%1"'
  !insertmacro OpenWith ".png"
  !insertmacro OpenWith ".jpg"
  !insertmacro OpenWith ".jpeg"
  !insertmacro OpenWith ".webp"
  !insertmacro OpenWith ".gif"
  !insertmacro OpenWith ".avif"
  !insertmacro OpenWith ".jxl"
  !insertmacro OpenWith ".mp4"
  !insertmacro OpenWith ".mkv"
  !insertmacro OpenWith ".mov"
  WriteRegStr HKCU "Software\quikscope" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "DisplayName" "quikscope"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "Publisher" "quikscope contributors"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "DisplayIcon" "$INSTDIR\quikscope.ico"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope" "NoRepair" 1
SectionEnd

Section /o "Start at login" Autostart
  ExecWait '"$INSTDIR\quikscope.exe" autostart on'
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  ExecWait '"$INSTDIR\quikscope.exe" quit'
  ExecWait '"$INSTDIR\quikscope.exe" autostart off'
  !insertmacro RemoveStoppedExecutable
  # Remove only known app cache files, never a user's custom screenshot files.
  Delete "$LOCALAPPDATA\quikscope\last.raw"
  Delete "$LOCALAPPDATA\quikscope\last.raw.*.part"
  Delete "$LOCALAPPDATA\quikscope\daemon.log"
  Delete "$LOCALAPPDATA\quikscope\daemon.log.1"
  Delete "$LOCALAPPDATA\quikscope\ipc.sock"
  # Lossless animation masters and clipboard clips are cache, unlike saved
  # recordings.
  RMDir /r "$LOCALAPPDATA\quikscope\recordings"
  RMDir /r "$LOCALAPPDATA\quikscope\clips"
  RMDir "$LOCALAPPDATA\quikscope"
  Delete "$SMPROGRAMS\quikscope.lnk"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope"
  DeleteRegKey HKCU "Software\quikscope"
  DeleteRegKey HKCU "Software\Classes\quikscope.media"
  DeleteRegKey HKCU "Software\Classes\Applications\quikscope.exe"
  !insertmacro RemoveOpenWith ".png"
  !insertmacro RemoveOpenWith ".jpg"
  !insertmacro RemoveOpenWith ".jpeg"
  !insertmacro RemoveOpenWith ".webp"
  !insertmacro RemoveOpenWith ".gif"
  !insertmacro RemoveOpenWith ".avif"
  !insertmacro RemoveOpenWith ".jxl"
  !insertmacro RemoveOpenWith ".mp4"
  !insertmacro RemoveOpenWith ".mkv"
  !insertmacro RemoveOpenWith ".mov"
  Delete "$INSTDIR\quikscope.ico"
  Delete "$INSTDIR\LICENSE-MIT"
  Delete "$INSTDIR\LICENSE-APACHE"
  Delete "$INSTDIR\THIRD-PARTY-LICENSES.html"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\INTER-LICENSE.txt"
  Delete "$INSTDIR\LUCIDE-LICENSE.txt"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  # User configuration and screenshots intentionally remain untouched.
SectionEnd
