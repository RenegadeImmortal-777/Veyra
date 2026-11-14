$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$releaseExe = Join-Path $root "engine-rs\target\release\kovrek.exe"
$debugExe = Join-Path $root "engine-rs\target\debug\kovrek.exe"
$exe = if (Test-Path $releaseExe) { $releaseExe } else { $debugExe }
if (-not (Test-Path $exe)) { throw "Kovrek compiler binary not found" }
$env:KOVREK_HOME = $root
$registry = Get-Content (Join-Path $root "registry\index.json") -Raw | ConvertFrom-Json
$names = @($registry.packages.PSObject.Properties.Name)
$base = Join-Path $env:TEMP "kovrek-package-matrix"
if (Test-Path $base) { Remove-Item $base -Recurse -Force }
New-Item -ItemType Directory $base | Out-Null
$failed = @()
foreach ($name in $names) {
    $project = Join-Path $base $name
    & $exe new $project | Out-Null
    Push-Location $project
    & $exe add $name | Out-Null
    ('use "' + $name + '";' + [Environment]::NewLine + 'let pkg = import_package("' + $name + '");' + [Environment]::NewLine + 'assert(len(keys(pkg)) > 0);') | Set-Content '.\src\main.kvr'
    & $exe check *> $null
    $checkOk = ($LASTEXITCODE -eq 0)
    $runOk = $false
    if ($checkOk) {
        & $exe run *> $null
        $runOk = ($LASTEXITCODE -eq 0)
    }
    if (-not ($checkOk -and $runOk)) { $failed += $name; Write-Host "FAIL $name" } else { Write-Host "PASS $name" }
    Pop-Location
}
Write-Host "TOTAL=$($names.Count)"
Write-Host "FAILED=$($failed.Count)"
if ($failed.Count -gt 0) { throw ($failed -join ",") }
