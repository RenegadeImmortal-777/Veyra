$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot\..\engine-rs"
cargo fmt --check
cargo test
cargo check --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
Pop-Location
& "$PSScriptRoot\package-matrix.ps1"
Write-Host "Veyra release checks passed."
