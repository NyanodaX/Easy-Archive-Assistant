param([string]$Binary)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskBinary = if ($Binary) { $Binary } else { Join-Path $taskRoot 'target\release\easy-archive-assistant.exe' }
if (-not (Test-Path -LiteralPath $taskBinary)) { throw 'Build the release CLI first.' }

# Development fixtures only. No user archive is selected or modified. Keep the
# small, uniquely-owned fixtures for inspection; never recursively delete links.
$taskQa = Join-Path $taskRoot ('.artifacts\smoke-' + [Guid]::NewGuid().ToString('N'))
$taskReal = Join-Path $taskQa 'real'
New-Item -ItemType Directory -Path $taskReal -Force | Out-Null
$taskSample = Join-Path $taskReal 'sample.zip'
[System.IO.File]::WriteAllBytes($taskSample, [byte[]](0,1,2,3,255))

$taskHelp = & $taskBinary --lang both --help
if ($LASTEXITCODE -ne 0) { throw 'Standalone release failed to start.' }
if (-not (($taskHelp -join "`n").Contains('Offline'))) { throw 'Missing English help.' }

$taskResult = & $taskBinary split $taskSample --size 2B --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone dry-run failed.' }
$taskJson = $taskResult | ConvertFrom-Json
if ($taskJson.status -ne 'planned' -or $taskJson.data.part_count -ne 3) { throw 'Unexpected plan.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne 1) { throw 'Dry-run created files.' }
if ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes($taskSample)) -ne 'AAECA/8=') { throw 'Input bytes changed.' }

$taskJunction = Join-Path $taskQa 'junction'
New-Item -ItemType Junction -Path $taskJunction -Target $taskReal | Out-Null
$taskResult = & $taskBinary inspect (Join-Path $taskJunction 'sample.zip') --json
if ($LASTEXITCODE -ne 3) { throw 'Junction ancestor was not rejected.' }
$taskJson = $taskResult | ConvertFrom-Json
if ($taskJson.error.code -ne 'UNSAFE_PATH') { throw 'Unexpected junction rejection code.' }

$taskResult = & $taskBinary split (Join-Path $taskJunction 'sample.zip') --size 2B --json
if ($LASTEXITCODE -ne 3) { throw 'Write operation accepted a junction ancestor.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne 1) { throw 'Rejected path wrote files.' }
$taskResult = & $taskBinary split $taskSample --output-dir $taskJunction --size 2B --json
if ($LASTEXITCODE -ne 3) { throw 'Write operation accepted a junction output directory.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne 1) { throw 'Rejected output directory wrote files.' }

$taskResult = & $taskBinary split $taskSample --size 2B --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone split failed.' }
$taskSplit = $taskResult | ConvertFrom-Json
if ($taskSplit.schema_version -ne 2 -or $taskSplit.status -ne 'success') { throw 'Wrong receipt schema or status.' }
if ($taskSplit.verification_level -ne 'verified_against_original') { throw 'Wrong split verification level.' }
if ($taskSplit.data.staged_payload_links_scheduled_for_removal -ne 4) { throw 'Live staging cleanup did not run.' }
$taskIndependentHash = (Get-FileHash -LiteralPath $taskSample -Algorithm SHA256).Hash.ToLowerInvariant()
if ($taskIndependentHash -ne $taskSplit.data.sha256) { throw 'Independent SHA256 does not match the receipt.' }
$taskManifest = Join-Path $taskReal 'sample.zip.afparts.json'
$taskResult = & $taskBinary verify $taskManifest --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone verify failed.' }
if (($taskResult | ConvertFrom-Json).verification_level -ne 'verified_against_available_sources') { throw 'Manifest was incorrectly treated as an external trust anchor.' }
$taskResult = & $taskBinary merge $taskManifest --basename restored.zip --expected-sha256 $taskSplit.data.sha256 --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone merge failed.' }
if ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes((Join-Path $taskReal 'restored.zip'))) -ne 'AAECA/8=') { throw 'Round trip changed bytes.' }
$taskBeforeReshard = @(Get-ChildItem -LiteralPath $taskReal -Force).Count
$taskResult = & $taskBinary reshard (Join-Path $taskReal 'sample.zip.002') --size 3B --basename reshaped.zip --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone reshard plan failed.' }
$taskPlan = $taskResult | ConvertFrom-Json
if ($taskPlan.data.creates_complete_intermediate -ne $false -or $taskPlan.data.part_count -ne 2 -or $taskPlan.verification_level -ne 'unverified') { throw 'Unexpected reshard plan.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne $taskBeforeReshard) { throw 'Reshard plan wrote files.' }
$taskResult = & $taskBinary reshard $taskManifest --size 3B --basename reshaped.zip --expected-sha256 $taskIndependentHash --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone direct reshard failed.' }
$taskReshard = $taskResult | ConvertFrom-Json
if ($taskReshard.verification_level -ne 'verified_against_original' -or $taskReshard.data.part_count -ne 2) { throw 'Wrong reshard receipt.' }
if ($taskReshard.data.staged_payload_links_scheduled_for_removal -ne 3) { throw 'Reshard live staging cleanup did not run.' }
$taskBytes = [byte[]]([System.IO.File]::ReadAllBytes((Join-Path $taskReal 'reshaped.zip.001')) + [System.IO.File]::ReadAllBytes((Join-Path $taskReal 'reshaped.zip.002')))
if ([Convert]::ToBase64String($taskBytes) -ne 'AAECA/8=') { throw 'Reshard changed bytes.' }
if (Test-Path -LiteralPath (Join-Path $taskReal 'reshaped.zip')) { throw 'Unexpected complete intermediate.' }
if ((Get-FileHash -LiteralPath $taskSample -Algorithm SHA256).Hash.ToLowerInvariant() -ne $taskIndependentHash) { throw 'Reshard changed original input.' }
$taskResult = & $taskBinary verify $taskReshard.data.manifest_path --expected-sha256 $taskIndependentHash --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone new group verification failed.' }
$taskResult = & $taskBinary recover $taskReshard.data.staging_directory --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Completed reshard did not reverify after staging cleanup.' }
$taskCount = @(Get-ChildItem -LiteralPath $taskReal -Force).Count
$taskResult = & $taskBinary coverage --target-manifest $taskManifest --source-manifest $taskReshard.data.manifest_path --part 2 --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone coverage failed.' }
$taskCoverage = $taskResult | ConvertFrom-Json
if ($taskCoverage.status -ne 'inspected' -or $taskCoverage.data.complete_coverage -ne $true -or $taskCoverage.data.covered_bytes -ne 2 -or @($taskCoverage.data.segments).Count -ne 2) { throw 'Cross-boundary coverage map was wrong.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne $taskCount) { throw 'Coverage wrote files.' }
$taskResult = & $taskBinary coverage --target-manifest $taskManifest --source-manifest $taskReshard.data.manifest_path --part 2 --metadata-only --json
if ($LASTEXITCODE -ne 0 -or ($taskResult | ConvertFrom-Json).data.complete_coverage -ne $false) { throw 'Metadata-only coverage claimed complete verification.' }
$taskResult = & $taskBinary rebuild-part --original $taskSample --manifest $taskManifest --part 2 --basename rebuilt.zip.002 --dry-run --json
if ($LASTEXITCODE -ne 0 -or ($taskResult | ConvertFrom-Json).data.plan.space.estimated_payload_bytes -ne 2) { throw 'Standalone rebuild plan failed.' }
if (@(Get-ChildItem -LiteralPath $taskReal -Force).Count -ne $taskCount) { throw 'Rebuild plan wrote files.' }
$taskResult = & $taskBinary rebuild-part --original $taskSample --manifest $taskManifest --part 2 --basename rebuilt.zip.002 --json
if ($LASTEXITCODE -ne 0) { throw 'Standalone rebuild failed.' }
$taskRebuilt = $taskResult | ConvertFrom-Json
if ($taskRebuilt.data.verification_scope -ne 'target_range_only' -or $taskRebuilt.data.staged_payload_links_scheduled_for_removal -ne 1) { throw 'Unexpected rebuild verification or cleanup.' }
if ([Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $taskReal 'rebuilt.zip.002'))) -ne 'AgM=') { throw 'Rebuilt range changed bytes.' }
$taskResult = & $taskBinary recover $taskRebuilt.data.staging_directory --json
if ($LASTEXITCODE -ne 0 -or ($taskResult | ConvertFrom-Json).data.verification_scope -ne 'target_range_only') { throw 'Completed range recovery lost its verification scope.' }
$taskResult = & $taskBinary rebuild-part --original $taskSample --manifest $taskManifest --part 2 --json
if ($LASTEXITCODE -ne 4) { throw 'Rebuild accepted overwriting an existing part.' }
$taskResult = & $taskBinary transactions $taskReal --json
if ($LASTEXITCODE -ne 0) { throw 'Task report failed.' }
if (@(($taskResult | ConvertFrom-Json).data.tasks | Where-Object { $_.status -ne 'committed' }).Count -ne 0) { throw 'Unexpected pending task.' }

$taskDirectory = (($taskResult | ConvertFrom-Json).data.tasks | Select-Object -First 1).directory
$taskBefore = @(Get-ChildItem -LiteralPath $taskDirectory -Force).Count
$taskResult = & $taskBinary recover $taskDirectory --dry-run --json
if ($LASTEXITCODE -ne 0) { throw 'Recovery planning failed.' }
if (($taskResult | ConvertFrom-Json).data.writes_performed -ne $false) { throw 'Recovery plan did not report no writes.' }
if (@(Get-ChildItem -LiteralPath $taskDirectory -Force).Count -ne $taskBefore) { throw 'Recovery plan wrote task data.' }
$taskResult = & $taskBinary recover $taskDirectory --json
if ($LASTEXITCODE -ne 0) { throw 'Idempotent committed-task recovery failed.' }

Write-Output 'PASS: standalone bilingual help, read-only plans, junction rejection, split/verify/merge/reshard/rebuild/coverage, retained tasks and recovery checks.'
exit 0

