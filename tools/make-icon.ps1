# Draw the Trenchlight logo and write every icon form the launcher needs.
#
#   powershell -ExecutionPolicy Bypass -File tools\make-icon.ps1
#
# Outputs:
#   assets\icon.png        1024x1024 master
#   assets\icon.ico        16/24/32/48/64/128/256, PNG-compressed entries
#   assets\icon-64.rgba    64x64 raw RGBA, embedded for the tao window icon
#
# Drawn rather than painted: a flat vector mark stays readable at 16 px, which a
# generated picture does not.

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$assets = Join-Path $root 'assets'
if (-not (Test-Path $assets)) { New-Item -ItemType Directory -Path $assets -Force | Out-Null }

function New-RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function New-Logo([int]$S) {
    $bmp = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    $k = $S / 1024.0
    # Rounded abyss tile.
    $tile = New-RoundRect 0 0 $S $S (190 * $k)
    $rect = New-Object System.Drawing.RectangleF 0, 0, $S, $S
    $bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $rect,
        [System.Drawing.ColorTranslator]::FromHtml('#0A2434'),
        [System.Drawing.ColorTranslator]::FromHtml('#04101B'),
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillPath($bg, $tile)

    # Inner hairline so the tile reads as a window on a light desktop too.
    $penEdge = New-Object System.Drawing.Pen ([System.Drawing.ColorTranslator]::FromHtml('#1B3A46')), (10 * $k)
    $g.DrawPath($penEdge, $tile)

    # Depth gauge: the scale the needle reads against.
    $penGauge = New-Object System.Drawing.Pen ([System.Drawing.ColorTranslator]::FromHtml('#2E7783')), (12 * $k)
    $penGauge.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $penGauge.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawLine($penGauge, 168 * $k, 520 * $k, 856 * $k, 520 * $k)
    $penTick = New-Object System.Drawing.Pen ([System.Drawing.ColorTranslator]::FromHtml('#2E7783')), (9 * $k)
    $penTick.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $penTick.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    foreach ($tx in 196, 316, 436, 556, 676, 796) {
        $g.DrawLine($penTick, $tx * $k, 486 * $k, $tx * $k, 554 * $k)
    }

    # The mark: a T whose stem becomes the plumb that reads the gauge.
    $tealTop = [System.Drawing.ColorTranslator]::FromHtml('#7FF3EE')
    $tealLow = [System.Drawing.ColorTranslator]::FromHtml('#3AC9CF')
    $mark = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.RectangleF 0, (280 * $k), $S, (600 * $k)), $tealTop, $tealLow,
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)

    # Square-bottomed stem so the needle grows out of it without a seam; the
    # top hides under the bar.
    $g.FillRectangle($mark, (452 * $k), (396 * $k), (120 * $k), (264 * $k))
    $bar = New-RoundRect (272 * $k) (288 * $k) (480 * $k) (116 * $k) (30 * $k)
    $g.FillPath($mark, $bar)
    # Taper starts exactly where the stem ends, so there is no seam or ledge.
    $needle = New-Object System.Drawing.Drawing2D.GraphicsPath
    $needle.AddPolygon(@(
        (New-Object System.Drawing.PointF (452 * $k), (660 * $k)),
        (New-Object System.Drawing.PointF (572 * $k), (660 * $k)),
        (New-Object System.Drawing.PointF (512 * $k), (838 * $k))))
    $g.FillPath($mark, $needle)

    # A single ice highlight along the top edge of the bar.
    $penHi = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(150, 242, 253, 255)), (7 * $k)
    $penHi.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $penHi.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawLine($penHi, 310 * $k, 308 * $k, 714 * $k, 308 * $k)

    $g.Dispose()
    return $bmp
}

# Master.
$master = New-Logo 1024
$master.Save((Join-Path $assets 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png)

# ICO with PNG-compressed entries (Windows Vista and newer read these).
$sizes = 16, 24, 32, 48, 64, 128, 256
$pngs = @()
foreach ($s in $sizes) {
    $b = New-Object System.Drawing.Bitmap($s, $s, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $gg = [System.Drawing.Graphics]::FromImage($b)
    $gg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $gg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $gg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $gg.DrawImage($master, (New-Object System.Drawing.Rectangle 0, 0, $s, $s))
    $gg.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs += , @($s, $ms.ToArray())
    $b.Dispose()
}

$icoPath = Join-Path $assets 'icon.ico'
$fs = [System.IO.File]::Create($icoPath)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([UInt16]0)          # reserved
$bw.Write([UInt16]1)          # type: icon
$bw.Write([UInt16]$sizes.Count)
$offset = 6 + (16 * $sizes.Count)
foreach ($entry in $pngs) {
    $s = [int]$entry[0]
    $data = [byte[]]$entry[1]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim)     # width
    $bw.Write([byte]$dim)     # height
    $bw.Write([byte]0)        # palette
    $bw.Write([byte]0)        # reserved
    $bw.Write([UInt16]1)      # planes
    $bw.Write([UInt16]32)     # bits per pixel
    $bw.Write([UInt32]$data.Length)
    $bw.Write([UInt32]$offset)
    $offset += $data.Length
}
foreach ($entry in $pngs) { $bw.Write([byte[]]$entry[1]) }
$bw.Flush(); $bw.Dispose(); $fs.Dispose()

# Raw 64x64 RGBA for tao's window icon (no image crate in the tree).
$w64 = New-Object System.Drawing.Bitmap 64, 64, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$gg = [System.Drawing.Graphics]::FromImage($w64)
$gg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$gg.DrawImage($master, (New-Object System.Drawing.Rectangle 0, 0, 64, 64))
$gg.Dispose()
$bytes = New-Object System.Collections.Generic.List[byte]
for ($y = 0; $y -lt 64; $y++) {
    for ($x = 0; $x -lt 64; $x++) {
        $c = $w64.GetPixel($x, $y)
        $bytes.Add($c.R); $bytes.Add($c.G); $bytes.Add($c.B); $bytes.Add($c.A)
    }
}
[System.IO.File]::WriteAllBytes((Join-Path $assets 'icon-64.rgba'), $bytes.ToArray())
$w64.Dispose()
$master.Dispose()

"icon.png   {0} bytes" -f (Get-Item (Join-Path $assets 'icon.png')).Length
"icon.ico   {0} bytes, {1} sizes" -f (Get-Item $icoPath).Length, $sizes.Count
"icon-64.rgba {0} bytes" -f (Get-Item (Join-Path $assets 'icon-64.rgba')).Length
