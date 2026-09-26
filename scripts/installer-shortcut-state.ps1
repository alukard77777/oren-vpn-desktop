# Disposable installer tests share Explorer's shortcut directories with the user.
# Preserve exact original bytes, but never overwrite an independent user change.
function Save-InstallerShortcutState {
    param([string[]]$Paths)
    foreach ($path in $Paths) {
        $present = Test-Path -LiteralPath $path -PathType Leaf
        [pscustomobject]@{
            Path = $path
            WasPresent = $present
            Bytes = if ($present) { [IO.File]::ReadAllBytes($path) } else { $null }
        }
    }
}

function Restore-InstallerShortcutState {
    param([object[]]$Snapshots, [string[]]$OwnedTargets)
    $shell = New-Object -ComObject WScript.Shell
    foreach ($snapshot in $Snapshots) {
        $present = Test-Path -LiteralPath $snapshot.Path -PathType Leaf
        if ($present) {
            $currentTarget = $shell.CreateShortcut($snapshot.Path).TargetPath
            if ($currentTarget -notin $OwnedTargets) { continue }
        }
        if ($snapshot.WasPresent) {
            [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($snapshot.Path))
            [IO.File]::WriteAllBytes($snapshot.Path, [byte[]]$snapshot.Bytes)
        } elseif ($present) {
            Remove-Item -LiteralPath $snapshot.Path -Force
        }
    }
}
