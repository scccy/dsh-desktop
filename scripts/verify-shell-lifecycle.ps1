param(
    [string]$Exe = "D:\github\dsh-desktop\src-winui\out-pub\dsh-desktop-winui.exe"
)

# Verifies the two lifecycle guarantees the shell must keep:
#
#   1. single instance — a second launch exits and brings the first window up
#      instead of starting a second dsh (which would fight for the port);
#   2. no orphans — killing the shell (the crash case, where no exit handler
#      runs) still takes the sponsored dsh process down with it (job object).
#
# Both are asserted on real processes, not on screenshots.

$ErrorActionPreference = "Stop"

function Get-ShellProcess {
    Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue
}

function Get-DshChildren {
    Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like "*@deepseek-ai\dsh\lib\bin.js*" }
}

function Wait-Window($process, [int]$Seconds = 30) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
        $process.Refresh()
        if ($process.HasExited) { return $false }
        if ($process.MainWindowHandle -ne 0) { return $true }
    }
    return $false
}

# --- clean slate ------------------------------------------------------------
Get-ShellProcess | Stop-Process -Force
Get-DshChildren | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep -Seconds 2

$failures = 0

# --- 1. first instance ------------------------------------------------------
$first = Start-Process -FilePath $Exe -PassThru
if (-not (Wait-Window $first)) { "FAIL — the first instance never showed a window"; exit 1 }
Start-Sleep -Seconds 6
$childrenBefore = @(Get-DshChildren)
"first instance: pid=$($first.Id) window=$($first.MainWindowHandle) dsh children=$($childrenBefore.Count)"

# --- 2. second instance -----------------------------------------------------
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$second = Start-Process -FilePath $Exe -PassThru
$second.WaitForExit(15000) | Out-Null
$sw.Stop()
$second.Refresh()

if (-not $second.HasExited) {
    "FAIL — the second launch did not exit (single instance lock missing)"
    $second | Stop-Process -Force
    $failures++
} elseif ($sw.ElapsedMilliseconds -gt 10000) {
    "FAIL — the second launch took $($sw.ElapsedMilliseconds) ms to exit"
    $failures++
} else {
    "second instance: exited in $($sw.ElapsedMilliseconds) ms (exit code $($second.ExitCode))"
}

$shells = @(Get-ShellProcess)
$childrenAfter = @(Get-DshChildren)
"after second launch: shells=$($shells.Count) dsh children=$($childrenAfter.Count)"
if ($shells.Count -ne 1) { "FAIL — $($shells.Count) shell processes are running"; $failures++ }
if ($childrenAfter.Count -ne $childrenBefore.Count) {
    "FAIL — the second launch started its own dsh ($($childrenBefore.Count) -> $($childrenAfter.Count))"
    $failures++
}

# --- 2b. the second launch should have raised the first window -------------
Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();' -Namespace Lifecycle -Name User32 -ErrorAction SilentlyContinue
$first.Refresh()
$foreground = [Lifecycle.User32]::GetForegroundWindow()
if ($foreground -eq $first.MainWindowHandle) {
    "activation: the first window is in the foreground"
} else {
    # Windows may refuse a programmatic foreground switch; report, do not fail.
    "activation: foreground is $foreground, first window is $($first.MainWindowHandle) (Windows may block programmatic activation)"
}

# --- 3. crash: kill the shell, the child must follow ------------------------
$victims = @(Get-DshChildren)
$first.Refresh()
"killing the shell (pid $($first.Id)) with Stop-Process -Force, like a crash…"
$first | Stop-Process -Force
$deadline = (Get-Date).AddSeconds(10)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 250
    if ((Get-DshChildren).Count -eq 0) { break }
}

$stragglers = @(Get-DshChildren)
if ($stragglers.Count -eq 0) {
    "orphans: none — $($victims.Count) dsh process(es) died with the shell"
} else {
    "FAIL — $($stragglers.Count) dsh process(es) survived the shell: $($stragglers.ProcessId -join ', ')"
    $stragglers | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    $failures++
}

if ($failures -gt 0) {
    "RESULT: $failures lifecycle check(s) failed"
    exit 1
}
"RESULT: single instance and kill-on-close both hold"
