[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$definition = [IO.File]::ReadAllText((Join-Path (Split-Path -Parent $PSScriptRoot) 'packaging\windows-x64\installer.iss'), [Text.Encoding]::UTF8)

function Assert-Contains([string]$Token) {
    if (-not $definition.Contains($Token)) { throw "FAIL: installer definition is missing: $Token" }
}

foreach ($token in @(
    'DefaultDirName={autopf}\MultiCore',
    'PrivilegesRequired=admin',
    'PrivilegesRequiredOverridesAllowed=commandline dialog',
    'russian.PrivilegesRequiredOverrideAllUsersRecommended=Со службой (рекомендуется)',
    'russian.PrivilegesRequiredOverrideCurrentUser=Без службы',
    'Один запрос прав администратора сейчас',
    'TUN будет запрашивать права при каждом подключении',
    'Name: "cleandata"; Description: "Удалить старые подписки и настройки"',
    "WizardIsTaskSelected('cleandata')",
    "SafeDelTree(UserLocalAppData + '\MultiCore')",
    "'create ' + ServiceName + ' binPath= ""\""' + ServiceBinaryPath + '\"" --service"" start= auto obj= LocalSystem",
    "'failure ' + ServiceName + ' reset= 86400 actions= restart/5000/restart/5000'",
    "'start ' + ServiceName",
    "'delete ' + ServiceName",
    "ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName",
    "Result := Result + '\Programs\MultiCore';",
    "PerUserInstallDir + '\unins000.exe'",
    "SuppressibleMsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO)",
    'not UninstallSilent',
    "SafeDelTree(ExpandConstant('{commonappdata}\MultiCore'))",
    'IsAdminInstallMode',
    # A per-user 1.0.x install must not silently pin the choice to "Без службы".
    'UsePreviousPrivileges=no',
    # The service holds files in {app}\current; stop it before the files-in-use check.
    'function PrepareToInstall(var NeedsRestart: Boolean): String;',
    # The Settings card runs /ALLUSERS /SILENT /LAUNCH=1 after quitting; relaunch as the user.
    "WizardSilent and (ExpandConstant('{param:LAUNCH|0}') = '1')",
    'ExecAsOriginalUser(',
    'LaunchAfterSilentMigration;',
    # I1: the per-user uninstaller lives in a user-writable folder; it must never run elevated.
    "ExecAsOriginalUser(Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'",
    # I2: service mode installs only to Program Files (no dir page, /DIR= refused).
    "Result := (PageID = wpSelectDir) and IsAdminInstallMode;",
    "Result := ExpandConstant('{commonpf64}\MultiCore');",
    # x86 flavour: 32-bit install mode, so Program Files (x86) on 64-bit Windows.
    "Result := ExpandConstant('{commonpf32}\MultiCore');",
    'if not Is64BitInstallMode then',
    "CompareText(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), ServiceInstallDir) <> 0",
    # I3: a running per-user copy (desktop, daemon, core host, cores) blocks the migration.
    "'SELECT Handle, ExecutablePath, SessionId FROM Win32_Process'",
    'if DirExists(PerUserInstallDir) and not WaitForPerUserCopyToExit then',
    # I1: an elevated Setup/Uninstall may run as another account (a standard user typed an
    # administrator's credentials); per-user state belongs to the user signed in to this session.
    'Root: HKA; Subkey: "Software\Classes\multicore"',
    'Tasks: autostart; Check: not IsAdminInstallMode',
    "CreateOleObject('WbemScripting.SWbemLocator')",
    "ExecMethod_('GetOwnerSid')",
    "'\Volatile Environment', 'LOCALAPPDATA'",
    "RegWriteStringValue(UserHiveRoot, UserHiveKey(RunKey), 'MultiCore', AutostartCommand)",
    'DeleteAutostartValue(UserHiveRoot, UserHiveKey(RunKey));',
    'DeleteUserProtocolHandler;',
    # M6: "Без службы" over a service install removes the service install first (or stops).
    "RegQueryStringValue(HKLM64, UninstallKey, 'UninstallString', Uninstaller)",
    "RegQueryStringValue(HKLM32, UninstallKey, 'UninstallString', Uninstaller)",
    # x86 on 64-bit Windows: warn (x64 preferred) and never replace an x64 install.
    "<event('InitializeSetup')>",
    'Result := (not Is64BitInstallMode) and IsWin64;',
    'if RegKeyExists(HKLM64, UninstallKey) then',
    '(Pos(''"platform": "windows-x64"'', Manifest) > 0)',
    'уже установлен 64-битный MULTICORE',
    # x64 over an x86 install on 64-bit Windows: migrate it, keeping the user's data.
    'function X86ServiceUninstaller: String;',
    "RegQueryStringValue(HKLM32, UninstallKey, 'UninstallString', X86Uninstaller)",
    "ExpandConstant('{commonpf32}\MultiCore\')",
    'function RemoveX86ServiceInstall: String;',
    "Exec(X86Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'",
    'Result := X86ServiceUninstaller;',
    'function X86PerUserCopyPresent: Boolean;',
    '(Pos(''"platform": "windows-x86"'', Manifest) > 0)',
    'function RemoveX86PerUserCopy: String;',
    "while (X86PerUserCopyPresent or FileExists(PerUserInstallDir + '\unins000.exe')) and",
    "if WizardIsTaskSelected('autostart') or MoveAutostartFromServiceInstall then",
    "ShellExec('', ServiceUninstaller, '/SILENT /SUPPRESSMSGBOXES /NORESTART'",
    'MULTICORE уже установлен со службой',
    'Result := RemoveServiceInstall;',
    'MoveAutostartFromServiceInstall',
    # The uninstaller touches the service only for the service install itself.
    'if IsServiceInstall then',
    # Data folders are never deleted through a planted junction or symbolic link.
    'ReparsePointAttribute = $400;',
    '(FindRec.Attributes and ReparsePointAttribute) <> 0',
    # A failed registration is reported, not swallowed.
    'if Registered <> 0 then',
    # The product name is MULTICORE wherever it is seen; the service NAME stays MultiCore.
    '#define AppName "MULTICORE"',
    '#define AppPublisher "MULTICORE"',
    "ServiceName = 'MultiCore';",
    "'create ' + ServiceName + ' binPath= ""\""' + ServiceBinaryPath + '\"" --service"" start= auto obj= LocalSystem DisplayName= ""MULTICORE Service""'",
    # An upgraded service (registered by 1.1.1 as "MultiCore") gets the new display name too.
    "'config ' + ServiceName + ' binPath= ""\""' + ServiceBinaryPath + '\"" --service"" start= auto obj= LocalSystem DisplayName= ""MULTICORE Service""'",
    "' ""MULTICORE: ядра Mihomo и Xray для мгновенного подключения""'",
    # Launch at sign-in keeps its value name, so an upgrade never adds a second entry.
    'ValueName: "MultiCore"; ValueData: """{app}\current\MultiCore.exe"" --background"',
    # NTFS keeps the old casing on overwrite: 1.1.1's MultiCore.lnk shortcuts are deleted first.
    '[InstallDelete]',
    'Type: files; Name: "{autoprograms}\MultiCore.lnk"',
    'Type: files; Name: "{autodesktop}\MultiCore.lnk"; Tasks: desktopicon',
    'Name: "{autodesktop}\MULTICORE"; Filename: "{app}\current\MultiCore.exe"'
)) {
    Assert-Contains $token
}
if ($definition -match 'Name: "cleandata"[^\r\n]*Flags: unchecked') { } else { throw 'FAIL: cleanup checkbox must be off by default' }
foreach ($forbidden in @(
    'RunTool(Uninstaller', 'Exec(Uninstaller', 'restart/5000//',
    # I1: never the elevated account's hive or profile for the URL handler or the user's data.
    'Root: HKCU; Subkey: "Software\Classes\multicore"',
    'Known limit: reinstalling over a service install',
    # Usernames and paths may hold ’ ' " & % ^ ! spaces or Cyrillic: no shell and no script
    # source is ever built from them (PowerShell treats ’ as a quote, cmd expands % and ^).
    'powershell', '-Command', '{cmd}', 'cmd.exe', 'StringChangeEx('
)) {
    if ($definition.IndexOf($forbidden, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "FAIL: installer definition must not contain: $forbidden" }
}
# {localappdata} is the elevated account's folder in service mode: only the fallback may use it.
if (([regex]::Matches($definition, [regex]::Escape('{localappdata}'))).Count -ne 1) {
    throw 'FAIL: {localappdata} may appear only in the interactive-user fallback'
}
# Process arguments are built only from Program Files paths and constants, never from the user's
# profile, name or SID.
foreach ($line in ($definition -split "`r?`n")) {
    if ($line -match '\b(RunTool|RunSc|Exec|ExecAsOriginalUser|ShellExec)\(' -and
        $line -match 'UserLocalAppData|UserSid|PerUserInstallDir|\{username\}|\{localappdata\}') {
        throw "FAIL: a process argument is built from per-user data: $($line.Trim())"
    }
}
# The uninstall entry checked for M6 is this installer's own AppId.
$appId = [regex]::Match($definition, 'AppId=\{\{([0-9A-F-]+)\}').Groups[1].Value
if (-not $appId -or -not $definition.Contains("Uninstall\{$appId}_is1'")) {
    throw 'FAIL: the service-install lookup must use this AppId'
}
if ($definition -match '(?<!Safe)DelTree\(ExpandConstant') { throw 'FAIL: every data deletion must go through SafeDelTree' }
if ($definition -match '(?<!Suppressible)MsgBox\(') { throw 'FAIL: every message box must be suppressible' }
function Get-Body([string]$Header) {
    $start = $definition.IndexOf($Header)
    if ($start -lt 0) { throw "FAIL: missing $Header" }
    $end = $definition.IndexOf("`nend;", $start)
    return $definition.Substring($start, $end - $start)
}
$prepare = Get-Body 'function PrepareToInstall('
if (-not ($prepare.IndexOf('ServiceInstallDir) <> 0') -lt $prepare.IndexOf('WaitForPerUserCopyToExit') -and $prepare.IndexOf('WaitForPerUserCopyToExit') -lt $prepare.IndexOf('StopService;'))) {
    throw 'FAIL: PrepareToInstall must check the directory, then the running per-user copy, then stop the service'
}
$install = Get-Body 'procedure CurStepChanged('
if (-not ($install.IndexOf('RemovePerUserCopy;') -ge 0 -and $install.IndexOf('RemovePerUserCopy;') -lt $install.IndexOf("SafeDelTree(UserLocalAppData + '\MultiCore')"))) {
    throw 'FAIL: the per-user copy must be removed before its data is cleaned'
}
if (-not ($prepare.IndexOf('Result := RemoveServiceInstall;') -ge 0 -and $prepare.IndexOf('Result := RemoveServiceInstall;') -lt $prepare.IndexOf('ServiceInstallDir) <> 0'))) {
    throw 'FAIL: a per-user install must deal with an existing service install before anything else'
}
if (-not ($prepare.IndexOf('if X64InstallPresent then') -ge 0 -and $prepare.IndexOf('if X64InstallPresent then') -lt $prepare.IndexOf('Result := RemoveServiceInstall;'))) {
    throw 'FAIL: the x86 installer must refuse an x64 install before touching any install'
}
if (-not ($prepare.IndexOf('ServiceInstallDir) <> 0') -lt $prepare.IndexOf('Result := RemoveX86ServiceInstall;') -and $prepare.IndexOf('Result := RemoveX86ServiceInstall;') -lt $prepare.IndexOf('WaitForPerUserCopyToExit'))) {
    throw 'FAIL: x64 service mode must migrate an x86 service install after the directory check and before the per-user wait'
}
if (-not ($prepare.IndexOf('Result := RemoveServiceInstall;') -lt $prepare.IndexOf('Result := RemoveX86PerUserCopy;') -and $prepare.IndexOf('Result := RemoveX86PerUserCopy;') -lt $prepare.IndexOf('ServiceInstallDir) <> 0'))) {
    throw 'FAIL: x64 per-user mode must remove the service install, then an x86 per-user copy'
}
foreach ($guard in @(@('function X86ServiceUninstaller:', 'HKLM32'), @('function X86PerUserCopyPresent:', 'LoadStringFromFile'))) {
    $header = $guard[0]
    $body = Get-Body $header
    if (-not ($body.IndexOf('if not Is64BitInstallMode then') -ge 0 -and $body.IndexOf('if not Is64BitInstallMode then') -lt $body.IndexOf($guard[1]))) {
        throw "FAIL: $header must do nothing in the x86 installer"
    }
}
$x86Removal = Get-Body 'function RemoveX86ServiceInstall:'
if (-not ($x86Removal.IndexOf('StopService;') -ge 0 -and $x86Removal.IndexOf('StopService;') -lt $x86Removal.IndexOf('WaitForExit(') -and $x86Removal.IndexOf('WaitForExit(') -lt $x86Removal.IndexOf('Exec(X86Uninstaller'))) {
    throw 'FAIL: the x86 service install is stopped and its processes gone before its uninstaller runs'
}
$x64Check = Get-Body 'function X64InstallPresent:'
if (-not ($x64Check.IndexOf('if not IsX86SetupOn64BitWindows then') -ge 0 -and $x64Check.IndexOf('if not IsX86SetupOn64BitWindows then') -lt $x64Check.IndexOf('RegKeyExists(HKLM64'))) {
    throw 'FAIL: the x64 installer (and 32-bit Windows) must never evaluate the x64-install refusal'
}
$lookup = Get-Body 'procedure LookupInteractiveUser;'
if (-not $lookup.Contains('if not IsAdminInstallMode then')) {
    throw 'FAIL: a per-user Setup runs as the user itself and keeps HKCU and {localappdata}'
}
$sid = Get-Body 'function IsSidString('
if (-not $sid.Contains("'0'..'9', '-'")) { throw 'FAIL: the SID used in a registry path must be validated' }
Write-Output 'PASS: service installer contract'
