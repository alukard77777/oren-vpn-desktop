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
    "DelTree(ExpandConstant('{localappdata}\MultiCore'), True, True, True)",
    "'create ' + ServiceName + ' binPath= ""\""' + ServiceBinaryPath + '\"" --service"" start= auto obj= LocalSystem",
    "'failure ' + ServiceName",
    "'start ' + ServiceName",
    "'delete ' + ServiceName",
    "ExpandConstant('{sys}\net.exe'), 'stop ' + ServiceName",
    "{localappdata}\Programs\MultiCore\unins000.exe",
    "MsgBox('Удалить подписки и настройки?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2)",
    'not UninstallSilent',
    "DelTree(ExpandConstant('{commonappdata}\MultiCore'), True, True, True)",
    'IsAdminInstallMode',
    # A per-user 1.0.x install must not silently pin the choice to "Без службы".
    'UsePreviousPrivileges=no',
    # The service holds files in {app}\current; stop it before the files-in-use check.
    'function PrepareToInstall(var NeedsRestart: Boolean): String;'
)) {
    Assert-Contains $token
}
if ($definition -match 'Name: "cleandata"[^\r\n]*Flags: unchecked') { } else { throw 'FAIL: cleanup checkbox must be off by default' }
Write-Output 'PASS: service installer contract'
