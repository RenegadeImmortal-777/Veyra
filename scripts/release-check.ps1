$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot\..\engine-rs"
cargo fmt --check
if ($LASTEXITCODE -ne 0) { throw "cargo fmt --check failed" }
cargo test
if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }
cargo check --no-default-features
if ($LASTEXITCODE -ne 0) { throw "portable core check failed" }
cargo clippy --all-targets --all-features -- -D warnings
if ($LASTEXITCODE -ne 0) { throw "clippy failed" }
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "release build failed" }
Pop-Location
& "$PSScriptRoot\package-matrix.ps1"
& "$PSScriptRoot\ecosystem-gate.ps1"
Write-Host "Veyra release checks passed."
