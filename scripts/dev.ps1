$ErrorActionPreference = 'Stop'
$taskCargoArguments = @($args)
if ($taskCargoArguments.Count -eq 0) { $taskCargoArguments = @('test', '--workspace') }
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskCargo = Join-Path $taskRoot '.tools\cargo\bin\cargo.exe'
$taskLocalHost = Join-Path $taskRoot '.tools\rustup\toolchains\stable-x86_64-pc-windows-gnullvm'
$taskVariables = @('CARGO_HOME', 'RUSTUP_HOME', 'PATH', 'CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER', 'CARGO_ENCODED_RUSTFLAGS')
$taskOriginalEnvironment = @{}
foreach ($taskVariable in $taskVariables) {
    $taskOriginalEnvironment[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable, 'Process')
}
$taskResult = 1
Push-Location $taskRoot
try {
    if ((Test-Path -LiteralPath $taskCargo) -and (Test-Path -LiteralPath $taskLocalHost)) {
        $env:CARGO_HOME = Join-Path $taskRoot '.tools\cargo'
        $env:RUSTUP_HOME = Join-Path $taskRoot '.tools\rustup'
        $env:PATH = (Join-Path $taskRoot '.tools\cargo\bin') + ';' + (Join-Path $taskLocalHost 'lib\rustlib\x86_64-pc-windows-gnullvm\bin') + ';' + $env:PATH
        $taskLlvm = Join-Path $taskRoot '.tools\llvm-mingw-20261020-ucrt-x86_64\bin'
        if (Test-Path -LiteralPath $taskLlvm) {
            $env:PATH = $taskLlvm + ';' + $env:PATH
            $taskSdkLib = Join-Path (Split-Path -Parent $taskLlvm) 'x86_64-w64-mingw32\lib'
            $taskRuntimeLib = Join-Path $taskLocalHost 'lib\rustlib\x86_64-pc-windows-gnullvm\lib\self-contained'
            $env:CARGO_ENCODED_RUSTFLAGS = @('-C', 'target-feature=+crt-static', '-L', ('native=' + $taskRuntimeLib), '-L', ('native=' + $taskSdkLib)) -join [char]31
        }
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER = 'rust-lld'
        & $taskCargo '+stable-x86_64-pc-windows-gnullvm' @taskCargoArguments
    } else {
        & cargo @taskCargoArguments
    }
    $taskResult = $LASTEXITCODE
} finally {
    foreach ($taskVariable in $taskVariables) {
        [Environment]::SetEnvironmentVariable($taskVariable, $taskOriginalEnvironment[$taskVariable], 'Process')
    }
    Pop-Location
}
exit $taskResult

