# 生成占位图标：assets/ 下的三平台 PNG 与 src-tauri/icons/ 下 tauri-build 需要的 ICO/PNG。
# 首版为占位素材（圆角深色底 + 字母 d），正式设计稿到位后替换 assets/icon.svg 并重跑本脚本。
# assets/icon.svg 是设计源；本脚本用 System.Drawing 按同一几何直接绘制，避免引入 SVG 光栅化依赖。
# 用法： pwsh -File scripts/render-icons.ps1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root 'assets'
$icons = Join-Path $root 'src-tauri/icons'
New-Item -ItemType Directory -Force -Path $assets, $icons | Out-Null

function New-IconBitmap {
  param([int]$Size, [double]$Inset = 0.0)

  $bitmap = New-Object System.Drawing.Bitmap $Size, $Size
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
  $graphics.Clear([System.Drawing.Color]::Transparent)

  $background = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 24, 25, 28))
  $pad = [int]($Size * $Inset)
  $side = $Size - 2 * $pad
  $radius = [int]($side * 0.22)
  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $diameter = $radius * 2
  $path.AddArc($pad, $pad, $diameter, $diameter, 180, 90)
  $path.AddArc($pad + $side - $diameter, $pad, $diameter, $diameter, 270, 90)
  $path.AddArc($pad + $side - $diameter, $pad + $side - $diameter, $diameter, $diameter, 0, 90)
  $path.AddArc($pad, $pad + $side - $diameter, $diameter, $diameter, 90, 90)
  $path.CloseFigure()
  $graphics.FillPath($background, $path)

  $family = $null
  foreach ($name in @('Segoe UI', 'Arial', 'DejaVu Sans')) {
    try { $family = New-Object System.Drawing.FontFamily $name; break } catch { }
  }
  if ($null -eq $family) { $family = (New-Object System.Drawing.FontFamily 'Arial') }
  $font = New-Object System.Drawing.Font $family, ($side * 0.62), ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
  $format = New-Object System.Drawing.StringFormat
  $format.Alignment = [System.Drawing.StringAlignment]::Center
  $format.LineAlignment = [System.Drawing.StringAlignment]::Center
  $text = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 77, 107, 254))
  $rect = New-Object System.Drawing.RectangleF $pad, $pad, $side, $side
  $graphics.DrawString('d', $font, $text, $rect, $format)

  $graphics.Dispose()
  $font.Dispose()
  $background.Dispose()
  $text.Dispose()
  $path.Dispose()
  return $bitmap
}

function Save-Png {
  param([System.Drawing.Bitmap]$Bitmap, [string]$Path)
  $Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
}

# 源 PNG：Windows 与 Linux 用满幅圆角；macOS 用内缩圆角背景（对齐官方 ICNS 约定）。
$windowsPng = New-IconBitmap -Size 1024
Save-Png -Bitmap $windowsPng -Path (Join-Path $assets 'icon-windows.png')
Save-Png -Bitmap $windowsPng -Path (Join-Path $assets 'icon-linux.png')
Save-Png -Bitmap $windowsPng -Path (Join-Path $icons 'icon.png')
Save-Png -Bitmap $windowsPng -Path (Join-Path $assets 'icon.png')

$macosPng = New-IconBitmap -Size 1024 -Inset 0.08
Save-Png -Bitmap $macosPng -Path (Join-Path $assets 'icon-macos.png')

$size32 = New-IconBitmap -Size 32
Save-Png -Bitmap $size32 -Path (Join-Path $icons '32x32.png')
$size128 = New-IconBitmap -Size 128
Save-Png -Bitmap $size128 -Path (Join-Path $icons '128x128.png')
Save-Png -Bitmap $size128 -Path (Join-Path $icons '128x128@2x.png')

# ICO 容器：多尺寸 PNG 负载（Vista+ 与 tauri-build 均接受）。
$sizes = @(16, 20, 24, 32, 40, 48, 64, 128, 256)
$payloads = @()
foreach ($size in $sizes) {
  $bitmap = New-IconBitmap -Size $size
  $stream = New-Object System.IO.MemoryStream
  $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
  $payloads += , @{ Size = $size; Bytes = $stream.ToArray() }
  $stream.Dispose()
  $bitmap.Dispose()
}

$icoPath = Join-Path $icons 'icon.ico'
$stream = New-Object System.IO.FileStream $icoPath, ([System.IO.FileMode]::Create)
$writer = New-Object System.IO.BinaryWriter $stream
$writer.Write([uint16]0)
$writer.Write([uint16]1)
$writer.Write([uint16]$payloads.Count)
$offset = 6 + 16 * $payloads.Count
foreach ($entry in $payloads) {
  $dimension = if ($entry.Size -ge 256) { 0 } else { $entry.Size }
  $writer.Write([byte]$dimension)
  $writer.Write([byte]$dimension)
  $writer.Write([byte]0)
  $writer.Write([byte]0)
  $writer.Write([uint16]1)
  $writer.Write([uint16]32)
  $writer.Write([uint32]$entry.Bytes.Length)
  $writer.Write([uint32]$offset)
  $offset += $entry.Bytes.Length
}
foreach ($entry in $payloads) { $writer.Write($entry.Bytes) }
$writer.Flush()
$writer.Dispose()
$stream.Dispose()

$windowsPng.Dispose()
$macosPng.Dispose()
$size32.Dispose()
$size128.Dispose()

Get-ChildItem $assets, $icons -Filter '*.png' | Select-Object Name, Length
Get-Item $icoPath | Select-Object Name, Length
