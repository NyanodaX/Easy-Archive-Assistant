param([string]$Binary, [ValidateSet('copy','publish')][string]$Phase = 'copy')
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
if (-not $Binary) { $Binary = Join-Path $taskRoot 'target\release\easy-archive-assistant.exe' }
$taskWorker = Get-ChildItem -LiteralPath (Join-Path $taskRoot 'target\debug\deps') -File |
    Where-Object { $_.Name -match '^rescue-[0-9a-f]{16}\.exe$' } | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $taskWorker) { throw 'Build workspace tests first.' }
$taskQa = Join-Path $taskRoot ('.artifacts\rescue-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskQa | Out-Null
$taskOriginal = Join-Path $taskQa 'sample.zip'
[IO.File]::WriteAllBytes($taskOriginal, [Text.Encoding]::ASCII.GetBytes('hello'))
$taskResult = & $Binary split $taskOriginal --size 2B --json
if ($LASTEXITCODE -ne 0) { throw 'Source split failed.' }
$taskHash = ($taskResult | ConvertFrom-Json).data.sha256
$taskManifest = Join-Path $taskQa 'sample.zip.afparts.json'
$taskResult = & $Binary reshard $taskManifest --size 3B --basename backup.zip --json
if ($LASTEXITCODE -ne 0) { throw 'Backup creation failed.' }
# This exact file is a fixture created above in the unique owned directory.
[IO.File]::Delete((Join-Path $taskQa 'sample.zip.002'))
$taskRequest = @{ target_manifest = $taskManifest; source_manifests = @((Join-Path $taskQa 'backup.zip.afparts.json')); part = 2; output_dir = $taskQa; basename = $null; expected_sha256 = $taskHash } | ConvertTo-Json -Compress
[IO.File]::WriteAllText((Join-Path $taskQa 'request.json'), $taskRequest, [Text.UTF8Encoding]::new($false))
$taskInfo = [Diagnostics.ProcessStartInfo]::new()
$taskInfo.FileName = $taskWorker.FullName
$taskInfo.Arguments = '--ignored --exact rescue_crash_worker'
$taskInfo.UseShellExecute = $false
$taskInfo.CreateNoWindow = $true
$taskInfo.RedirectStandardOutput = $true
$taskInfo.RedirectStandardError = $true
$taskInfo.EnvironmentVariables['EAA_RESCUE_ROOT'] = $taskQa
$taskInfo.EnvironmentVariables['EAA_RESCUE_POINT'] = $Phase
$taskChild = [Diagnostics.Process]::Start($taskInfo)
try {
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath (Join-Path $taskQa 'ready')) -and [DateTime]::UtcNow -lt $taskDeadline) {
        if ($taskChild.HasExited) { throw 'Worker exited before checkpoint.' }
        [Threading.Thread]::Sleep(10)
    }
    if (-not (Test-Path -LiteralPath (Join-Path $taskQa 'ready'))) { throw 'Worker did not reach checkpoint.' }
} finally {
    if (-not $taskChild.HasExited) { $taskChild.Kill() }
    $taskChild.WaitForExit()
    $taskChild.Dispose()
}
$taskResult = & $Binary transactions $taskQa --json
if ($LASTEXITCODE -ne 0) { throw 'Task inspection failed.' }
$taskDirectory = (($taskResult | ConvertFrom-Json).data.tasks | Where-Object { $_.status -eq 'needs_recovery' } | Select-Object -First 1).directory
if (-not $taskDirectory) { throw 'Interrupted rescue was not visible.' }
[IO.File]::WriteAllText((Join-Path $taskDirectory 'unknown-kept.txt'), 'KEEP')
$taskOld = @{}
Get-ChildItem -LiteralPath $taskQa -Recurse -Force -File | ForEach-Object { $taskOld[$_.FullName] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
$taskOperation = if ($Phase -eq 'copy') { 'restart' } else { 'recover' }
$taskCount = @(Get-ChildItem -LiteralPath $taskQa -Recurse -Force -File).Count
$taskResult = & $Binary $taskOperation $taskDirectory --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Rescue recovery plan failed.' }
if (@(Get-ChildItem -LiteralPath $taskQa -Recurse -Force -File).Count -ne $taskCount) { throw 'Recovery plan wrote files.' }
$taskResult = & $Binary $taskOperation $taskDirectory --json
if ($LASTEXITCODE -ne 0) { throw 'Rescue recovery failed.' }
$taskReceipt = $taskResult | ConvertFrom-Json
if ($taskReceipt.data.verification_scope -ne 'target_range_only') { throw 'Lost range scope.' }
if ([IO.File]::ReadAllText((Join-Path $taskQa 'sample.zip.002')) -ne 'll') { throw 'Recovered part bytes differ.' }
foreach ($taskOldPath in $taskOld.Keys) {
    if ((Get-FileHash -LiteralPath $taskOldPath -Algorithm SHA256).Hash -ne $taskOld[$taskOldPath]) { throw 'An old or unknown file changed.' }
}
$taskResult = & $Binary verify $taskManifest --expected-sha256 $taskHash --json
if ($LASTEXITCODE -ne 0) { throw 'Restored target group does not verify.' }
$taskResult = & $Binary recover $taskDirectory --json
if ($LASTEXITCODE -ne 0) { throw 'Completed rescue recovery failed.' }
if (($taskResult | ConvertFrom-Json).verification_level -ne 'verified_against_available_sources') { throw 'Recovery incorrectly trusted an editable record.' }
Write-Output "PASS: rescue $Phase interruption, read-only plan, standalone $taskOperation, exact target bytes, unchanged old files and full target group verification."
exit 0
