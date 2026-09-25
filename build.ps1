param([switch]$Release, [switch]$Test)
$ErrorActionPreference = 'Stop'
$taskRoot = $PSScriptRoot
if (Test-Path "$taskRoot/.tools/cargo/bin/cargo.exe") {
    $env:CARGO_HOME = "$taskRoot/.tools/cargo"
    $env:RUSTUP_HOME = "$taskRoot/.tools/rustup"
    $env:PATH = "$taskRoot/.tools/cargo/bin;$env:PATH"
}
Push-Location $taskRoot
try {
    if ($Test) { cargo test } elseif ($Release) { cargo build --release } else { cargo build }
    if ($LASTEXITCODE -ne 0) { throw "Cargo failed with exit code $LASTEXITCODE" }
} finally { Pop-Location }
