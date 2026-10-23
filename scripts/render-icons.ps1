# 从设计源 assets/icon.png 生成各平台图标与 tauri-build 需要的 ICO/PNG。
# 设计源是 1024×1024 的透明圆角 PNG（圆角已烘焙进 alpha）；本脚本只做缩放与
# macOS 的内缩排版，不重绘、不改色。
# 用法： pwsh -File scripts/render-icons.ps1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root 'assets'
$icons = Join-Path $root 'src-tauri/icons'
$ui = Join-Path $root 'src-ui'
$source = Join-Path $assets 'icon.png'
if (-not (Test-Path $source)) {
  throw "missing design source: $source"
}
New-Item -ItemType Directory -Force -Path $icons, $ui | Out-Null

$design = [System.Drawing.Image]::FromFile($source)

function New-Scaled {
  param([int]$Size)

  $bitmap = New-Object System.Drawing.Bitmap $Size, $Size
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
  $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
  $graphics.Clear([System.Drawing.Color]::Transparent)
  $graphics.DrawImage($design, 0, 0, $Size, $Size)
  $graphics.Dispose()
  return $bitmap
}

function Save-Png {
  param([System.Drawing.Bitmap]$Bitmap, [string]$Path)
  $Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
}

$full = New-Scaled -Size 1024
Save-Png -Bitmap $full -Path (Join-Path $icons 'icon.png')

# macOS 图标的边距约定：内容内缩，四周留透明。
$macos = New-Object System.Drawing.Bitmap 1024, 1024
$graphics = [System.Drawing.Graphics]::FromImage($macos)
$graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$graphics.Clear([System.Drawing.Color]::Transparent)
$graphics.DrawImage($design, 82, 82, 860, 860)
$graphics.Dispose()
Save-Png -Bitmap $macos -Path (Join-Path $assets 'icon-macos.png')

$size32 = New-Scaled -Size 32
Save-Png -Bitmap $size32 -Path (Join-Path $icons '32x32.png')
$size128 = New-Scaled -Size 128
Save-Png -Bitmap $size128 -Path (Join-Path $icons '128x128.png')
$size256 = New-Scaled -Size 256
Save-Png -Bitmap $size256 -Path (Join-Path $icons '128x128@2x.png')

# 标题栏里 16 DIP 的品牌图，按 6× 出图供高分屏使用。
$sizeHeader = New-Scaled -Size 96
Save-Png -Bitmap $sizeHeader -Path (Join-Path $ui 'icon.png')

# ICO 容器：多尺寸 PNG 负载（Vista+ 与 tauri-build 均接受）。
$sizes = @(16, 20, 24, 32, 40, 48, 64, 128, 256)
$payloads = @()
foreach ($size in $sizes) {
  $bitmap = New-Scaled -Size $size
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

$full.Dispose()
$macos.Dispose()
$size32.Dispose()
$size128.Dispose()
$size256.Dispose()
$sizeHeader.Dispose()
$design.Dispose()

Get-ChildItem $assets, $icons, $ui -Filter '*.png' | Select-Object Name, Length
Get-Item $icoPath | Select-Object Name, Length
