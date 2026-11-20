Unicode true
ManifestDPIAware true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
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
!define PUBLISHER "quikscope contributors"
!define HOMEPAGE "https://github.com/qt715/quikscope"
!define ART "${SOURCE}\assets\branding\installer"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope"

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
BrandingText "quikscope ${VERSION}"
# NSIS gives Uninstall.exe the installer's version resource, so these strings
# describe both.
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "quikscope"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "quikscope Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "${PUBLISHER}"

# MUI2's own icon default would override a plain Icon directive.
!define MUI_ICON "${SOURCE}\assets\branding\quikscope.ico"
!define MUI_UNICON "${SOURCE}\assets\branding\quikscope.ico"
# Must match the background of assets/branding/installer/header.svg.
!define MUI_BGCOLOR "F8F7FE"
!define MUI_TEXTCOLOR "1E1D2B"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
# 96 DPI defaults; qs.GuiInit and the page show functions swap in the
# rasterisation for the system DPI.
!define MUI_HEADERIMAGE_BITMAP "${ART}\header-100.bmp"
!define MUI_WELCOMEFINISHPAGE_BITMAP "${ART}\sidebar-100.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "${ART}\sidebar-100.bmp"
!define MUI_CUSTOMFUNCTION_GUIINIT qs.GuiInit
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.qs.GuiInit
!define MUI_FINISHPAGE_NOREBOOTSUPPORT
!define MUI_FINISHPAGE_LINK_COLOR "4B47B8"
!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TITLE "quikscope ${VERSION}"
!define MUI_WELCOMEPAGE_TEXT "Setup installs quikscope ${VERSION} for your account. An existing installation is upgraded; your settings are kept."
!define MUI_PAGE_CUSTOMFUNCTION_SHOW qs.WelcomeShow
!insertmacro MUI_PAGE_WELCOME
!define MUI_PAGE_HEADER_TEXT "License"
!define MUI_PAGE_HEADER_SUBTEXT "MIT or Apache License 2.0, at your choice."
!define MUI_LICENSEPAGE_TEXT_TOP "Both licences and the third-party notices are installed with quikscope."
!define MUI_LICENSEPAGE_TEXT_BOTTOM "Click Next to continue."
!define MUI_LICENSEPAGE_BUTTON "$(^NextBtn)"
!insertmacro MUI_PAGE_LICENSE "${SOURCE}\LICENSE-MIT"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_TITLE "quikscope is installed"
!define MUI_FINISHPAGE_TEXT "quikscope runs in the notification area."
# Silent installs never reach this page, so /S does not start the daemon.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Start quikscope now"
!define MUI_FINISHPAGE_RUN_FUNCTION qs.Start
!define MUI_FINISHPAGE_LINK "Windows guide"
!define MUI_FINISHPAGE_LINK_LOCATION "${HOMEPAGE}#windows-1011"
!define MUI_PAGE_CUSTOMFUNCTION_SHOW qs.FinishShow
!insertmacro MUI_PAGE_FINISH
!define MUI_UNCONFIRMPAGE_TEXT_TOP "quikscope is closed and removed from this folder. Settings, screenshots and recordings are kept."
!define MUI_PAGE_HEADER_SUBTEXT "Remove quikscope from your account."
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!define MUI_FINISHPAGE_TITLE "quikscope is uninstalled"
!define MUI_FINISHPAGE_TEXT "Settings remain in $APPDATA\quikscope."
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.qs.FinishShow
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

!macro QsArtFiles SCALE
  File "/oname=$PLUGINSDIR\qs-sidebar.bmp" "${ART}\sidebar-${SCALE}.bmp"
  File "/oname=$PLUGINSDIR\qs-header.bmp" "${ART}\header-${SCALE}.bmp"
!macroend

!macro QsArtFunctions UN
  # Stack: control, bitmap path -> bitmap handle now owned by the control.
  # GDI's bitmap stretching (MUI's FitControl) blurs or drops pixels, so the
  # bitmap is shown unscaled and centred; the art's uniform edge colour fills
  # or absorbs the few pixels it differs from the dialog-unit control size.
  Function ${UN}qs.SetImage
    System::Store "S"
    Pop $1
    Pop $0
    System::Call 'user32::GetClientRect(pr0,@r2)'
    System::Call '*$2(i,i,i.r3,i.r4)'
    System::Call 'user32::GetWindowLong(pr0,i-16)i.r5'
    IntOp $5 $5 | 0x200 # SS_CENTERIMAGE
    System::Call 'user32::SetWindowLong(pr0,i-16,ir5)'
    System::Call 'user32::LoadImage(p0,tr1,i0,i0,i0,i0x10)p.r6' # IMAGE_BITMAP, LR_LOADFROMFILE
    SendMessage $0 0x172 0 $6 $7 # STM_SETIMAGE
    System::Call 'gdi32::DeleteObject(pr7)'
    System::Call 'user32::SetWindowPos(pr0,p0,i0,i0,ir3,ir4,i0x16)' # NOMOVE|NOZORDER|NOACTIVATE
    Push $6
    System::Store "L"
  FunctionEnd

  Function ${UN}qs.GuiInit
    System::Store "S"
    InitPluginsDir
    # Setup is system-DPI aware: pick the largest rasterisation not above it.
    System::Call 'user32::GetDC(p0)p.r1'
    System::Call 'gdi32::GetDeviceCaps(pr1,i88)i.r0' # LOGPIXELSX
    System::Call 'user32::ReleaseDC(p0,pr1)'
    ${If} $0 >= 288
      !insertmacro QsArtFiles 300
    ${ElseIf} $0 >= 240
      !insertmacro QsArtFiles 250
    ${ElseIf} $0 >= 216
      !insertmacro QsArtFiles 225
    ${ElseIf} $0 >= 192
      !insertmacro QsArtFiles 200
    ${ElseIf} $0 >= 168
      !insertmacro QsArtFiles 175
    ${ElseIf} $0 >= 144
      !insertmacro QsArtFiles 150
    ${ElseIf} $0 >= 120
      !insertmacro QsArtFiles 125
    ${Else}
      !insertmacro QsArtFiles 100
    ${EndIf}
    GetDlgItem $0 $HWNDPARENT 1046
    Push $0
    Push "$PLUGINSDIR\qs-header.bmp"
    Call ${UN}qs.SetImage
    # The header bitmap lives as long as the window.
    Pop $0
    System::Store "L"
  FunctionEnd

  Function ${UN}qs.FinishShow
    Push $mui.FinishPage.Image
    Push "$PLUGINSDIR\qs-sidebar.bmp"
    Call ${UN}qs.SetImage
    # MUI frees this handle when the page closes.
    Pop $mui.FinishPage.Image.Bitmap
  FunctionEnd
!macroend
!insertmacro QsArtFunctions ""
!insertmacro QsArtFunctions "un."

Function qs.WelcomeShow
  Push $mui.WelcomePage.Image
  Push "$PLUGINSDIR\qs-sidebar.bmp"
  Call qs.SetImage
  Pop $mui.WelcomePage.Image.Bitmap
FunctionEnd

Function qs.Start
  # Setup normally runs unelevated, so the daemon inherits the user's token.
  # If it was started elevated anyway, the shell opens the Start menu entry so
  # the daemon still runs unelevated.
  System::Call 'shell32::IsUserAnAdmin()i.r0'
  ${If} $0 <> 0
    Exec '"$WINDIR\explorer.exe" "$SMPROGRAMS\quikscope.lnk"'
  ${Else}
    Exec '"$INSTDIR\quikscope.exe" daemon'
  ${EndIf}
FunctionEnd

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
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "quikscope"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\quikscope.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "${HOMEPAGE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "HelpLink" "${HOMEPAGE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLUpdateInfo" "${HOMEPAGE}/releases"
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section /o "Start at login" Autostart
  ExecWait '"$INSTDIR\quikscope.exe" autostart on'
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${Main} "quikscope, a Start menu entry and $\"Open with$\" entries for images and videos."
  !insertmacro MUI_DESCRIPTION_TEXT ${Autostart} "Start quikscope when you sign in."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

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
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
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
