; Syncra installer (Inno Setup 6). Built by deploy/windows/build-setup.sh,
; which passes AppVersion, AppExe (portable build) and Vigem (ViGEmBus setup).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{A4788CAF-37F7-41BE-AEDB-6F567A30915A}
AppName=Syncra
AppVersion={#AppVersion}
AppPublisher=Syncra
DefaultDirName={autopf}\Syncra
DefaultGroupName=Syncra
DisableProgramGroupPage=yes
; The gamepad driver and the firewall rule need admin rights.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputBaseFilename=Syncra-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
UninstallDisplayIcon={app}\Syncra.exe

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"
Name: "gamepad"; Description: "Install the gamepad driver (ViGEmBus), needed to share gamepads from this PC"; Check: not VigemInstalled

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; DestName: "Syncra.exe"; Flags: ignoreversion
Source: "{#Vigem}"; DestDir: "{tmp}"; DestName: "ViGEmBus_setup.exe"; Tasks: gamepad; Flags: deleteafterinstall

[Icons]
Name: "{group}\Syncra"; Filename: "{app}\Syncra.exe"
Name: "{group}\Uninstall Syncra"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Syncra"; Filename: "{app}\Syncra.exe"; Tasks: desktopicon

[Run]
; The stream is UDP to and from the other PC: allow the app through the firewall.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""parsec-rs"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Syncra"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Syncra"" dir=in action=allow program=""{app}\Syncra.exe"" enable=yes profile=any"; Flags: runhidden
Filename: "{tmp}\ViGEmBus_setup.exe"; Parameters: "/qn /norestart"; StatusMsg: "Installing the gamepad driver..."; Tasks: gamepad
Filename: "{app}\Syncra.exe"; Description: "Start Syncra"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Syncra"""; Flags: runhidden; RunOnceId: "fwrule"

[Code]
function VigemInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ViGEmBus');
end;

function InitializeSetup: Boolean;
begin
  Result := True;
  // NVENC (sharing) and NVDEC (connecting) come with the NVIDIA driver.
  if not (FileExists(ExpandConstant('{sys}\nvEncodeAPI64.dll')) and FileExists(ExpandConstant('{sys}\nvcuda.dll'))) then
    Result := MsgBox('No NVIDIA driver was found on this PC.' + #13#10#13#10 +
      'Syncra needs an NVIDIA graphics card with its driver installed, both to share this PC and to connect to another one.' + #13#10#13#10 +
      'Install anyway?', mbConfirmation, MB_YESNO) = IDYES;
end;
