param([string]$Binary, [ValidateSet('copy','draft','cleanup','reshard','rebuild-copy','rebuild-publish')][string]$Phase = 'copy')
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
if (-not $Binary) { $Binary = Join-Path $taskRoot 'target\release\easy-archive-assistant.exe' }
if (-not (Test-Path -LiteralPath $Binary)) { throw 'Build the release CLI first.' }
$taskPattern = if ($Phase -eq 'copy') { '^restart-[0-9a-f]{16}\.exe$' } elseif ($Phase -eq 'reshard') { '^reshard-[0-9a-f]{16}\.exe$' } elseif ($Phase.StartsWith('rebuild-')) { '^rebuild-[0-9a-f]{16}\.exe$' } else { '^lifecycle-[0-9a-f]{16}\.exe$' }
$taskWorker = Get-ChildItem -LiteralPath (Join-Path $taskRoot 'target\debug\deps') -File |
    Where-Object { $_.Name -match $taskPattern } |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $taskWorker) { throw 'Run the workspace tests to build the controlled crash worker.' }
$taskQa = Join-Path $taskRoot ('.artifacts\interrupted-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskQa | Out-Null
[IO.File]::WriteAllBytes((Join-Path $taskQa 'sample.zip'), [Text.Encoding]::ASCII.GetBytes('hello'))
[IO.File]::WriteAllBytes((Join-Path $taskQa 'unrelated.txt'), [Text.Encoding]::ASCII.GetBytes('KEEP'))
if ($Phase -eq 'reshard' -or $Phase.StartsWith('rebuild-')) {
    $taskSeed = & $Binary split (Join-Path $taskQa 'sample.zip') --size 2B --json
    if ($LASTEXITCODE -ne 0) { throw 'Source group creation failed.' }
}
if ($Phase.StartsWith('rebuild-')) {
    $taskRequest = @{ original = (Join-Path $taskQa 'sample.zip'); manifest = (Join-Path $taskQa 'sample.zip.afparts.json'); part = 2; output_dir = $taskQa; basename = 'rebuilt.zip.002' } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText((Join-Path $taskQa 'request.json'), $taskRequest, [Text.UTF8Encoding]::new($false))
}
$taskInfo = [Diagnostics.ProcessStartInfo]::new()
$taskInfo.FileName = $taskWorker.FullName
$taskInfo.Arguments = if ($Phase -eq 'copy') { '--ignored --exact restart_crash_worker' } elseif ($Phase -eq 'reshard') { '--ignored --exact reshard_crash_worker' } elseif ($Phase.StartsWith('rebuild-')) { '--ignored --exact rebuild_crash_worker' } else { '--ignored --exact lifecycle_crash_worker' }
$taskInfo.UseShellExecute = $false
$taskInfo.CreateNoWindow = $true
$taskInfo.RedirectStandardOutput = $true
$taskInfo.RedirectStandardError = $true
if ($Phase -eq 'copy') {
    $taskInfo.EnvironmentVariables['EAA_RESTART_ROOT'] = $taskQa
    $taskInfo.EnvironmentVariables['EAA_RESTART_POINT'] = 'copy'
} elseif ($Phase -eq 'reshard') {
    $taskInfo.EnvironmentVariables['EAA_RESHARD_ROOT'] = $taskQa
    $taskInfo.EnvironmentVariables['EAA_RESHARD_INPUT'] = Join-Path $taskQa 'sample.zip.afparts.json'
    $taskInfo.EnvironmentVariables['EAA_RESHARD_BASENAME'] = 'reshaped.zip'
} elseif ($Phase.StartsWith('rebuild-')) {
    $taskInfo.EnvironmentVariables['EAA_REBUILD_ROOT'] = $taskQa
    $taskInfo.EnvironmentVariables['EAA_REBUILD_POINT'] = if ($Phase -eq 'rebuild-copy') { 'copy' } else { 'publish' }
} else {
    $taskInfo.EnvironmentVariables['EAA_LIFECYCLE_ROOT'] = $taskQa
    $taskInfo.EnvironmentVariables['EAA_LIFECYCLE_POINT'] = $Phase
}
$taskChild = [Diagnostics.Process]::Start($taskInfo)
try {
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(15)
    $taskReady = Join-Path $taskQa 'ready'
    while (-not (Test-Path -LiteralPath $taskReady) -and [DateTime]::UtcNow -lt $taskDeadline) {
        if ($taskChild.HasExited) { throw 'Controlled worker exited before its interruption point.' }
        [Threading.Thread]::Sleep(10)
    }
    if (-not (Test-Path -LiteralPath $taskReady)) { throw 'Worker failed to reach interruption point.' }
} finally {
    if (-not $taskChild.HasExited) { $taskChild.Kill() }
    $taskChild.WaitForExit()
    $taskChild.Dispose()
}
$taskReport = & $Binary transactions $taskQa --json
if ($LASTEXITCODE -ne 0) { throw 'Interrupted task report failed.' }
$taskExpectedStatus = if ($Phase -eq 'cleanup') { 'committed' } else { 'needs_recovery' }
$taskData = ($taskReport | ConvertFrom-Json).data.tasks | Where-Object { $_.status -eq $taskExpectedStatus } | Select-Object -First 1
if (-not $taskData.restart_record_present) { throw 'Missing startup record or draft candidate.' }
$taskOperation = if ($Phase -eq 'cleanup' -or $Phase -eq 'rebuild-publish') { 'recover' } else { 'restart' }
if ($taskData.status -ne $taskExpectedStatus) { throw 'Unexpected task state.' }
$taskDirectory = $taskData.directory
[IO.File]::WriteAllBytes((Join-Path $taskDirectory 'unknown-kept.txt'), [Text.Encoding]::ASCII.GetBytes('UNKNOWN'))
$taskOldFiles = @{}
Get-ChildItem -LiteralPath $taskDirectory -Force -File | ForEach-Object { $taskOldFiles[$_.FullName] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
$taskBefore = @(Get-ChildItem -LiteralPath $taskDirectory -Force).Count
$taskPlan = & $Binary $taskOperation $taskDirectory --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone restart plan failed.' }
if (($taskPlan | ConvertFrom-Json).data.writes_performed -ne $false) { throw 'Restart plan reported writes.' }
if (@(Get-ChildItem -LiteralPath $taskDirectory -Force).Count -ne $taskBefore) { throw 'Restart planning created files.' }
$taskResult = & $Binary $taskOperation $taskDirectory --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone restart failed.' }
$taskReceipt = $taskResult | ConvertFrom-Json
if ($taskReceipt.status -ne 'success') { throw 'Restart did not succeed.' }
foreach ($taskOldFile in $taskOldFiles.Keys) {
    if ((Get-FileHash -LiteralPath $taskOldFile -Algorithm SHA256).Hash -ne $taskOldFiles[$taskOldFile]) { throw 'Previous or unknown file was changed.' }
}
if ([IO.File]::ReadAllText((Join-Path $taskQa 'sample.zip')) -ne 'hello') { throw 'Input changed.' }
if ([IO.File]::ReadAllText((Join-Path $taskQa 'unrelated.txt')) -ne 'KEEP') { throw 'Unrelated file changed.' }
if ($Phase.StartsWith('rebuild-')) {
    if ([IO.File]::ReadAllText((Join-Path $taskQa 'rebuilt.zip.002')) -ne 'll') { throw 'Rebuild recovery changed target bytes.' }
    if ($taskReceipt.data.verification_scope -ne 'target_range_only' -or $taskReceipt.data.target_range.start -ne 2 -or $taskReceipt.data.target_range.end_exclusive -ne 4) { throw 'Range scope lost on restart/recovery.' }
} else {
    $taskManifest = $taskReceipt.data.manifest_path
    $taskResult = & $Binary merge $taskManifest --basename restored.zip --json
    if ($LASTEXITCODE -ne 0) { throw 'Post-restart merge failed.' }
    if ([IO.File]::ReadAllText((Join-Path $taskQa 'restored.zip')) -ne 'hello') { throw 'Restart round trip changed bytes.' }
}
$taskResult = & $Binary restart $taskDirectory --json
if ($LASTEXITCODE -ne 6) { throw 'Completed task accepted a writing restart.' }
Write-Output "PASS: interrupted $Phase phase, read-only plan, standalone $taskOperation, previous/unknown files retained, exact output and completed-task rejection."
exit 0
