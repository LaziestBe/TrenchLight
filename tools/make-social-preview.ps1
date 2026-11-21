# Draws the GitHub social preview (1280x640) from the app icon and the
# Trenchlight palette. Run from the repo root:
#   powershell -ExecutionPolicy Bypass -File tools\make-social-preview.ps1
Add-Type -AssemblyName System.Drawing

$W = 1280
$H = 640
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$iconPath = Join-Path $root 'assets\icon.png'
$outPath = Join-Path $root 'docs\social-preview.png'

# Palette, measured from the launcher.
$abyss0 = [System.Drawing.ColorTranslator]::FromHtml('#04101B')
$abyss1 = [System.Drawing.ColorTranslator]::FromHtml('#071826')
$abyss2 = [System.Drawing.ColorTranslator]::FromHtml('#0B222F')
$ice = [System.Drawing.ColorTranslator]::FromHtml('#F2FDFF')
$ice2 = [System.Drawing.ColorTranslator]::FromHtml('#B6D6DC')
$teal = [System.Drawing.ColorTranslator]::FromHtml('#3AC9CF')
$teal2 = [System.Drawing.ColorTranslator]::FromHtml('#60E9E6')

$bmp = New-Object System.Drawing.Bitmap($W, $H)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic

# Background: vertical abyss gradient.
$rect = New-Object System.Drawing.Rectangle(0, 0, $W, $H)
$bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $abyss0, $abyss2, 90)
$g.FillRectangle($bg, $rect)
$bg.Dispose()

# A soft teal glow behind the mark, so it does not sit on a flat field.
$glowPath = New-Object System.Drawing.Drawing2D.GraphicsPath
$glowPath.AddEllipse(70, 120, 420, 400)
$glow = New-Object System.Drawing.Drawing2D.PathGradientBrush($glowPath)
$glow.CenterColor = [System.Drawing.Color]::FromArgb(70, 58, 201, 207)
$glow.SurroundColors = @([System.Drawing.Color]::FromArgb(0, 58, 201, 207))
$g.FillPath($glow, $glowPath)
$glow.Dispose()
$glowPath.Dispose()

# The mark.
if (Test-Path $iconPath) {
    $icon = [System.Drawing.Image]::FromFile($iconPath)
    $g.DrawImage($icon, 150, 170, 300, 300)
    $icon.Dispose()
}

# Wordmark.
$fTitle = New-Object System.Drawing.Font('Segoe UI Semibold', 76, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$fSub = New-Object System.Drawing.Font('Segoe UI', 27, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$fKick = New-Object System.Drawing.Font('Segoe UI', 17, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)

$white = New-Object System.Drawing.SolidBrush($ice)
$soft = New-Object System.Drawing.SolidBrush($ice2)
$accent = New-Object System.Drawing.SolidBrush($teal2)

# Kicker above the title.
$g.DrawString('MINECRAFT: JAVA EDITION LAUNCHER', $fKick, $accent, 520, 196)
$g.DrawString('Trenchlight', $fTitle, $white, 512, 226)
$g.DrawString('Written in Rust. Isolated instances,', $fSub, $soft, 520, 344)
$g.DrawString('offline accounts, no Microsoft sign-in.', $fSub, $soft, 520, 382)

# Accent rule under the block.
$pen = New-Object System.Drawing.Pen($teal, 3)
$g.DrawLine($pen, 520, 452, 700, 452)
$pen.Dispose()

$fSmall = New-Object System.Drawing.Font('Segoe UI', 19, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$g.DrawString('Windows 10 | 11      MIT licence      github.com/LaziestBe/TrenchLight', $fSmall, $soft, 520, 476)

# Bottom hairline in the brand teal.
$edge = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(90, 58, 201, 207), 2)
$g.DrawLine($edge, 0, $H - 3, $W, $H - 3)
$edge.Dispose()

$dir = Split-Path -Parent $outPath
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)

$g.Dispose(); $bmp.Dispose()
$fTitle.Dispose(); $fSub.Dispose(); $fKick.Dispose(); $fSmall.Dispose()
$white.Dispose(); $soft.Dispose(); $accent.Dispose()

Write-Output "png: $outPath (${W} x ${H})"
