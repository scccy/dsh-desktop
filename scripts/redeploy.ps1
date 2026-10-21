param(
    # Where the shell is installed. Defaults to the machine this project is
    # developed on; pass -InstallDir for anything else.
    [string]$InstallDir = "D:\soft\dsh-desktop",
    # Rebuild everything before touching the installed copy.
    [switch]$SkipBuild,
    # Do not start the shell after installing it.
    [switch]$NoLaunch
)

# Builds the installer, replaces the installed copy with it, and checks the result.
#
# One command for the whole loop:
#   1. close the running shell (politely first: that runs the exit path and the
#      job object takes the sponsored dsh down with it);
#   2. build the installer — which ends with its own smoke test, so a publish that
#      cannot start never reaches this step;
#   3. uninstall the old copy, install the new one;
#   4. verify the installed executable is byte-identical to the one that was just
#      published, then start it and wait for a window.
#
# Any failure stops the script with a non-zero exit code and leaves the previous
# installation untouched (the uninstall only happens after a successful build).

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$publish = Join-Path $repo "src-winui\out-pub"
$builder = Join-Path $PSScriptRoot "build-installer.ps1"

function Get-Version {
    $manifest = Get-Content (Join-Path $repo "Cargo.toml") -Raw
    if ($manifest -match '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw "could not read the workspace version from Cargo.toml"
}

function Close-RunningShell {
    $running = @(Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) {
        Write-Host "shell: not running" -ForegroundColor DarkGray
        return
    }

    foreach ($process in $running) {
        Write-Host "shell: closing pid $($process.Id)" -ForegroundColor Cyan
        # Graceful first: the exit path stops the child and the job object reaps
        # whatever is left. Force only if it does not go away.
        $null = $process.CloseMainWindow()
        if (-not $process.WaitForExit(10000)) {
            Write-Host "shell: pid $($process.Id) did not close in 10 s; killing it" -ForegroundColor Yellow
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        }
    }

    # The job object reaps the sponsored child, so normally nothing is left.
    # Stragglers are only reported: other dsh processes on this machine may belong
    # to something else entirely, and this script has no business killing them.
    Start-Sleep -Seconds 2
    $leftover = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like "*@deepseek-ai\dsh\lib\bin.js*" })
    if ($leftover.Count -gt 0) {
        Write-Host "note: $($leftover.Count) dsh process(es) still running (pid $($leftover.ProcessId -join ', ')) — not touched" -ForegroundColor DarkGray
    }
}

$version = Get-Version
Write-Host "dsh desktop $version — redeploy to $InstallDir" -ForegroundColor Cyan

# --- 1. close what is running ----------------------------------------------
Close-RunningShell

# --- 2. build (includes the publish smoke test) -----------------------------
if ($SkipBuild) {
    Write-Host "`nbuild: skipped (-SkipBuild)" -ForegroundColor Yellow
} else {
    & $builder
    if ($LASTEXITCODE -ne 0) {
        throw "the build failed; the installed copy was left untouched"
    }
}

$installer = Join-Path $repo "dist\dsh-desktop-winui-setup-$version.exe"
if (-not (Test-Path $installer)) { throw "installer not found: $installer" }
$publishedExe = Join-Path $publish "dsh-desktop-winui.exe"
if (-not (Test-Path $publishedExe)) { throw "publish output not found: $publishedExe" }

# --- 3. replace the installation -------------------------------------------
$uninstaller = Join-Path $InstallDir "unins000.exe"
if (Test-Path $uninstaller) {
    Write-Host "`nuninstall: $InstallDir" -ForegroundColor Cyan
    $process = Start-Process -FilePath $uninstaller -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART" -PassThru -Wait
    if ($process.ExitCode -ne 0) { throw "the uninstaller failed with exit code $($process.ExitCode)" }
    Start-Sleep -Seconds 2
    if (Test-Path $InstallDir) {
        # A file held open (WebView2 profile, an editor) can keep the folder alive.
        Write-Host "uninstall: $InstallDir still exists; removing the remainder" -ForegroundColor Yellow
        Remove-Item -Recurse -Force $InstallDir -ErrorAction SilentlyContinue
    }
} else {
    Write-Host "`nuninstall: nothing installed at $InstallDir" -ForegroundColor DarkGray
}

Write-Host "install: $installer" -ForegroundColor Cyan
$process = Start-Process -FilePath $installer -ArgumentList @(
    "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART",
    "/DIR=$InstallDir", "/MERGETASKS=desktopicon"
) -PassThru -Wait
if ($process.ExitCode -ne 0) { throw "the installer failed with exit code $($process.ExitCode)" }
Start-Sleep -Seconds 2

# --- 4. verify what landed --------------------------------------------------
$installedExe = Join-Path $InstallDir "app\dsh-desktop-winui.exe"
if (-not (Test-Path $installedExe)) { throw "the installer did not create $installedExe" }

$installedHash = (Get-FileHash $installedExe -Algorithm SHA256).Hash
$publishedHash = (Get-FileHash $publishedExe -Algorithm SHA256).Hash
if ($installedHash -ne $publishedHash) {
    throw "the installed executable does not match the published one ($installedHash vs $publishedHash)"
}

$payload = Get-ChildItem (Join-Path $InstallDir "app") -Recurse -File | Measure-Object -Property Length -Sum
$desktopShortcut = Join-Path ([Environment]::GetFolderPath('Desktop')) "dsh desktop.lnk"
$startMenuShortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\dsh desktop\dsh desktop.lnk"

Write-Host ""
Write-Host ("installed : {0} ({1:N1} MB / {2} files)" -f $InstallDir, ($payload.Sum / 1MB), $payload.Count)
Write-Host "verified  : installed exe == published exe (SHA-256)"
Write-Host "shortcuts : desktop=$([bool](Test-Path $desktopShortcut)) start-menu=$([bool](Test-Path $startMenuShortcut))"
if (-not (Test-Path $desktopShortcut)) {
    Write-Host "warning: the desktop shortcut is missing" -ForegroundColor Yellow
}

if ($NoLaunch) {
    Write-Host "launch    : skipped (-NoLaunch)" -ForegroundColor Yellow
    exit 0
}

# --- 5. start it and wait for the window ------------------------------------
Write-Host "`nlaunch: $installedExe" -ForegroundColor Cyan
$shell = Start-Process -FilePath $installedExe -WorkingDirectory (Join-Path $InstallDir "app") -PassThru
$deadline = (Get-Date).AddSeconds(40)
$windowed = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 300
    $shell.Refresh()
    if ($shell.HasExited) { break }
    if ($shell.MainWindowHandle -ne 0) { $windowed = $true; break }
}

if ($shell.HasExited) {
    throw "the installed shell exited with code $($shell.ExitCode); see %LOCALAPPDATA%\dsh desktop\logs"
}
if (-not $windowed) {
    throw "the installed shell did not show a window within 40 s"
}

Start-Sleep -Seconds 5
$shell.Refresh()
$children = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.ParentProcessId -eq $shell.Id -or $_.CommandLine -like "*@deepseek-ai\dsh\lib\bin.js*" })

Write-Host ("running   : pid {0}, {1} MB, dsh children={2}" -f $shell.Id, [math]::Round($shell.WorkingSet64 / 1MB, 0), $children.Count) -ForegroundColor Green
Write-Host "redeploy: done" -ForegroundColor Green
