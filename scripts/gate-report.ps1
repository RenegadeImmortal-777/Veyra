# Kovrek release gate report
#
# Runs every release gate in order, captures the RAW output and the exit code of
# each one, and writes gate-report.txt at the repository root.
#
# Why a script: the report file can be inspected afterwards without re-running
# anything, and it preserves exit codes instead of hiding them behind a pipe.
#
# Usage (from the repository root):
#     powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\gate-report.ps1
#
# Exit code is 0 when every gate passes, 1 when any gate fails.
#
# -Quick runs the Rust gates only and skips the outer scripts (package matrix,
# ecosystem gate, release check), which are the slow part. Use it as the inner
# loop while refactoring; run the full suite before a release.

param([switch]$Quick)

$ErrorActionPreference = 'Continue'

$root   = Split-Path -Parent $PSScriptRoot
$engine = Join-Path $root 'engine-rs'
$report = Join-Path $root 'gate-report.txt'

$lines = New-Object System.Collections.Generic.List[string]
function Add-Line {
    param([string]$Text)
    $lines.Add([string]$Text) | Out-Null
    Write-Host $Text
}

Add-Line 'Kovrek gate report'
if ($Quick) { Add-Line 'mode      : QUICK (Rust gates only)' }
Add-Line ("generated : {0}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Add-Line ("machine   : {0}" -f $env:COMPUTERNAME)
Add-Line ("repo      : {0}" -f $root)

# --------------------------------------------------------------------------
# Environment
# --------------------------------------------------------------------------
Add-Line ''
Add-Line '--- environment ---'

foreach ($tool in @('cargo', 'rustc', 'git', 'python')) {
    $cmd = Get-Command $tool -ErrorAction SilentlyContinue
    if ($cmd) {
        $version = (& $tool --version 2>&1 | Select-Object -First 1)
        Add-Line ("{0,-7}: {1}  [{2}]" -f $tool, $version, $cmd.Source)
    } else {
        Add-Line ("{0,-7}: NOT FOUND ON PATH" -f $tool)
    }
}

if (Test-Path (Join-Path $root '.git')) {
    Push-Location $root
    $head   = (& git rev-parse --short HEAD 2>&1) -join ' '
    $branch = (& git rev-parse --abbrev-ref HEAD 2>&1) -join ' '
    $dirty  = @(& git status --porcelain 2>&1).Count
    Pop-Location
    Add-Line ("git head: {0} (branch {1})" -f $head, $branch)
    Add-Line ("git dirty files: {0}" -f $dirty)
}

# --------------------------------------------------------------------------
# Gate runner
# --------------------------------------------------------------------------
$gates = New-Object System.Collections.Generic.List[object]

function Invoke-Gate {
    param(
        [string]   $Name,
        [string]   $WorkingDirectory,
        [string]   $File,
        [string[]] $Arguments
    )

    Add-Line ''
    Add-Line ("=== {0} ===" -f $Name)
    Add-Line ("cwd     : {0}" -f $WorkingDirectory)
    Add-Line ("command : {0} {1}" -f $File, ($Arguments -join ' '))

    Push-Location $WorkingDirectory
    # Capture output and exit code separately: piping would lose the status.
    $output = & $File @Arguments 2>&1
    $code   = $LASTEXITCODE
    Pop-Location

    foreach ($line in $output) { Add-Line ([string]$line) }

    $verdict = if ($code -eq 0) { 'PASS' } else { 'FAIL' }
    Add-Line ("exit code: {0}" -f $code)
    Add-Line ("RESULT   : {0}" -f $verdict)

    $gates.Add([pscustomobject]@{ Name = $Name; ExitCode = $code; Verdict = $verdict })
}

# --------------------------------------------------------------------------
# Gates
# --------------------------------------------------------------------------
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Add-Line ''
    Add-Line 'cargo is not on PATH - the Rust gates cannot run. Install the Rust'
    Add-Line 'toolchain or fix PATH, then run this script again.'
}
elseif (-not (Test-Path (Join-Path $engine 'Cargo.toml'))) {
    Add-Line ''
    Add-Line ("engine-rs/Cargo.toml not found under {0}" -f $root)
}
else {
    Invoke-Gate -Name 'cargo fmt --check'                 -WorkingDirectory $engine -File 'cargo' -Arguments @('fmt', '--check')
    Invoke-Gate -Name 'cargo test'                        -WorkingDirectory $engine -File 'cargo' -Arguments @('test')
    Invoke-Gate -Name 'cargo check (portable core)'       -WorkingDirectory $engine -File 'cargo' -Arguments @('check', '--no-default-features')
    Invoke-Gate -Name 'cargo clippy (-D warnings)'        -WorkingDirectory $engine -File 'cargo' -Arguments @('clippy', '--all-targets', '--all-features', '--', '-D', 'warnings')
    Invoke-Gate -Name 'cargo build --release'             -WorkingDirectory $engine -File 'cargo' -Arguments @('build', '--release')
}

if (-not $Quick) {
    foreach ($extra in @('package-matrix.ps1', 'ecosystem-gate.ps1', 'release-check.ps1')) {
        $path = Join-Path $PSScriptRoot $extra
        if (Test-Path $path) {
            Invoke-Gate -Name $extra -WorkingDirectory $root -File 'powershell' `
                -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $path)
        }
    }
}

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------
Add-Line ''
Add-Line '=== summary ==='
foreach ($gate in $gates) {
    Add-Line ("{0,-28} {1}" -f $gate.Name, $gate.Verdict)
}

$failed = @($gates | Where-Object { $_.ExitCode -ne 0 }).Count
Add-Line ("gates run: {0}    failed: {1}" -f $gates.Count, $failed)

$lines -join "`r`n" | Set-Content -Path $report -Encoding UTF8

Write-Host ''
Write-Host ("Report written to {0}" -f $report)

if ($failed -gt 0) { exit 1 }
exit 0
