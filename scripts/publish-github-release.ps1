[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Repository,

    [Parameter(Mandatory = $true)]
    [string]$Tag,

    [Parameter(Mandatory = $true)]
    [string]$Target,

    [Parameter(Mandatory = $true)]
    [string]$Title,

    [Parameter(Mandatory = $true)]
    [string]$NotesPath,

    [Parameter(Mandatory = $true)]
    [string]$AssetManifest,

    [string]$Token = $env:GH_TOKEN
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($Token)) {
    throw 'A GitHub release token is required'
}
if (-not (Test-Path -LiteralPath $NotesPath -PathType Leaf)) {
    throw "Release notes do not exist: $NotesPath"
}
if (-not (Test-Path -LiteralPath $AssetManifest -PathType Leaf)) {
    throw "Release asset manifest does not exist: $AssetManifest"
}
$AssetPath = @(Get-Content -LiteralPath $AssetManifest | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if ($AssetPath.Count -eq 0) {
    throw 'Release asset manifest is empty'
}
foreach ($asset in $AssetPath) {
    if (-not (Test-Path -LiteralPath $asset -PathType Leaf)) {
        throw "Release asset does not exist: $asset"
    }
}

$headers = @{
    Authorization          = "Bearer $Token"
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent'           = 'multicore-release-workflow'
}
$apiRoot = "https://api.github.com/repos/$Repository"
$escapedTag = [Uri]::EscapeDataString($Tag)
$notes = Get-Content -LiteralPath $NotesPath -Raw

try {
    $release = Invoke-RestMethod -Method Get -Uri "$apiRoot/releases/tags/$escapedTag" -Headers $headers
}
catch {
    $statusCode = [int]$_.Exception.Response.StatusCode
    if ($statusCode -ne 404) {
        throw
    }
    $release = $null
}

$releaseBody = @{
    tag_name         = $Tag
    target_commitish = $Target
    name             = $Title
    body             = $notes
    draft            = $false
    prerelease       = $false
    make_latest      = 'true'
} | ConvertTo-Json

if ($null -eq $release) {
    $release = Invoke-RestMethod -Method Post -Uri "$apiRoot/releases" -Headers $headers `
        -ContentType 'application/json' -Body $releaseBody
}
else {
    $release = Invoke-RestMethod -Method Patch -Uri "$apiRoot/releases/$($release.id)" -Headers $headers `
        -ContentType 'application/json' -Body $releaseBody
}

$existingAssets = @($release.assets)
foreach ($asset in $AssetPath) {
    $resolvedAsset = (Resolve-Path -LiteralPath $asset).Path
    $assetName = [IO.Path]::GetFileName($resolvedAsset)

    foreach ($existing in @($existingAssets | Where-Object { $_.name -ceq $assetName })) {
        Invoke-RestMethod -Method Delete -Uri "$apiRoot/releases/assets/$($existing.id)" -Headers $headers | Out-Null
    }

    $encodedName = [Uri]::EscapeDataString($assetName)
    $uploadUri = "https://uploads.github.com/repos/$Repository/releases/$($release.id)/assets?name=$encodedName"
    Invoke-RestMethod -Method Post -Uri $uploadUri -Headers $headers `
        -ContentType 'application/octet-stream' -InFile $resolvedAsset | Out-Null
}

Write-Host "Published $Tag to $($release.html_url)"
