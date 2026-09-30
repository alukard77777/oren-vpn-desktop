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
end;

function ServiceBinaryPath: String;
begin
  Result := ExpandConstant('{app}\current\runtime\multicore-core-host.exe');
end;

procedure StopService;
begin
  { net stop waits until the service has stopped; "not started" is fine. }
  RunTool(ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName);
end;

procedure RegisterService;
begin
  if RunSc('query ' + ServiceName) = 0 then
    RunSc('config ' + ServiceName + ' binPath= "\"' + ServiceBinaryPath + '\" --service" start= auto obj= LocalSystem')
  else
    RunSc('create ' + ServiceName + ' binPath= "\"' + ServiceBinaryPath + '\" --service" start= auto obj= LocalSystem DisplayName= "MultiCore"');
  RunSc('description ' + ServiceName + ' "MultiCore: ядра Mihomo и Xray для мгновенного подключения"');
  RunSc('failure ' + ServiceName + ' reset= 86400 actions= restart/5000/restart/5000//');
  RunSc('start ' + ServiceName);
end;

procedure RemovePerUserCopy;
var
  Uninstaller: String;
begin
  { Migration from a per-user install: its data in %LOCALAPPDATA%\MultiCore stays. }
  Uninstaller := ExpandConstant('{localappdata}\Programs\MultiCore\unins000.exe');
  if FileExists(Uninstaller) then
    RunTool(Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART');
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
  { The running service holds files in the current folder; stop it before Setup checks for files in use. }
  if IsAdminInstallMode then
    StopService;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    if WizardIsTaskSelected('cleandata') then
      DelTree(ExpandConstant('{localappdata}\MultiCore'), True, True, True);
    if IsAdminInstallMode then
    begin
      StopService;
      RemovePerUserCopy;
    end;
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
       (MsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(ExpandConstant('{localappdata}\MultiCore'), True, True, True);
  end;
  if (CurUninstallStep = usPostUninstall) and IsAdminInstallMode then
    DelTree(ExpandConstant('{commonappdata}\MultiCore'), True, True, True);
end;
