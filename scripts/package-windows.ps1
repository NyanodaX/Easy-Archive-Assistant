param([string]$NoticesDirectory)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskBinary = Join-Path $taskRoot 'target\release\easy-archive-assistant.exe'
$taskDesktop = Join-Path $taskRoot 'target\release\easy-archive-desktop.exe'
if (-not (Test-Path -LiteralPath $taskBinary)) { throw 'Build the release CLI first.' }
if (-not (Test-Path -LiteralPath $taskDesktop)) { throw 'Build the release desktop first.' }
if (-not $NoticesDirectory) { $NoticesDirectory = Join-Path $taskRoot '.artifacts\notices' }
$taskNotices = (Resolve-Path -LiteralPath $NoticesDirectory).Path
if (-not (Test-Path -LiteralPath (Join-Path $taskNotices 'DEPENDENCIES.json'))) { throw 'Generate dependency notices first.' }
function Copy-NewTree([string]$Source, [string]$Destination) {
    New-Item -ItemType Directory -Path $Destination -ErrorAction Stop | Out-Null
    foreach ($taskEntry in Get-ChildItem -LiteralPath $Source -Force) {
        if (($taskEntry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Refusing a reparse point in release sources.' }
        $taskCopyPath = Join-Path $Destination $taskEntry.Name
        if ($taskEntry.PSIsContainer) { Copy-NewTree $taskEntry.FullName $taskCopyPath }
        else { [IO.File]::Copy($taskEntry.FullName, $taskCopyPath, $false) }
    }
}
$taskEncoding = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    $taskInfo = (& $taskBinary --version --json) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Release executable failed its version check.' }
    $taskLabel = [string]$taskInfo.data.product_name
    if ([string]::IsNullOrWhiteSpace($taskLabel) -or $taskLabel.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0) { throw 'Invalid package name.' }
    $taskRelease = Join-Path $taskRoot ('.artifacts\releases\' + [Guid]::NewGuid().ToString('N'))
    if (Test-Path -LiteralPath $taskRelease) { throw 'Refusing to overwrite a package directory.' }
    $taskPackage = Join-Path $taskRelease $taskLabel
    New-Item -ItemType Directory -Path $taskPackage | Out-Null
    [IO.File]::Copy($taskDesktop, (Join-Path $taskPackage ($taskLabel + '.exe')), $false)
    [IO.File]::Copy($taskBinary, (Join-Path $taskPackage 'easy-archive-assistant.exe'), $false)
    $taskLoader = Join-Path $taskRoot 'target\release\WebView2Loader.dll'
    if (Test-Path -LiteralPath $taskLoader) { [IO.File]::Copy($taskLoader, (Join-Path $taskPackage 'WebView2Loader.dll'), $false) }
    foreach ($taskFile in @('README.md', 'README.zh-CN.md', 'LICENSE', 'SECURITY.md', 'CONTRIBUTING.md', 'CHANGELOG.md')) {
        [IO.File]::Copy((Join-Path $taskRoot $taskFile), (Join-Path $taskPackage $taskFile), $false)
    }
    foreach ($taskFolder in @('schemas', 'tests\fixtures')) {
        $taskSource = Join-Path $taskRoot $taskFolder
        $taskTarget = Join-Path $taskPackage $taskFolder
        Copy-NewTree $taskSource $taskTarget
    }
    Copy-NewTree $taskNotices (Join-Path $taskPackage 'third-party-notices')
    $taskHashes = Get-ChildItem -LiteralPath $taskPackage -Recurse -File | Sort-Object FullName | ForEach-Object {
        $taskHash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $taskRelative = $_.FullName.Substring($taskPackage.Length + 1).Replace('\', '/')
        $taskHash + '  ' + $taskRelative
    }
    [IO.File]::WriteAllLines((Join-Path $taskPackage 'SHA256SUMS.txt'), [string[]]$taskHashes, [Text.UTF8Encoding]::new($false))
    $taskZip = Join-Path $taskRelease ($taskLabel + '.zip')
    Compress-Archive -LiteralPath $taskPackage -DestinationPath $taskZip
    $taskZipHash = (Get-FileHash -LiteralPath $taskZip -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText(($taskZip + '.sha256'), $taskZipHash + '  ' + [IO.Path]::GetFileName($taskZip) + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    [pscustomobject]@{ package = $taskPackage; zip = $taskZip; sha256 = $taskZipHash } | ConvertTo-Json
} finally {
    [Console]::OutputEncoding = $taskEncoding
}
