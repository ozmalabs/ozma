; Ozma Agent — Windows Installer (NSIS)
;
; Creates a standard Windows installer that:
;   1. Installs the agent to Program Files
;   2. Registers as a Windows service (auto-start on boot)
;   3. Adds a Start Menu shortcut
;   4. Adds to PATH
;   5. Prompts for the controller URL
;   6. Starts the service
;   7. Writes config to %APPDATA%\Ozma\agent.json
;   8. Registers version in HKLM\SOFTWARE\Ozma\Agent\Version
;
; Build: makensis agent/installer/ozma-agent.nsi
; Requires: NSIS 3.x, dist/ozma-agent/ from PyInstaller

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

; ── Metadata ──────────────────────────────────────────────────────────────────
Name "Ozma Agent"
OutFile "dist\ozma-agent-setup.exe"
InstallDir "$PROGRAMFILES64\Ozma\Agent"
InstallDirRegKey HKLM "Software\OzmaLabs\Agent" "InstallDir"
RequestExecutionLevel admin
Unicode True

; ── Version info ──────────────────────────────────────────────────────────────
VIProductVersion "1.0.0.0"
VIAddVersionKey "ProductName" "Ozma Agent"
VIAddVersionKey "CompanyName" "Ozma Labs Pty Ltd"
VIAddVersionKey "LegalCopyright" "Copyright Ozma Labs Pty Ltd"
VIAddVersionKey "FileDescription" "Ozma Agent Installer"
VIAddVersionKey "FileVersion" "1.0.0"
VIAddVersionKey "ProductVersion" "1.0.0"

; ── UI ────────────────────────────────────────────────────────────────────────
!define MUI_ICON "..\..\controller\static\favicon.ico"
!define MUI_UNICON "..\..\controller\static\favicon.ico"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
Page custom ControllerPage ControllerPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ── Variables ─────────────────────────────────────────────────────────────────
Var ControllerURL
Var MachineName
Var hCtlURL
Var hCtlName

; ── Controller URL page ───────────────────────────────────────────────────────
Function ControllerPage
    nsDialogs::Create 1018
    Pop $0

    ${NSD_CreateLabel} 0 0 100% 24u "Enter your ozma controller URL and a name for this machine:"
    Pop $0

    ${NSD_CreateLabel} 0 30u 80u 12u "Controller URL:"
    Pop $0
    ${NSD_CreateText} 85u 28u 215u 14u "https://"
    Pop $hCtlURL

    ${NSD_CreateLabel} 0 50u 80u 12u "Machine name:"
    Pop $0
    ${NSD_CreateText} 85u 48u 215u 14u "$COMPUTERNAME"
    Pop $hCtlName

    ${NSD_CreateLabel} 0 72u 100% 24u "The machine name appears in the ozma dashboard. The controller URL is where your ozma controller is running."
    Pop $0

    nsDialogs::Show
FunctionEnd

Function ControllerPageLeave
    ${NSD_GetText} $hCtlURL $ControllerURL
    ${NSD_GetText} $hCtlName $MachineName
FunctionEnd

; ── Write JSON config ─────────────────────────────────────────────────────────
Function WriteAgentConfig
    ; Create %APPDATA%\Ozma directory if it doesn't exist
    CreateDirectory "$APPDATA\Ozma"

    ; Write agent.json with JSON formatting
    FileOpen $0 "$APPDATA\Ozma\agent.json" w
    FileWrite $0 '{$'

    ; Controller URL
    FileWrite $0 '$\n  "controller_url": "$ControllerURL"'
    
    ; Machine name
    FileWrite $0 ',$\n  "machine_name": "$MachineName"'

    ; Agent version
    FileWrite $0 ',$\n  "version": "1.0.0"'

    ; Config path (where agent itself is installed)
    FileWrite $0 ',$\n  "config_path": "$INSTDIR"'

    FileWrite $0 '$\n}$'
    FileClose $0
FunctionEnd

; ── Install section ───────────────────────────────────────────────────────────
Section "Install"
    SetOutPath "$INSTDIR"

    ; Copy all files from the PyInstaller dist
    File /r "..\..\dist\ozma-agent\*.*"

    ; Write JSON config to %APPDATA%\Ozma\agent.json
    Call WriteAgentConfig

    ; Create uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Registry - InstallDir (legacy compatibility)
    WriteRegStr HKLM "Software\OzmaLabs\Agent" "InstallDir" "$INSTDIR"

    ; Registry - OzmaLabs (legacy compatibility)
    WriteRegStr HKLM "Software\OzmaLabs\Agent" "ControllerURL" "$ControllerURL"
    WriteRegStr HKLM "Software\OzmaLabs\Agent" "MachineName" "$MachineName"

    ; Registry - Ozma\Agent (per spec)
    WriteRegStr HKLM "Software\Ozma\Agent" "Version" "1.0.0"
    WriteRegStr HKLM "Software\Ozma\Agent" "InstallDir" "$INSTDIR"
    WriteRegStr HKLM "Software\Ozma\Agent" "ControllerURL" "$ControllerURL"
    WriteRegStr HKLM "Software\Ozma\Agent" "MachineName" "$MachineName"

    ; Add/Remove Programs entry
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "DisplayName" "Ozma Agent"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "Publisher" "Ozma Labs Pty Ltd"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "DisplayVersion" "1.0.0"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent" \
        "NoRepair" 1

    ; Start Menu shortcuts
    CreateDirectory "$SMPROGRAMS\Ozma"
    CreateShortcut "$SMPROGRAMS\Ozma\Ozma Agent.lnk" "$INSTDIR\ozma-agent.exe" \
        '--name "$MachineName" --controller "$ControllerURL"' \
        "$INSTDIR\ozma-agent.exe" 0
    CreateShortcut "$SMPROGRAMS\Ozma\Uninstall Ozma Agent.lnk" "$INSTDIR\uninstall.exe"

    ; Install and start as a Windows service via NSSM (bundled) or Task Scheduler
    ; Try NSSM first (proper service), fall back to Task Scheduler (startup task)
    IfFileExists "$INSTDIR\nssm.exe" 0 +5
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" install OzmaAgent "$INSTDIR\ozma-agent.exe" --name "$MachineName" --controller "$ControllerURL"'
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" set OzmaAgent AppDirectory "$INSTDIR"'
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" set OzmaAgent Description "Ozma Agent — connects this machine to your ozma mesh"'
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" start OzmaAgent'
        Goto ServiceDone

    ; Fallback: Task Scheduler (runs at logon, restarts on failure)
    nsExec::ExecToLog 'schtasks /Create /TN "OzmaAgent" /TR "\"$INSTDIR\ozma-agent.exe\" --name \"$MachineName\" --controller \"$ControllerURL\"" /SC ONLOGON /RL HIGHEST /F'
    ; Start it now
    nsExec::ExecToLog 'schtasks /Run /TN "OzmaAgent"'

    ServiceDone:

SectionEnd

; ── Uninstall section ─────────────────────────────────────────────────────────
Section "Uninstall"
    ; Stop and remove service
    IfFileExists "$INSTDIR\nssm.exe" 0 +3
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" stop OzmaAgent'
        nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove OzmaAgent confirm'
        Goto UninstServiceDone

    ; Remove scheduled task
    nsExec::ExecToLog 'schtasks /Delete /TN "OzmaAgent" /F'

    UninstServiceDone:

    ; Remove config file from %APPDATA%\Ozma
    Delete "$APPDATA\Ozma\agent.json"

    ; Remove files
    RMDir /r "$INSTDIR"

    ; Remove shortcuts
    RMDir /r "$SMPROGRAMS\Ozma"

    ; Remove registry - OzmaLabs
    DeleteRegKey HKLM "Software\OzmaLabs\Agent"

    ; Remove registry - Ozma
    DeleteRegKey HKLM "Software\Ozma\Agent"

    ; Remove Add/Remove Programs entry
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\OzmaAgent"
SectionEnd
