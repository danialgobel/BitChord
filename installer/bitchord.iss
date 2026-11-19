; Inno Setup script for BitChord.
; Build: cargo build --release, then
;   iscc installer\bitchord.iss
; Output: installer\Output\BitChord-Setup-<version>.exe

#define AppName "BitChord"
#define AppVersion "2.0.2"
#define AppExe "bitchord.exe"
#ifdef Arm64
  #define Arch "arm64"
  #define Suffix "-arm64"
  #define BinDir "..\target\aarch64-pc-windows-msvc\release"
#else
  #define Arch "x64compatible"
  #define Suffix ""
  #define BinDir "..\target\release"
#endif

[Setup]
AppId={{6E0B7C4A-2D1F-4C8E-9B3A-7F5D1E2A9C40}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Danial Gobel
AppPublisherURL=https://github.com/danialgobel/BitChord
AppSupportURL=https://github.com/danialgobel/BitChord/issues
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Default to admin (Program Files), with dialog override allowed for non-admin per-user install
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline dialog
ArchitecturesAllowed={#Arch}
ArchitecturesInstallIn64BitMode={#Arch}
MinVersion=10.0
OutputBaseFilename=BitChord-Setup-{#AppVersion}{#Suffix}
OutputDir=..\release
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\assets\icon\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\WebView2Loader.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\bin\*"; DestDir: "{app}\bin"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; ytm-rs:// links (jam invites). HKA is HKCU for per-user installs, HKLM for all-users.
; The installed app also re-registers itself under HKCU on launch.
Root: HKA; Subkey: "Software\Classes\ytm-rs"; ValueType: string; ValueName: ""; ValueData: "URL:ytm-rs Protocol"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\ytm-rs"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\ytm-rs\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"",0"
Root: HKA; Subkey: "Software\Classes\ytm-rs\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""

[UninstallDelete]
; Installers downloaded by the in-app updater (leaves user configs and library intact)
Type: filesandordirs; Name: "{userappdata}\ytm-rs\data\updates"

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
function IsWebView2Installed: Boolean;
var
  Version: String;
begin
  Result := RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) or
            RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) or
            RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version);
  if Result and (Version = '') then
    Result := False;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  ErrorCode: Integer;
begin
  Result := True;
  if CurPageID = wpReady then
  begin
    if not IsWebView2Installed then
    begin
      if MsgBox('BitChord requires the Microsoft Edge WebView2 Runtime for Google/Spotify sign-in.' + #13#10 +
                'It was not detected on your PC.' + #13#10#13#10 +
                'Would you like to open the official download page to install it?', mbConfirmation, MB_YESNO) = IDYES then
      begin
        ShellExec('open', 'https://go.microsoft.com/fwlink/p/?LinkId=2124703', '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
      end;
    end;
  end;
end;
