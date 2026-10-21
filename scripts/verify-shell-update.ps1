param(
    [int]$TimeoutSeconds = 60,
    [string]$Out = "D:\github\dsh-desktop\.winui-update.png"
)

# Drives 应用 → 检查 dsh 更新… with real mouse input and waits for the verdict
# dialog. Proves the whole chain: caption-row menu -> C# bridge -> Rust npm probe
# -> event -> ContentDialog.

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing, System.Windows.Forms
Add-Type -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
[DllImport("user32.dll")] public static extern void keybd_event(byte k, byte scan, uint f, IntPtr e);
'@ -Namespace ShellUpdate -Name Native -ErrorAction SilentlyContinue

$process = Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $process) { throw "the shell is not running" }
$handle = $process.MainWindowHandle

[ShellUpdate.Native]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
[ShellUpdate.Native]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
[ShellUpdate.Native]::SetForegroundWindow($handle) | Out-Null
Start-Sleep -Milliseconds 600
if ([ShellUpdate.Native]::GetForegroundWindow() -ne $handle) { throw "the shell is not in the foreground" }

$root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
$appMenu = $root.FindFirst(
    [System.Windows.Automation.TreeScope]::Descendants,
    (New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, "应用")))
if (-not $appMenu) { throw "the 应用 menu is missing" }

$rect = $appMenu.Current.BoundingRectangle
[ShellUpdate.Native]::SetCursorPos([int]($rect.X + $rect.Width / 2), [int]($rect.Y + $rect.Height / 2)) | Out-Null
Start-Sleep -Milliseconds 250
[ShellUpdate.Native]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 90
[ShellUpdate.Native]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Seconds 2

$checkItem = $root.FindFirst(
    [System.Windows.Automation.TreeScope]::Descendants,
    (New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, "检查 dsh 更新…")))
if (-not $checkItem) {
    $desktop = [System.Windows.Automation.AutomationElement]::RootElement
    $checkItem = $desktop.FindFirst(
        [System.Windows.Automation.TreeScope]::Descendants,
        (New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::NameProperty, "检查 dsh 更新…")))
}
if (-not $checkItem) { throw "the update menu item is missing" }

$checkRect = $checkItem.Current.BoundingRectangle
[ShellUpdate.Native]::SetCursorPos([int]($checkRect.X + $checkRect.Width / 2), [int]($checkRect.Y + $checkRect.Height / 2)) | Out-Null
Start-Sleep -Milliseconds 250
[ShellUpdate.Native]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 90
[ShellUpdate.Native]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
"clicked 检查 dsh 更新… — waiting for the verdict"

$titles = @("有新版本可用", "已是最新版本", "检查更新失败")
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$found = $null
while ((Get-Date) -lt $deadline -and -not $found) {
    Start-Sleep -Milliseconds 700
    $desktop = [System.Windows.Automation.AutomationElement]::RootElement
    foreach ($title in $titles) {
        $hit = $desktop.FindFirst(
            [System.Windows.Automation.TreeScope]::Descendants,
            (New-Object System.Windows.Automation.PropertyCondition(
                [System.Windows.Automation.AutomationElement]::NameProperty, $title)))
        if ($hit) { $found = $hit; break }
    }
}

if (-not $found) {
    "FAIL — no verdict dialog appeared within $TimeoutSeconds s"
    exit 1
}

$body = @()
$desktop = [System.Windows.Automation.AutomationElement]::RootElement
$texts = $desktop.FindAll(
    [System.Windows.Automation.TreeScope]::Descendants,
    (New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Text)))
foreach ($text in $texts) {
    try {
        if ($text.Current.Name -match 'dsh |最新|升级命令|npm|exited') { $body += $text.Current.Name }
    } catch { }
}

"PASS — dialog: $($found.Current.Name)"
$body | Select-Object -Unique | ForEach-Object { "  · $_" }

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$screen = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($screen)
$graphics.CopyFromScreen($bounds.X, $bounds.Y, 0, 0, $screen.Size)
$graphics.Dispose()
$crop = New-Object System.Drawing.Bitmap(700, 420)
$cropGraphics = [System.Drawing.Graphics]::FromImage($crop)
$cropGraphics.DrawImage(
    $screen,
    (New-Object System.Drawing.Rectangle(0, 0, 700, 420)),
    (New-Object System.Drawing.Rectangle(0, 0, 700, 420)),
    [System.Drawing.GraphicsUnit]::Pixel)
$cropGraphics.Dispose()
$crop.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$crop.Dispose()
$screen.Dispose()
"screenshot: $Out"
