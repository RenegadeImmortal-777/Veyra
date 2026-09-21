$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "engine-rs\target\debug\veyra.exe"
$env:VEYRA_HOME = $root
$registry = Get-Content (Join-Path $root "registry\index.json") -Raw | ConvertFrom-Json
$names = @($registry.packages.PSObject.Properties.Name)
$base = Join-Path $env:TEMP "veyra-package-matrix"
if (Test-Path $base) { Remove-Item $base -Recurse -Force }
New-Item -ItemType Directory $base | Out-Null
$failed = @()
foreach ($name in $names) {
    $project = Join-Path $base $name
    & $exe new $project | Out-Null
    Push-Location $project
    & $exe add $name | Out-Null
    ('use "' + $name + '";' + [Environment]::NewLine + 'print("PACKAGE_OK");') | Set-Content '.\src\main.vy'
    & $exe check *> $null
    if ($LASTEXITCODE -ne 0) { $failed += $name; Write-Host "FAIL $name" } else { Write-Host "PASS $name" }
    Pop-Location
}
Write-Host "TOTAL=$($names.Count)"
Write-Host "FAILED=$($failed.Count)"
if ($failed.Count -gt 0) { throw ($failed -join ",") }
