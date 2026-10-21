param(
    # Release is the only configuration that is worth shipping; Debug is accepted
    # so the pipeline can be exercised without a full rebuild.
    [string]$Configuration = "Release",
    [switch]$SkipRust,
    [switch]$SkipPublish,
    # The published build is started once before the script reports success; see
    # Invoke-SmokeTest. Skip it only when a shell is already running by design.
    [switch]$SkipSmokeTest
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
    Write-Host "`n[2/3] dotnet publish (self-contained, no trimming)" -ForegroundColor Cyan
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

# Keep only the languages the shell actually shows. The publish output carries a
# folder per culture for WinAppSDK's native resources (87 of them), and all but
# three are dead weight. This is done on disk rather than through Inno's
# `Excludes`, because that option matches file names only — every culture holds
# the same file names (Microsoft.ui.xaml.dll.mui), so name matching cannot tell
# zh-CN from af-ZA. WinUI falls back to en-us for anything missing.
$keepCultures = @("en-us", "zh-CN", "zh-TW", "Microsoft.UI.Xaml")
$dropped = Get-ChildItem $publish -Directory | Where-Object { $keepCultures -notcontains $_.Name }
foreach ($folder in $dropped) { Remove-Item -Recurse -Force $folder.FullName }
$pruned = Get-ChildItem $publish -Recurse -File | Measure-Object -Property Length -Sum
Write-Host ("`n[3/3] Inno Setup (dropped {0} culture folders; payload now {1:N1} MB / {2} files)" -f $dropped.Count, ($pruned.Sum / 1MB), $pruned.Count) -ForegroundColor Cyan

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
# --- smoke test -------------------------------------------------------------
# Start the published build once, wait for a window, close it again.
#
# This is the check that catches the whole class of "only the published build is
# broken" failures (missing .pri, trimmed-away assemblies, ABI mismatch between the
# C# shell and dsh_ffi.dll): they all produce a process that exits within seconds
# with 0xC000027B and no message. A build that cannot start must not be handed out.

function Invoke-SmokeTest {
    param([string]$Exe, [int]$TimeoutSeconds = 40)

    $running = Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue
    if ($running) {
        Write-Host "smoke test: skipped — a shell is already running (pid $($running[0].Id)); close it and rebuild to check the publish" -ForegroundColor Yellow
        return "skipped"
    }

    $crashLog = Join-Path $env:LOCALAPPDATA "dsh desktop\logs\winui-crash.log"
    $crashLogSize = if (Test-Path $crashLog) { (Get-Item $crashLog).Length } else { 0 }

    Write-Host "`nsmoke test: starting $([System.IO.Path]::GetFileName($Exe))" -ForegroundColor Cyan
    $process = Start-Process -FilePath $Exe -PassThru
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $windowed = $false
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
        $process.Refresh()
        if ($process.HasExited) { break }
        if ($process.MainWindowHandle -ne 0) { $windowed = $true; break }
    }

    if ($process.HasExited) {
        Write-Host ("smoke test: FAILED — the process exited with code {0} ({1})" -f $process.ExitCode, (Format-HexExitCode $process.ExitCode)) -ForegroundColor Red
        Show-CrashLog $crashLog $crashLogSize
        return "failed"
    }

    if (-not $windowed) {
        Write-Host "smoke test: FAILED — no window appeared within $TimeoutSeconds s" -ForegroundColor Red
        Show-CrashLog $crashLog $crashLogSize
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        return "failed"
    }

    Start-Sleep -Seconds 3
    $process.Refresh()
    $memory = [math]::Round($process.WorkingSet64 / 1MB, 0)
    Write-Host "smoke test: window up (pid $($process.Id), $memory MB) — closing it again" -ForegroundColor Green

    # Close politely so the exit path runs (it stops the sponsored child), then
    # make sure it is gone. A forced kill after the timeout is not a build failure
    # by itself: what matters is that the window came up and nothing crashed.
    $null = $process.CloseMainWindow()
    $closed = $process.WaitForExit(8000)
    if (-not $closed) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        Write-Host "smoke test: the window did not close in 8 s; the process was killed" -ForegroundColor Yellow
    }

    $process.Refresh()
    $exitCode = if ($process.HasExited) { $process.ExitCode } else { $null }
    if ($null -eq $exitCode) {
        Write-Host "smoke test: exit code unknown"
    } else {
        Write-Host ("smoke test: exit code {0}" -f (Format-HexExitCode $exitCode))
    }

    # Only a crash after the window appeared counts as failure; the smoke test is
    # about "does the published build start", not about the exit code of a window
    # the tester just closed.
    if ($exitCode -eq -1073741189) {
        Write-Host "smoke test: FAILED — the shell died with 0xC000027B (stowed exception)" -ForegroundColor Red
        Show-CrashLog $crashLog $crashLogSize
        return "failed"
    }

    if ((Test-Path $crashLog) -and (Get-Item $crashLog).Length -gt $crashLogSize) {
        Write-Host "smoke test: FAILED — the run wrote to winui-crash.log" -ForegroundColor Red
        Show-CrashLog $crashLog $crashLogSize
        return "failed"
    }

    return "passed"
}

function Format-HexExitCode([long]$Code) {
    # Windows NTSTATUS values arrive as negative Int32; show them like the OS does.
    if ($Code -lt 0) {
        $Code += 4294967296
    }
    return "0x{0:X8}" -f [uint32]$Code
}

function Show-CrashLog([string]$Path, [long]$PreviousSize) {
    if (-not (Test-Path $Path)) { return }
    $size = (Get-Item $Path).Length
    if ($size -le $PreviousSize) { return }
    Write-Host "--- winui-crash.log (new entries) ---" -ForegroundColor Yellow
    $text = Get-Content $Path -Raw
    $text.Substring([Math]::Min($PreviousSize, $text.Length)) -split "`r?`n" | Select-Object -First 20 | ForEach-Object { "  $_" }
    Write-Host "--------------------------------------" -ForegroundColor Yellow
}

if ($SkipSmokeTest) {
    Write-Host "smoke test: skipped (-SkipSmokeTest)" -ForegroundColor Yellow
    exit 0
}

$smoke = Invoke-SmokeTest -Exe (Join-Path $publish "dsh-desktop-winui.exe")
if ($smoke -eq "failed") {
    # The installer file is already on disk; a package that cannot start must not
    # be left there where someone could pick it up.
    Remove-Item $installer.FullName -Force -ErrorAction SilentlyContinue
    Write-Host ""
    Write-Host "The published build does not start; the installer was deleted." -ForegroundColor Red
    exit 1
}

if ($smoke -eq "passed") {
    Write-Host "smoke test: passed" -ForegroundColor Green
}