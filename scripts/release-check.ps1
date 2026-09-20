$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot\..\engine-rs"
cargo fmt --check
cargo test
cargo check --no-default-features
cargo build --release
Pop-Location
Write-Host "Veyra release checks passed."
