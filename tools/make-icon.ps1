Add-Type -AssemblyName System.Drawing

$outDir = Join-Path (Split-Path $PSScriptRoot -Parent) "assets"
New-Item -ItemType Directory -Force $outDir | Out-Null

function New-ShieldPng([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    $pad = [double]$size * 0.07
    $w = [double]$size - (2 * $pad)
    $h = [double]$size - (2 * $pad)
    $x = $pad
    $y = $pad

    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.StartFigure()
    $path.AddLine($x + 0.50 * $w, $y + 0.03 * $h, $x + 0.94 * $w, $y + 0.17 * $h)
    $path.AddLine($x + 0.94 * $w, $y + 0.48 * $h, $x + 0.94 * $w, $y + 0.48 * $h)
    $path.AddBezier(
        $x + 0.94 * $w, $y + 0.48 * $h,
        $x + 0.94 * $w, $y + 0.74 * $h,
        $x + 0.75 * $w, $y + 0.92 * $h,
        $x + 0.50 * $w, $y + 0.98 * $h)
    $path.AddBezier(
        $x + 0.50 * $w, $y + 0.98 * $h,
        $x + 0.25 * $w, $y + 0.92 * $h,
        $x + 0.06 * $w, $y + 0.74 * $h,
        $x + 0.06 * $w, $y + 0.48 * $h)
    $path.AddLine($x + 0.06 * $w, $y + 0.17 * $h, $x + 0.50 * $w, $y + 0.03 * $h)
    $path.CloseFigure()

    $rect = New-Object System.Drawing.RectangleF($x, $y, $w, $h)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $rect,
        [System.Drawing.Color]::FromArgb(255, 76, 158, 255),
        [System.Drawing.Color]::FromArgb(255, 0, 86, 208),
        90.0)
    $g.FillPath($brush, $path)

    $innerBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(22, 255, 255, 255))
    $g.FillPath($innerBrush, $path)

    $outlineWidth = [Math]::Max(1.0, [double]$size * 0.035)
    $outline = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(170, 255, 255, 255), $outlineWidth)
    $outline.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $g.DrawPath($outline, $path)

    if ($size -ge 24) {
        $checkWidth = [Math]::Max(1.5, [double]$size * 0.085)
        $check = New-Object System.Drawing.Pen([System.Drawing.Color]::White, $checkWidth)
        $check.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $check.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $check.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
        $g.DrawLines($check, [System.Drawing.PointF[]]@(
            [System.Drawing.PointF]::new($x + 0.29 * $w, $y + 0.52 * $h),
            [System.Drawing.PointF]::new($x + 0.44 * $w, $y + 0.67 * $h),
            [System.Drawing.PointF]::new($x + 0.72 * $w, $y + 0.35 * $h)
        ))
        $check.Dispose()
    }

    $outline.Dispose()
    $innerBrush.Dispose()
    $brush.Dispose()
    $path.Dispose()

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Dispose()
    $g.Dispose()
    $bmp.Dispose()
    return ,$bytes
}

$sizes = @(16, 20, 24, 32, 40, 48, 64, 128, 256)
$images = @()
foreach ($s in $sizes) {
    $images += [pscustomobject]@{ Size = $s; Bytes = (New-ShieldPng $s) }
}

$ico = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ico)
$bw.Write([UInt16]0)
$bw.Write([UInt16]1)
$bw.Write([UInt16]$images.Count)
$offset = 6 + (16 * $images.Count)
foreach ($img in $images) {
    $dim = if ($img.Size -ge 256) { 0 } else { $img.Size }
    $bw.Write([byte]$dim)
    $bw.Write([byte]$dim)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([UInt16]1)
    $bw.Write([UInt16]32)
    $bw.Write([UInt32]$img.Bytes.Length)
    $bw.Write([UInt32]$offset)
    $offset += $img.Bytes.Length
}
foreach ($img in $images) {
    $bw.Write([byte[]]$img.Bytes)
}
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $outDir "vtdrop.ico"), $ico.ToArray())
$bw.Dispose()
$ico.Dispose()
Write-Host "ICO: $((Get-Item (Join-Path $outDir 'vtdrop.ico')).Length) bytes"
