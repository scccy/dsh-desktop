param(
    [string]$Out = "D:\github\dsh-desktop\.winui-menu.png",
    [int]$CropWidth = 760,
    [int]$CropHeight = 560
)

# Regression check for the caption-row menus (ADR-004).
#
# The trap this guards: the caption band is a non-client area by default, so the
# menus accept keyboard focus and expand with Space while ignoring every mouse
# click. Screenshots cannot show that difference reliably and keyboard-only
# tests report success, so this script drives a real left click and asserts on
# the automation tree.
#
# Everything is filtered by process id: the official DeepSeek Harness client runs
# the same page with the same menu names, and scanning the whole desktop picks up
# its items ("编辑") as if they were ours.

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing, System.Windows.Forms
Add-Type -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
[DllImport("user32.dll")] public static extern void keybd_event(byte k, byte scan, uint f, IntPtr e);
'@ -Namespace ShellMenu -Name Native -ErrorAction SilentlyContinue

$process = Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $process) { throw "the shell is not running" }
$handle = $process.MainWindowHandle
$shellPid = $process.Id

function Activate {
    [ShellMenu.Native]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
    [ShellMenu.Native]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
    [ShellMenu.Native]::SetForegroundWindow($handle) | Out-Null
    Start-Sleep -Milliseconds 500
    if ([ShellMenu.Native]::GetForegroundWindow() -ne $handle) { throw "the shell is not in the foreground" }
}

function Dismiss {
    [ShellMenu.Native]::keybd_event(0x1B, 0, 0, [IntPtr]::Zero)
    [ShellMenu.Native]::keybd_event(0x1B, 0, 2, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 400
}

# Menu items of *this* process that are currently exposed, i.e. the open flyout.
function VisibleMenuItems {
    $desktop = [System.Windows.Automation.AutomationElement]::RootElement
    $all = $desktop.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $names = @()
    foreach ($element in $all) {
        try {
            if ($element.Current.ProcessId -ne $shellPid) { continue }
            if ($element.Current.ControlType -eq [System.Windows.Automation.ControlType]::MenuItem) {
                $names += $element.Current.Name
            }
        } catch { }
    }
    $names | Where-Object { $_ } | Sort-Object -Unique
}

# The click must land on our own window; another app on top would "pass" silently.
function ClickMenuBarItem($name) {
    $root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
    $item = $root.FindFirst(
        [System.Windows.Automation.TreeScope]::Descendants,
        (New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::NameProperty, $name)))
    if (-not $item) { return $false }

    $rect = $item.Current.BoundingRectangle
    $x = [int]($rect.X + $rect.Width / 2)
    $y = [int]($rect.Y + $rect.Height / 2)
    $under = [System.Windows.Automation.AutomationElement]::FromPoint((New-Object System.Windows.Point($x, $y)))
    if ($under -and $under.Current.ProcessId -ne $shellPid) {
        throw "the point $x,$y belongs to process $($under.Current.ProcessId), not to the shell ($shellPid)"
    }

    [ShellMenu.Native]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 250
    [ShellMenu.Native]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 90
    [ShellMenu.Native]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Seconds 2
    return $true
}

$cases = @(
    @{ Menu = "应用"; Expect = @("打开设置…", "重试连接", "打开日志目录", "检查 dsh 更新…", "退出") },
    @{ Menu = "视图"; Expect = @("刷新页面", "关于 dsh desktop") }
)

$failures = 0
foreach ($case in $cases) {
    Activate
    Dismiss
    if (-not (ClickMenuBarItem $case.Menu)) {
        "FAIL — menu '$($case.Menu)' is not in the automation tree"
        $failures++
        continue
    }

    $visible = VisibleMenuItems
    $missing = $case.Expect | Where-Object { $visible -notcontains $_ }
    if ($missing.Count -eq 0) {
        "PASS — '$($case.Menu)' opened by mouse click: $($case.Expect -join ', ')"
    } else {
        "FAIL — '$($case.Menu)' click did not open its items (missing: $($missing -join ', '); visible: $($visible -join ', '))"
        $failures++
    }
}

# Leave the 应用 menu open for a screenshot, so the artefact matches a passing run.
Activate
Dismiss
$null = ClickMenuBarItem "应用"
Start-Sleep -Milliseconds 500

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$screen = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($screen)
$graphics.CopyFromScreen($bounds.X, $bounds.Y, 0, 0, $screen.Size)
$graphics.Dispose()
$crop = New-Object System.Drawing.Bitmap($CropWidth, $CropHeight)
$cropGraphics = [System.Drawing.Graphics]::FromImage($crop)
$cropGraphics.DrawImage(
    $screen,
    (New-Object System.Drawing.Rectangle(0, 0, $CropWidth, $CropHeight)),
    (New-Object System.Drawing.Rectangle(0, 0, $CropWidth, $CropHeight)),
    [System.Drawing.GraphicsUnit]::Pixel)
$cropGraphics.Dispose()
$crop.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$crop.Dispose()
$screen.Dispose()

if ($failures -gt 0) {
    "RESULT: $failures menu(s) failed — the caption row swallowed the click"
    exit 1
}
"RESULT: all caption-row menus respond to mouse clicks (screenshot: $Out)"
