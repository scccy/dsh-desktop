param(
    # Release is the only configuration that is worth shipping; Debug is accepted
    # so the pipeline can be exercised without a full rebuild.
    [string]$Configuration = "Release",
    [switch]$SkipRust,
    [switch]$SkipPublish
)

# Builds the Windows installer:  Rust bridge -> dotnet publish -> Inno Setup.
#
# Produces dist\dsh-desktop-winui-setup-<version>.exe. Nothing is installed or
# signed; signing is a later step that hooks in on the ISCC output.

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$project = Join-Path $repo "src-winui\DshDesktop.WinUI\DshDesktop.WinUI.csproj"
$publish = Join-Path $repo "src-winui\out-pub"
$iss = Join-Path $repo "src-winui\installer\dsh-desktop.iss"
$dist = Join-Path $repo "dist"

function Find-Tool([string]$name, [string[]]$candidates) {
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) { return $candidate }
    }
    $onPath = Get-Command $name -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    throw "$name not found. Install it, or pass the path explicitly."
}

function Find-DotNet {
    $candidates = @(
        "D:\lan\dotnet\dotnet.exe",
        "C:\Program Files\dotnet\dotnet.exe"
    )
    return Find-Tool "dotnet" $candidates
}

function Find-Iscc {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe"
    )
    return Find-Tool "ISCC.exe" $candidates
}

# The version lives in one place: the workspace manifest.
function Read-Version {
    $manifest = Get-Content (Join-Path $repo "Cargo.toml") -Raw
    if ($manifest -match '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw "could not read the workspace version from Cargo.toml"
}

$version = Read-Version
Write-Host "dsh desktop $version — building the Windows installer" -ForegroundColor Cyan

if (-not $SkipRust) {
    $cargo = Find-Tool "cargo" @("D:\lan\cargo\bin\cargo.exe", (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"))
    Write-Host "`n[1/3] Rust bridge (cdylib)" -ForegroundColor Cyan
    & $cargo build -p dsh-ffi --release --manifest-path (Join-Path $repo "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "cargo build -p dsh-ffi failed" }
} else {
    Write-Host "`n[1/3] Rust bridge (skipped)" -ForegroundColor DarkGray
}

if (-not $SkipPublish) {
    $dotnet = Find-DotNet
    Write-Host "`n[2/3] dotnet publish (self-contained, trimmed)" -ForegroundColor Cyan
    if (Test-Path $publish) { Remove-Item -Recurse -Force $publish }
    & $dotnet publish $project -c $Configuration -r win-x64 --self-contained true -o $publish -v minimal
    if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed" }
} else {
    Write-Host "`n[2/3] dotnet publish (skipped)" -ForegroundColor DarkGray
}

if (-not (Test-Path (Join-Path $publish "dsh-desktop-winui.exe"))) {
    throw "the publish output at $publish does not contain dsh-desktop-winui.exe"
}
if (-not (Test-Path (Join-Path $publish "dsh_ffi.dll"))) {
    throw "the publish output at $publish does not contain dsh_ffi.dll; run without -SkipRust"
}

Write-Host "`n[3/3] Inno Setup" -ForegroundColor Cyan
$iscc = Find-Iscc
New-Item -ItemType Directory -Force -Path $dist | Out-Null
& $iscc "/DAppVersion=$version" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

$installer = Get-ChildItem $dist -Filter "dsh-desktop-winui-setup-$version.exe" |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
if (-not $installer) { throw "the installer was not produced in $dist" }

$published = Get-ChildItem $publish -Recurse -File | Measure-Object -Property Length -Sum
Write-Host ""
Write-Host "installer : $($installer.FullName)" -ForegroundColor Green
Write-Host ("size      : {0:N1} MB (from {1:N1} MB / {2} files)" -f ($installer.Length / 1MB), ($published.Sum / 1MB), $published.Count)
Write-Host "unsigned  : SmartScreen will warn until a certificate is wired in" -ForegroundColor Yellow
