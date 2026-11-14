$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "engine-rs\target\release\kovrek.exe"
if (-not (Test-Path $exe)) { throw "Release binary missing: $exe" }

$base = Join-Path $env:TEMP "kovrek-ecosystem-gate"
if (Test-Path $base) { Remove-Item $base -Recurse -Force }
New-Item -ItemType Directory $base | Out-Null
$serverExe = Join-Path $base "kovrek-gate-server.exe"
Copy-Item $exe $serverExe -Force
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText($Path, $Text, $utf8)
}

$oldRegistry = $env:KOVREK_REGISTRY_URL
try {
    # Hosted registry gate
    $registryDir = Join-Path $base "registry"
    New-Item -ItemType Directory $registryDir | Out-Null
    Write-Utf8NoBom (Join-Path $registryDir "remotebase.kvr") 'fn base() -> String { return "base-ok"; }'
    Write-Utf8NoBom (Join-Path $registryDir "remotetop.kvr") 'fn top() -> String { return "top-ok"; }'
    $index = @{
        registry_version = 1
        packages = @{
            remotebase = @{
                version = "1.0.0"
                entry = "remotebase.kvr"
                url = "http://127.0.0.1:18998/remotebase.kvr"
            }
            remotetop = @{
                version = "1.0.0"
                entry = "remotetop.kvr"
                url = "http://127.0.0.1:18998/remotetop.kvr"
                dependencies = @("remotebase")
            }
        }
    } | ConvertTo-Json -Depth 10
    Write-Utf8NoBom (Join-Path $registryDir "index.json") $index
    $registryServer = Start-Process -FilePath $serverExe -ArgumentList @("serve", $registryDir, "18998") -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 500
    try {
        $client = Join-Path $base "remote-client"
        & $exe new $client | Out-Null
        Push-Location $client
        $env:KOVREK_REGISTRY_URL = "http://127.0.0.1:18998/index.json"
        & $exe add remotetop | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "remote dependency add failed" }
        if (-not (Test-Path (Join-Path $client "vendor\remotebase\remotebase.kvr"))) {
            throw "dependency package was not installed"
        }
        Write-Utf8NoBom (Join-Path $client "src\main.kvr") 'use "remotetop"; use "remotebase"; assert(remotetop.top()=="top-ok"); assert(remotebase.base()=="base-ok");'
        & $exe check | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "remote dependency package check failed" }
        & $exe run | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "remote dependency package run failed" }
        & $exe audit | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "remote dependency package audit failed" }
        Pop-Location
    } finally {
        if ($registryServer -and -not $registryServer.HasExited) { Stop-Process -Id $registryServer.Id -Force }
    }

    # HTTP header gate
    $webDir = Join-Path $base "web"
    New-Item -ItemType Directory $webDir | Out-Null
    $app = 'fn inspect(request: Map) -> Map { let value=get(request["headers"],"X-Kovrek-Test"); return {"status":200,"content_type":"text/plain; charset=utf-8","body":value,"headers":{"X-Kovrek-Reply":"yes"}}; }'
    Write-Utf8NoBom (Join-Path $webDir "app.kvr") $app
    Write-Utf8NoBom (Join-Path $webDir "routes.json") '{"routes":{"GET /inspect":"inspect"}}'
    $webServer = Start-Process -FilePath $serverExe -ArgumentList @("app", (Join-Path $webDir "app.kvr"), (Join-Path $webDir "routes.json"), "18999") -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 500
    try {
        $response = Invoke-WebRequest -Uri "http://127.0.0.1:18999/inspect" -Headers @{"X-Kovrek-Test"="header-ok"} -UseBasicParsing
        if ($response.Content -ne "header-ok") { throw "request header was not exposed to handler" }
        if ($response.Headers["X-Kovrek-Reply"] -ne "yes") { throw "custom response header was not returned" }
    } finally {
        if ($webServer -and -not $webServer.HasExited) { Stop-Process -Id $webServer.Id -Force }
    }

    Write-Host "Kovrek ecosystem gates passed."
} finally {
    $env:KOVREK_REGISTRY_URL = $oldRegistry
    Pop-Location -ErrorAction SilentlyContinue
}
