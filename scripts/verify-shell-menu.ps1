param(
    [string]$Out = "D:\github\dsh-desktop\.winui-menu.png",
    [int]$CropWidth = 760,
    [int]$CropHeight = 560
)

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing, System.Windows.Forms
Add-Type -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
[DllImport("user32.dll")] public static extern void keybd_event(byte k, byte scan, uint f, IntPtr e);
'@ -Namespace Shell6 -Name N -ErrorAction SilentlyContinue

$process = Get-Process dsh-desktop-winui -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $process) { throw "the shell is not running" }
$handle = $process.MainWindowHandle

[Shell6.N]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
[Shell6.N]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
[Shell6.N]::SetForegroundWindow($handle) | Out-Null
Start-Sleep -Milliseconds 600
if ([Shell6.N]::GetForegroundWindow() -ne $handle) { throw "the shell is not in the foreground" }

$root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
$condition = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, "应用")
$item = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
if (-not $item) { throw "the 应用 menu is not in the automation tree" }
$rect = $item.Current.BoundingRectangle
$x = [int]($rect.X + $rect.Width / 2)
$y = [int]($rect.Y + $rect.Height / 2)
"mouse-clicking 应用 at $x,$y"

[Shell6.N]::SetCursorPos($x, $y) | Out-Null
Start-Sleep -Milliseconds 250
[Shell6.N]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 90
[Shell6.N]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Seconds 2

$desktop = [System.Windows.Automation.AutomationElement]::RootElement
$items = @()
$all = $desktop.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
foreach ($element in $all) {
    try {
        $name = $element.Current.Name
        if ($name -match '^(打开设置…|重试连接|打开日志目录|退出|刷新页面|关于 dsh desktop)$' -and
            $element.Current.ControlType -eq [System.Windows.Automation.ControlType]::MenuItem) {
            $items += $name
        }
    } catch { }
}

if ($items.Count -gt 0) {
    "MOUSE OK — flyout opened with: $($items -join ', ')"
} else {
    "MOUSE FAILED — no flyout after a real left click"
}

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
"saved $Out"
