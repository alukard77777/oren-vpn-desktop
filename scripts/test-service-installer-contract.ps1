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
    "SafeDelTree(ExpandConstant('{localappdata}\MultiCore'))",
    "'create ' + ServiceName + ' binPath= ""\""' + ServiceBinaryPath + '\"" --service"" start= auto obj= LocalSystem",
    "'failure ' + ServiceName + ' reset= 86400 actions= restart/5000/restart/5000'",
    "'start ' + ServiceName",
    "'delete ' + ServiceName",
    "ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName",
    "Result := ExpandConstant('{localappdata}\Programs\MultiCore');",
    "PerUserInstallDir + '\unins000.exe'",
    "SuppressibleMsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO)",
    'not UninstallSilent',
    "SafeDelTree(ExpandConstant('{commonappdata}\MultiCore'))",
    'IsAdminInstallMode',
    # A per-user 1.0.x install must not silently pin the choice to "Без службы".
    'UsePreviousPrivileges=no',
    # The service holds files in {app}\current; stop it before the files-in-use check.
    'function PrepareToInstall(var NeedsRestart: Boolean): String;',
    # I1: the per-user uninstaller lives in a user-writable folder; it must never run elevated.
    "ExecAsOriginalUser(Uninstaller, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'",
    # I2: service mode installs only to Program Files (no dir page, /DIR= refused).
    "Result := (PageID = wpSelectDir) and IsAdminInstallMode;",
    "Result := ExpandConstant('{commonpf64}\MultiCore');",
    "CompareText(RemoveBackslashUnlessRoot(ExpandConstant('{app}')), ServiceInstallDir) <> 0",
    # I3: a running per-user copy (desktop, daemon, core host, cores) blocks the migration.
    '$_.Path.StartsWith(',
    'if DirExists(PerUserInstallDir) and not WaitForPerUserCopyToExit then',
    # Data folders are never deleted through a planted junction or symbolic link.
    'ReparsePointAttribute = $400;',
    '(FindRec.Attributes and ReparsePointAttribute) <> 0',
    # A failed registration is reported, not swallowed.
    'if Registered <> 0 then'
)) {
    Assert-Contains $token
}
if ($definition -match 'Name: "cleandata"[^\r\n]*Flags: unchecked') { } else { throw 'FAIL: cleanup checkbox must be off by default' }
foreach ($forbidden in @('RunTool(Uninstaller', 'Exec(Uninstaller', 'restart/5000//')) {
    if ($definition.Contains($forbidden)) { throw "FAIL: installer definition must not contain: $forbidden" }
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
if (-not ($install.IndexOf('RemovePerUserCopy;') -ge 0 -and $install.IndexOf('RemovePerUserCopy;') -lt $install.IndexOf("SafeDelTree(ExpandConstant('{localappdata}\MultiCore'))"))) {
    throw 'FAIL: the per-user copy must be removed before its data is cleaned'
}
Write-Output 'PASS: service installer contract'
