$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskNpm = Join-Path $taskRoot '.tools\npm-12.2.1\package\bin\npm-cli.js'
Push-Location $taskRoot
try {
    if (Test-Path -LiteralPath $taskNpm) { & node $taskNpm @args } else { & npm @args }
    $taskExit = $LASTEXITCODE
} finally { Pop-Location }
exit $taskExit
