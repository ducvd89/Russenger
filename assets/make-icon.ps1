# Draws the Russenger icon (chat bubble with an "R" on a violet-blue gradient)
# and writes assets\icon.png (256), assets\icon-512.png and a multi-size assets\icon.ico.
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'
$out = $PSScriptRoot

function New-RoundedPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function Draw-Icon([int]$size) {
    $S = 1024
    $bmp = New-Object System.Drawing.Bitmap $S, $S, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.TextRenderingHint = 'AntiAliasGridFit'
    $g.Clear([System.Drawing.Color]::Transparent)

    # background: rounded square with a diagonal gradient
    $bg = New-RoundedPath 0 0 $S $S 232
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush ((New-Object System.Drawing.Point 0, 0), (New-Object System.Drawing.Point $S, $S), ([System.Drawing.Color]::FromArgb(255, 64, 160, 255)), ([System.Drawing.Color]::FromArgb(255, 8, 102, 255)))
    $g.FillPath($grad, $bg)

    # soft highlight on the top edge
    $hi = New-Object System.Drawing.Drawing2D.LinearGradientBrush ((New-Object System.Drawing.Point 0, 0), (New-Object System.Drawing.Point 0, 520), ([System.Drawing.Color]::FromArgb(70, 255, 255, 255)), ([System.Drawing.Color]::FromArgb(0, 255, 255, 255)))
    $g.SetClip($bg)
    $g.FillRectangle($hi, 0, 0, $S, 520)
    $g.ResetClip()

    # speech bubble: rounded rect + tail at the bottom-left
    $bubble = New-RoundedPath 190 180 644 520 170
    $tail = New-Object System.Drawing.Drawing2D.GraphicsPath
    $tail.AddPolygon(@((New-Object System.Drawing.PointF 290, 620), (New-Object System.Drawing.PointF 250, 835), (New-Object System.Drawing.PointF 500, 690)))
    $shape = New-Object System.Drawing.Drawing2D.GraphicsPath
    $shape.AddPath($bubble, $false)
    $shape.AddPath($tail, $false)

    # drop shadow
    $m = New-Object System.Drawing.Drawing2D.Matrix
    $m.Translate(0, 22)
    $sh = $shape.Clone(); $sh.Transform($m)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(60, 0, 40, 140))), $sh)
    $g.FillPath([System.Drawing.Brushes]::White, $bubble)
    $g.FillPath([System.Drawing.Brushes]::White, $tail)

    # the letter R, filled with the background gradient
    $rBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush ((New-Object System.Drawing.Point 300, 200), (New-Object System.Drawing.Point 720, 700), ([System.Drawing.Color]::FromArgb(255, 24, 119, 242)), ([System.Drawing.Color]::FromArgb(255, 8, 90, 230)))
    $font = New-Object System.Drawing.Font 'Segoe UI Black', 400, ([System.Drawing.FontStyle]::Regular), ([System.Drawing.GraphicsUnit]::Pixel)
    $sf = New-Object System.Drawing.StringFormat
    $sf.Alignment = 'Center'; $sf.LineAlignment = 'Center'
    $g.DrawString('R', $font, $rBrush, (New-Object System.Drawing.RectangleF 190, 205, 644, 470), $sf)
    $g.Dispose()

    $res = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g2 = [System.Drawing.Graphics]::FromImage($res)
    $g2.InterpolationMode = 'HighQualityBicubic'; $g2.SmoothingMode = 'HighQuality'; $g2.PixelOffsetMode = 'HighQuality'
    $g2.DrawImage($bmp, 0, 0, $size, $size)
    $g2.Dispose(); $bmp.Dispose()
    return $res
}

function Get-PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return $ms.ToArray()
}

(Draw-Icon 512).Save((Join-Path $out 'icon-512.png'), [System.Drawing.Imaging.ImageFormat]::Png)
(Draw-Icon 256).Save((Join-Path $out 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png)

# multi-size .ico with PNG-compressed entries
$sizes = 16, 24, 32, 48, 64, 128, 256
$pngs = New-Object 'System.Collections.Generic.List[byte[]]'
foreach ($s in $sizes) { $pngs.Add([byte[]](Get-PngBytes (Draw-Icon $s))) }
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]$pngs[$i].Length); $bw.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $bw.Write($p) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $out 'icon.ico'), $ms.ToArray())
Write-Host "icon written to $out"
