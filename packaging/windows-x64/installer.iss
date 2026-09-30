#ifndef PayloadDir
  #error PayloadDir must point at a verified portable package
#endif
#ifndef OutputDir
  #error OutputDir must be provided by build-windows-installer.ps1
#endif
#ifndef AppVersion
  #error AppVersion must be provided by build-windows-installer.ps1
#endif

#define AppName "MultiCore"
#define AppPublisher "MultiCore"
#define AppExeName "MultiCore.exe"

[Setup]
AppId={{D95AB954-248C-44C9-9E3A-952398802A2D}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
; Service mode (default): Program Files + the MultiCore service. "Без службы": per-user.
DefaultDirName={autopf}\MultiCore
DefaultGroupName=MultiCore
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline dialog
; Always ask: an existing per-user 1.0.x install must not silently pin the choice to "Без службы".
; Known limit: reinstalling over a service install and choosing "Без службы" leaves two installs.
UsePreviousPrivileges=no
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=MultiCore-Setup-x64
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
CloseApplicationsFilter=MultiCore.exe,multicore-daemon.exe,multicore-updater.exe,multicore-core-host.exe,mihomo.exe,xray.exe
RestartApplications=no
UninstallDisplayIcon={app}\current\MultiCore.exe
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
russian.PrivilegesRequiredOverrideTitle=Как установить MultiCore
russian.PrivilegesRequiredOverrideInstruction=Выберите способ установки
russian.PrivilegesRequiredOverrideText1=Со службой MultiCore подключается мгновенно. Один запрос прав администратора сейчас и ни одного потом. Без службы установка не требует прав: системный прокси работает как раньше, а режим TUN будет запрашивать права при каждом подключении.
russian.PrivilegesRequiredOverrideText2=Со службой MultiCore подключается мгновенно. Один запрос прав администратора сейчас и ни одного потом. Без службы установка не требует прав: системный прокси работает как раньше, а режим TUN будет запрашивать права при каждом подключении.
russian.PrivilegesRequiredOverrideAllUsers=Со службой (рекомендуется)
russian.PrivilegesRequiredOverrideAllUsersRecommended=Со службой (рекомендуется)
russian.PrivilegesRequiredOverrideCurrentUser=Без службы
russian.PrivilegesRequiredOverrideCurrentUserRecommended=Без службы
english.PrivilegesRequiredOverrideTitle=How to install MultiCore
english.PrivilegesRequiredOverrideInstruction=Choose how to install
english.PrivilegesRequiredOverrideText1=With the service MultiCore connects instantly: one administrator prompt now and none later. Without the service no administrator rights are needed: the system proxy works as before, and TUN mode asks for rights on every connect.
english.PrivilegesRequiredOverrideText2=With the service MultiCore connects instantly: one administrator prompt now and none later. Without the service no administrator rights are needed: the system proxy works as before, and TUN mode asks for rights on every connect.
english.PrivilegesRequiredOverrideAllUsers=With the service (recommended)
english.PrivilegesRequiredOverrideAllUsersRecommended=With the service (recommended)
english.PrivilegesRequiredOverrideCurrentUser=Without the service
english.PrivilegesRequiredOverrideCurrentUserRecommended=Without the service

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; GroupDescription: "Ярлыки:"; Flags: unchecked
Name: "autostart"; Description: "Запускать MultiCore вместе с Windows"; GroupDescription: "Автозапуск:"; Flags: unchecked
Name: "cleandata"; Description: "Удалить старые подписки и настройки"; GroupDescription: "Данные:"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}\current"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\MultiCore"; Filename: "{app}\current\MultiCore.exe"; WorkingDir: "{app}\current"
Name: "{autodesktop}\MultiCore"; Filename: "{app}\current\MultiCore.exe"; WorkingDir: "{app}\current"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "MultiCore"; ValueData: """{app}\current\MultiCore.exe"" --background"; Tasks: autostart
Root: HKCU; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: ""; ValueData: "URL:MultiCore Protocol"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\multicore\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\current\MultiCore.exe,0"
Root: HKCU; Subkey: "Software\Classes\multicore\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\current\MultiCore.exe"" ""%1"""

[Run]
Filename: "{app}\current\MultiCore.exe"; Description: "Запустить MultiCore"; WorkingDir: "{app}\current"; Flags: postinstall shellexec skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{app}\.multicore-previous-*"
Type: filesandordirs; Name: "{app}\.multicore-failed-*"
Type: filesandordirs; Name: "{app}\.multicore-stage-*"

[Code]
const
  ServiceName = 'MultiCore';
  { FILE_ATTRIBUTE_REPARSE_POINT }
  ReparsePointAttribute = $400;
  PerUserProcessWaitSeconds = 30;

function RunTool(const Tool, Params: String): Integer;
var
  ResultCode: Integer;
begin
  if not Exec(Tool, Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    ResultCode := -1;
  Result := ResultCode;
end;

function RunSc(const Params: String): Integer;
begin
  Result := RunTool(ExpandConstant('{sys}\sc.exe'), Params);
  if Result <> 0 then
    Log('sc ' + Params + ' exited with ' + IntToStr(Result));
end;

function ServiceBinaryPath: String;
begin
  Result := ExpandConstant('{app}\current\runtime\multicore-core-host.exe');
end;

{ Service mode installs only here: the SYSTEM service must never run from a user-writable folder. }
function ServiceInstallDir: String;
begin
  Result := ExpandConstant('{commonpf64}\MultiCore');
end;

function PerUserInstallDir: String;
begin
  Result := ExpandConstant('{localappdata}\Programs\MultiCore');
end;

{ Deletes a data folder, but never through a junction or symbolic link planted at its path. }
procedure SafeDelTree(const Path: String);
var
  FindRec: TFindRec;
  IsReparsePoint: Boolean;
begin
  if not FindFirst(Path, FindRec) then
    exit;
  IsReparsePoint := (FindRec.Attributes and ReparsePointAttribute) <> 0;
  FindClose(FindRec);
  if IsReparsePoint then
    Log('Refusing to delete a reparse point: ' + Path)
  else
    DelTree(Path, True, True, True);
end;

procedure StopService;
begin
  { net stop waits until the service has stopped; "not started" is fine. }
  RunTool(ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName);
end;

procedure RegisterService;
var
  Registered: Integer;
begin
  if RunSc('query ' + ServiceName) = 0 then
    Registered := RunSc('config ' + ServiceName + ' binPath= "\"' + ServiceBinaryPath + '\" --service" start= auto obj= LocalSystem')
  else
    Registered := RunSc('create ' + ServiceName + ' binPath= "\"' + ServiceBinaryPath + '\" --service" start= auto obj= LocalSystem DisplayName= "MultiCore"');
  if Registered <> 0 then
  begin
    { 1072: the old service is still marked for deletion until a reboot. }
    SuppressibleMsgBox('Не удалось зарегистрировать службу MultiCore (код ' + IntToStr(Registered) + '). '
      + 'Приложение будет работать без службы и запрашивать права для TUN. '
      + 'Перезагрузите компьютер и запустите установку снова.', mbError, MB_OK, IDOK);
    exit;
  end;
  RunSc('description ' + ServiceName + ' "MultiCore: ядра Mihomo и Xray для мгновенного подключения"');
  RunSc('failure ' + ServiceName + ' reset= 86400 actions= restart/5000/restart/5000');
  RunSc('start ' + ServiceName);
end;

{ True while any process runs from the per-user install (desktop, daemon, core host, cores). }
function PerUserCopyRunning: Boolean;
var
  Prefix: String;
begin
  Prefix := AddBackslash(PerUserInstallDir);
  StringChangeEx(Prefix, '''', '''''', True);
  { Runs elevated on purpose: the per-user copy may own an elevated core host. }
  Result := RunTool(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "'
    + 'if (Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith(''' + Prefix + ''', [StringComparison]::OrdinalIgnoreCase) }) { exit 1 } else { exit 0 }"') = 1;
end;

function WaitForPerUserCopyToExit: Boolean;
var
  Waited: Integer;
begin
  Waited := 0;
  Result := not PerUserCopyRunning;
  while (not Result) and (Waited < PerUserProcessWaitSeconds) do
  begin
    { The Settings card starts this installer and then quits the app; give it time. }
    Sleep(1000);
    Waited := Waited + 1;
    Result := not PerUserCopyRunning;
  end;
end;

procedure RemovePerUserCopy;
var
  Uninstaller: String;
  ResultCode: Integer;
begin
  { Migration from a per-user install: its data in %LOCALAPPDATA%\MultiCore stays.
    The uninstaller lives in a user-writable folder, so it never runs elevated. }
  Uninstaller := PerUserInstallDir + '\unins000.exe';
  if not FileExists(Uninstaller) then
    exit;
  if not ExecAsOriginalUser(Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    ResultCode := -1;
  if ResultCode <> 0 then
    Log('Per-user MultiCore uninstaller exited with ' + IntToStr(ResultCode));
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  { Service mode has exactly one allowed location. }
  Result := (PageID = wpSelectDir) and IsAdminInstallMode;
end;

procedure LaunchAfterSilentMigration;
var
  ResultCode: Integer;
begin
  { The Settings card runs Setup with /SILENT /LAUNCH=1 after quitting MultiCore; the
    postinstall [Run] entry is skipped when silent, so start the app once the service runs. }
  if WizardSilent and (ExpandConstant('{param:LAUNCH|0}') = '1') then
    ExecAsOriginalUser(ExpandConstant('{app}\current\{#AppExeName}'), '',
      ExpandConstant('{app}\current'), SW_SHOWNORMAL, ewNoWait, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not IsAdminInstallMode then
    exit;
  if CompareText(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), ServiceInstallDir) <> 0 then
  begin
    Result := 'Со службой MultiCore устанавливается только в ' + ServiceInstallDir + '.';
    exit;
  end;
  if DirExists(PerUserInstallDir) and not WaitForPerUserCopyToExit then
  begin
    Result := 'MultiCore ещё запущен. Закройте его (значок в трее → Выход) и запустите установку снова.';
    exit;
  end;
  { The running service holds files in the current folder; stop it before Setup checks for files in use. }
  StopService;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    if IsAdminInstallMode then
    begin
      StopService;
      RemovePerUserCopy;
    end;
    if WizardIsTaskSelected('cleandata') then
      SafeDelTree(ExpandConstant('{localappdata}\MultiCore'));
  end;
  if (CurStep = ssPostInstall) and IsAdminInstallMode then
    RegisterService;
  if CurStep = ssPostInstall then
    LaunchAfterSilentMigration;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AutostartValue: String;
  ExpectedAutostartValue: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    ExpectedAutostartValue := '"' + ExpandConstant('{app}\current\MultiCore.exe') + '" --background';
    if RegQueryStringValue(
         HKCU,
         'Software\Microsoft\Windows\CurrentVersion\Run',
         'MultiCore',
         AutostartValue) and (CompareText(AutostartValue, ExpectedAutostartValue) = 0) then
      RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'MultiCore');
    if IsAdminInstallMode then
    begin
      RunTool(ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName);
      RunSc('delete ' + ServiceName);
    end;
    if (not UninstallSilent) and
       (SuppressibleMsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES) then
      SafeDelTree(ExpandConstant('{localappdata}\MultiCore'));
  end;
  if (CurUninstallStep = usPostUninstall) and IsAdminInstallMode then
    SafeDelTree(ExpandConstant('{commonappdata}\MultiCore'));
end;
