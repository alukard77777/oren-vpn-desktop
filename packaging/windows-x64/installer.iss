#ifndef PayloadDir
  #error PayloadDir must point at a verified portable package
#endif
#ifndef OutputDir
  #error OutputDir must be provided by build-windows-installer.ps1
#endif
#ifndef AppVersion
  #error AppVersion must be provided by build-windows-installer.ps1
#endif
; x64 (default) or x86: the 32-bit flavour for 32-bit Windows, installed in 32-bit mode.
#ifndef Arch
  #define Arch "x64"
#endif
#if Arch != "x64" && Arch != "x86"
  #error Arch must be x64 or x86
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
; "Без службы" over a service install removes the service install first (PrepareToInstall).
UsePreviousPrivileges=no
UsedUserAreasWarning=no
#if Arch == "x64"
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=MultiCore-Setup-x64
#else
; Same AppId as x64. Always 32-bit install mode: {commonpf} is Program Files (x86) on 64-bit
; Windows, Program Files on 32-bit Windows; the uninstall entry lives in the 32-bit view.
ArchitecturesAllowed=x86compatible
OutputBaseFilename=MultiCore-Setup-x86
#endif
OutputDir={#OutputDir}
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
; Service mode: an elevated Setup may run as another account (a standard user typing an
; administrator's credentials), so its HKCU is not the user's. The user's launch at sign-in is
; written by [Code] into the signed-in user's hive; the URL handler is machine-wide (HKA = HKLM)
; and the app registers it again for each user that starts it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "MultiCore"; ValueData: """{app}\current\MultiCore.exe"" --background"; Tasks: autostart; Check: not IsAdminInstallMode
Root: HKA; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: ""; ValueData: "URL:MultiCore Protocol"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\multicore\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\current\MultiCore.exe,0"
Root: HKA; Subkey: "Software\Classes\multicore\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\current\MultiCore.exe"" ""%1"""

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
  ServiceUninstallWaitSeconds = 60;
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  { This installer's own uninstall entry; in HKLM only for a service-mode (all users) install. }
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{D95AB954-248C-44C9-9E3A-952398802A2D}_is1';

var
  UserLookupDone: Boolean;
  { The user signed in to this session when Setup/Uninstall runs elevated; '' means HKCU. }
  UserSid: String;
  { That user's %LOCALAPPDATA%; '' when it could not be found. }
  UserLocalAppDataPath: String;
  { "Без службы" replaced a service install whose launch at sign-in was on. }
  MoveAutostartFromServiceInstall: Boolean;

function GetCurrentProcessId: Cardinal;
  external 'GetCurrentProcessId@kernel32.dll stdcall';
function ProcessIdToSessionId(ProcessId: Cardinal; var SessionId: Cardinal): Boolean;
  external 'ProcessIdToSessionId@kernel32.dll stdcall';

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

{ Service mode installs only here: the SYSTEM service must never run from a user-writable folder.
  The x86 installer always runs in 32-bit install mode (Program Files (x86) on 64-bit Windows),
  the x64 one always in 64-bit mode. }
function ServiceInstallDir: String;
begin
  if not Is64BitInstallMode then
  begin
    Result := ExpandConstant('{commonpf32}\MultiCore');
    exit;
  end;
  Result := ExpandConstant('{commonpf64}\MultiCore');
end;

function AutostartCommand: String;
begin
  Result := '"' + ExpandConstant('{app}\current\{#AppExeName}') + '" --background';
end;

{ A SID goes into a registry path, so accept only "S-1-" followed by digits and dashes. }
function IsSidString(const Value: String): Boolean;
var
  I: Integer;
begin
  Result := (Length(Value) > 4) and (Copy(Value, 1, 4) = 'S-1-');
  if not Result then
    exit;
  for I := 5 to Length(Value) do
    case Value[I] of
      '0'..'9', '-': ;
    else
      begin
        Result := False;
        exit;
      end;
    end;
end;

function CurrentSessionId: Cardinal;
var
  Session: Cardinal;
begin
  if ProcessIdToSessionId(GetCurrentProcessId, Session) then
    Result := Session
  else
    Result := $FFFFFFFF;
end;

function WmiService: Variant;
var
  Locator: Variant;
begin
  Locator := CreateOleObject('WbemScripting.SWbemLocator');
  Result := Locator.ConnectServer('.', 'root\CIMV2');
end;

{ The owner of explorer.exe in this session: the user who signed in here. Only numbers go into
  the query, and the answer is checked before it is used in a registry path. }
function InteractiveUserSid: String;
var
  Session: Cardinal;
  Shells, Owner: Variant;
  Sid: String;
  I: Integer;
begin
  Result := '';
  Session := CurrentSessionId;
  if Session = $FFFFFFFF then
    exit;
  try
    Shells := WmiService.ExecQuery('SELECT Handle FROM Win32_Process WHERE Name = ''explorer.exe'' AND SessionId = ' + IntToStr(Session));
    for I := 0 to Shells.Count - 1 do
    begin
      Owner := Shells.ItemIndex(I).ExecMethod_('GetOwnerSid');
      if (Owner.ReturnValue = 0) and not VarIsNull(Owner.Sid) then
      begin
        Sid := Owner.Sid;
        if IsSidString(Sid) then
        begin
          Result := Sid;
          exit;
        end;
      end;
    end;
  except
    Log('Signed-in user lookup failed: ' + GetExceptionMessage);
  end;
end;

{ An elevated Setup or Uninstall runs as whoever typed the administrator credentials, so its
  HKCU and %LOCALAPPDATA% may belong to another account than the user who signed in. }
procedure LookupInteractiveUser;
var
  Sid, Path: String;
begin
  if UserLookupDone then
    exit;
  UserLookupDone := True;
  UserSid := '';
  UserLocalAppDataPath := ExpandConstant('{localappdata}');
  { A per-user Setup or Uninstall runs as the user itself. }
  if not IsAdminInstallMode then
    exit;
  Sid := InteractiveUserSid;
  if Sid = '' then
  begin
    Log('Signed-in user unknown; per-user state goes to the account running Setup');
    exit;
  end;
  UserSid := Sid;
  if not RegQueryStringValue(HKEY_USERS, Sid + '\Volatile Environment', 'LOCALAPPDATA', Path) then
    if not RegQueryStringValue(HKEY_USERS, Sid + '\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders', 'Local AppData', Path) then
      Path := '';
  if (Path <> '') and DirExists(Path) then
    UserLocalAppDataPath := Path
  else
  begin
    UserLocalAppDataPath := '';
    Log('Local AppData of the signed-in user not found; their files are left alone');
  end;
  Log('Per-user state belongs to ' + Sid);
end;

function UserLocalAppData: String;
begin
  LookupInteractiveUser;
  Result := UserLocalAppDataPath;
end;

function UserHiveRoot: Integer;
begin
  LookupInteractiveUser;
  if UserSid <> '' then
    Result := HKEY_USERS
  else
    Result := HKEY_CURRENT_USER;
end;

function UserHiveKey(const Subkey: String): String;
begin
  LookupInteractiveUser;
  if UserSid <> '' then
    Result := UserSid + '\' + Subkey
  else
    Result := Subkey;
end;

function PerUserInstallDir: String;
begin
  Result := UserLocalAppData;
  if Result <> '' then
    Result := Result + '\Programs\MultiCore';
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

{ Removes launch at sign-in only when it starts this install. }
procedure DeleteAutostartValue(const Root: Integer; const Key: String);
var
  AutostartValue: String;
  ExpectedAutostartValue: String;
begin
  ExpectedAutostartValue := AutostartCommand;
  if RegQueryStringValue(Root, Key, 'MultiCore', AutostartValue) and
     (CompareText(AutostartValue, ExpectedAutostartValue) = 0) then
    RegDeleteValue(Root, Key, 'MultiCore');
end;

procedure RegisterAutostartForUser;
begin
  if not RegWriteStringValue(UserHiveRoot, UserHiveKey(RunKey), 'MultiCore', AutostartCommand) then
    Log('Could not turn on launch at sign-in for the signed-in user');
end;

{ The app registers multicore:// for every user who starts it (HKCU); remove the signed-in
  user's registration when it opens this install. }
procedure DeleteUserProtocolHandler;
var
  Root: Integer;
  Key, Command: String;
begin
  LookupInteractiveUser;
  if UserSid <> '' then
  begin
    Root := HKEY_USERS;
    Key := UserSid + '_Classes\multicore';
  end
  else
  begin
    Root := HKEY_CURRENT_USER;
    Key := 'Software\Classes\multicore';
  end;
  if RegQueryStringValue(Root, Key + '\shell\open\command', '', Command) and
     (CompareText(Command, '"' + ExpandConstant('{app}\current\{#AppExeName}') + '" "%1"') = 0) then
    RegDeleteKeyIncludingSubkeys(Root, Key);
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

{ True while a process runs from Dir. SessionOnly skips other sessions (a service runs in
  session 0). WMI compares the paths here, so no shell or script ever sees a user's path. }
function ProcessRunningUnder(const Dir: String; const SessionOnly: Boolean): Boolean;
var
  Prefix, Path: String;
  Session: Cardinal;
  Processes, Process: Variant;
  I: Integer;
begin
  Result := False;
  if Dir = '' then
    exit;
  Prefix := AnsiLowercase(AddBackslash(Dir));
  Session := CurrentSessionId;
  try
    Processes := WmiService.ExecQuery('SELECT Handle, ExecutablePath, SessionId FROM Win32_Process');
    for I := 0 to Processes.Count - 1 do
    begin
      Process := Processes.ItemIndex(I);
      if (not VarIsNull(Process.ExecutablePath)) and ((not SessionOnly) or (Process.SessionId = Session)) then
      begin
        Path := Process.ExecutablePath;
        if Copy(AnsiLowercase(Path), 1, Length(Prefix)) = Prefix then
        begin
          Log('Still running: ' + Path);
          Result := True;
          exit;
        end;
      end;
    end;
  except
    Log('Running process check failed: ' + GetExceptionMessage);
  end;
end;

function WaitForExit(const Dir: String; const SessionOnly: Boolean): Boolean;
var
  Waited: Integer;
begin
  Waited := 0;
  Result := not ProcessRunningUnder(Dir, SessionOnly);
  while (not Result) and (Waited < PerUserProcessWaitSeconds) do
  begin
    { The Settings card starts this installer and then quits the app; give it time. }
    Sleep(1000);
    Waited := Waited + 1;
    Result := not ProcessRunningUnder(Dir, SessionOnly);
  end;
end;

{ Desktop, daemon, core host and cores of the signed-in user's per-user install. Runs elevated
  on purpose: the per-user copy may own an elevated core host. }
function WaitForPerUserCopyToExit: Boolean;
begin
  Result := WaitForExit(PerUserInstallDir, False);
end;

procedure RemovePerUserCopy;
var
  Uninstaller: String;
  ResultCode: Integer;
begin
  { Migration from a per-user install: its data in %LOCALAPPDATA%\MultiCore stays.
    The uninstaller lives in a user-writable folder, so it never runs elevated. }
  if PerUserInstallDir = '' then
    exit;
  Uninstaller := PerUserInstallDir + '\unins000.exe';
  if not FileExists(Uninstaller) then
    exit;
  if not ExecAsOriginalUser(Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    ResultCode := -1;
  if ResultCode <> 0 then
    Log('Per-user MultiCore uninstaller exited with ' + IntToStr(ResultCode));
end;

{ The uninstaller of a service-mode (all users) install, or '' when there is none. }
function ServiceInstallUninstaller: String;
var
  Uninstaller: String;
  Found: Boolean;
begin
  Result := '';
  { Each flavour's uninstall entry lives in the registry view of its install mode. }
  if Is64BitInstallMode then
    Found := RegQueryStringValue(HKLM64, UninstallKey, 'UninstallString', Uninstaller)
  else
    Found := RegQueryStringValue(HKLM32, UninstallKey, 'UninstallString', Uninstaller);
  if Found then
  begin
    Uninstaller := RemoveQuotes(Uninstaller);
    if FileExists(Uninstaller) then
      Result := Uninstaller;
  end;
end;

{ "Без службы" over a service install would leave two installs. Remove the service install
  first: its uninstaller sits in Program Files (administrators only) and asks for elevation
  itself; /SILENT skips its "delete subscriptions?" question, so the user's data stays. }
function RemoveServiceInstall: String;
var
  ServiceUninstaller, ServiceDir, AutostartValue: String;
  ErrorCode, Waited: Integer;
begin
  Result := '';
  ServiceUninstaller := ServiceInstallUninstaller;
  if ServiceUninstaller = '' then
    exit;
  if SuppressibleMsgBox('MultiCore уже установлен со службой. Чтобы установить его без службы, '
       + 'версию со службой нужно удалить (потребуются права администратора). Подписки и настройки сохранятся.'
       + #13#10#13#10 + 'Удалить MultiCore со службой и продолжить?',
       mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) <> IDYES then
  begin
    Result := 'MultiCore уже установлен со службой. Запустите установку снова и выберите «Со службой» '
      + 'или сначала удалите MultiCore в «Параметры → Приложения».';
    exit;
  end;
  ServiceDir := RemoveBackslash(ExtractFilePath(ServiceUninstaller));
  if not WaitForExit(ServiceDir, True) then
  begin
    Result := 'MultiCore ещё запущен. Закройте его (значок в трее → Выход) и запустите установку снова.';
    exit;
  end;
  { Keep launch at sign-in: the service install's value is removed with it. }
  MoveAutostartFromServiceInstall :=
    RegQueryStringValue(HKEY_CURRENT_USER, RunKey, 'MultiCore', AutostartValue) and
    (CompareText(AutostartValue, '"' + ServiceDir + '\current\{#AppExeName}" --background') = 0);
  if ShellExec('', ServiceUninstaller, '/SILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_SHOWNORMAL, ewWaitUntilTerminated, ErrorCode) then
  begin
    Waited := 0;
    while (ServiceInstallUninstaller <> '') and (Waited < ServiceUninstallWaitSeconds) do
    begin
      Sleep(1000);
      Waited := Waited + 1;
    end;
  end
  else
    Log('Service-mode uninstaller did not run: ' + SysErrorMessage(ErrorCode));
  if ServiceInstallUninstaller <> '' then
  begin
    MoveAutostartFromServiceInstall := False;
    Result := 'Не удалось удалить MultiCore со службой. Удалите его в «Параметры → Приложения» и запустите установку снова.';
  end;
end;

{ The 32-bit installer (32-bit install mode) on 64-bit Windows: the x64 installer fits better. }
function IsX86SetupOn64BitWindows: Boolean;
begin
  Result := (not Is64BitInstallMode) and IsWin64;
end;

{ An x64 install found by the x86 installer: its service install (64-bit uninstall entry) or a
  per-user copy whose package manifest is the x64 one. }
function X64InstallPresent: Boolean;
var
  Manifest: AnsiString;
begin
  Result := False;
  if not IsX86SetupOn64BitWindows then
    exit;
  if RegKeyExists(HKLM64, UninstallKey) then
  begin
    Result := True;
    exit;
  end;
  if (PerUserInstallDir <> '') and
     LoadStringFromFile(PerUserInstallDir + '\current\versions.json', Manifest) and
     (Pos('"platform": "windows-x64"', Manifest) > 0) then
    Result := True;
end;

<event('InitializeSetup')>
function WarnX86SetupOn64BitWindows: Boolean;
begin
  Result := True;
  if not IsX86SetupOn64BitWindows then
    exit;
  { Silent installs (the app's own service migration) keep the 32-bit flavour. }
  Result := SuppressibleMsgBox('Это 32-битная версия MultiCore. На 64-битной Windows лучше установить '
      + 'MultiCore-Setup-x64.exe: 32-битная версия работает, но медленнее и может дольше поднимать TUN.'
      + #13#10#13#10 + 'Всё равно установить 32-битную версию?',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDYES) = IDYES;
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
  { Never replace an x64 install with the 32-bit flavour. }
  if X64InstallPresent then
  begin
    Result := 'На этом компьютере уже установлен 64-битный MultiCore. Установите MultiCore-Setup-x64.exe '
      + 'или сначала удалите MultiCore в «Параметры → Приложения».';
    exit;
  end;
  if not IsAdminInstallMode then
  begin
    Result := RemoveServiceInstall;
    exit;
  end;
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
    if WizardIsTaskSelected('cleandata') and (UserLocalAppData <> '') then
      SafeDelTree(UserLocalAppData + '\MultiCore');
  end;
  if CurStep = ssPostInstall then
  begin
    if IsAdminInstallMode then
    begin
      if WizardIsTaskSelected('autostart') then
        RegisterAutostartForUser;
      RegisterService;
    end
    else if MoveAutostartFromServiceInstall then
      RegWriteStringValue(HKEY_CURRENT_USER, RunKey, 'MultiCore', AutostartCommand);
    LaunchAfterSilentMigration;
  end;
end;

{ Only the service install owns the service; an elevated uninstall of a per-user copy must not
  touch it. }
function IsServiceInstall: Boolean;
begin
  Result := IsAdminInstallMode and
    (CompareText(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), ServiceInstallDir) = 0);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    DeleteAutostartValue(HKCU, RunKey);
    if IsServiceInstall then
    begin
      { The user signed in here, not the account that elevated this uninstaller. }
      DeleteAutostartValue(UserHiveRoot, UserHiveKey(RunKey));
      DeleteUserProtocolHandler;
      RunTool(ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName);
      RunSc('delete ' + ServiceName);
    end;
    if (not UninstallSilent) and
       (SuppressibleMsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES) and
       (UserLocalAppData <> '') then
      SafeDelTree(UserLocalAppData + '\MultiCore');
  end;
  if (CurUninstallStep = usPostUninstall) and IsServiceInstall then
    SafeDelTree(ExpandConstant('{commonappdata}\MultiCore'));
end;
