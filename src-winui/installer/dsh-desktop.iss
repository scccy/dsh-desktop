; Inno Setup script for the Windows native shell (src-winui).
;
; Build with: pwsh -File scripts\build-installer.ps1
; or directly:  ISCC.exe /DAppVersion=0.1.0 src-winui\installer\dsh-desktop.iss
;
; Notes that matter:
; - The app is self-contained and unpackaged, so this installer is the only
;   "installation" it needs: copy the publish output, add shortcuts, write an
;   uninstall entry. No Windows App Runtime, no .NET runtime, no MSIX.
; - PrivilegesRequired=lowest: an unsigned setup must not ask for elevation.
;   Users who want an all-users install can still pick it in the dialog.
; - User data (%LOCALAPPDATA%\dsh desktop: webview2 profile, logs, config) is
;   deliberately left behind on uninstall.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif


#define AppName "dsh desktop"
#define AppPublisher "dsh desktop"
#define AppExe "dsh-desktop-winui.exe"
; Stable identity for upgrades: never change this GUID.
#define AppId "{{6F3B1C2A-9D4E-4B7C-8A15-2C7E5F0B91D3}"
#define PublishDir "..\out-pub"
#define RepoRoot "..\.."

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\dsh desktop
DefaultGroupName={#AppName}
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\app\{#AppExe}
OutputDir={#RepoRoot}\dist
InfoAfterFile={#RepoRoot}\src-winui\installer\after-install.txt
OutputBaseFilename=dsh-desktop-winui-setup-{#AppVersion}
SetupIconFile={#RepoRoot}\src-tauri\icons\icon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; WinUI 3 needs Windows 10 1809 or newer.
MinVersion=10.0.17763
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DisableProgramGroupPage=yes
DisableWelcomePage=no
AllowNoIcons=yes
CloseApplications=no
RestartApplications=no

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Everything the app needs lives under {app}\app so the install folder root stays
; readable: uninstaller, shortcuts target, this note. The publish output is a flat
; pile of ~280 files, so it is never unpacked next to the uninstaller.
Source: "{#PublishDir}\*"; DestDir: "{app}\app"; Excludes: "*.pdb,*.exe.WebView2\*"; Flags: ignoreversion recursesubdirs createallsubdirs
; The same note the wizard shows, left on disk so the folder explains itself:
; where the app is, where the data lives, and how to uninstall.
Source: "{#RepoRoot}\src-winui\installer\after-install.txt"; DestDir: "{app}"; DestName: "安装说明.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\app\{#AppExe}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\app\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\app\{#AppExe}"; WorkingDir: "{app}\app"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only files this installer created are removed. The per-user data directory is
; left alone: it holds the WebView2 profile, rolling logs and config.toml.
Type: filesandordirs; Name: "{app}\app\dsh-desktop-winui.exe.WebView2"
