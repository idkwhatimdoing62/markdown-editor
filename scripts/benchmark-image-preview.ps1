param(
    [string]$OutputDirectory = "artifacts/performance/image-preview",
    [int]$ImageCount = 20,
    [int]$ImageWidth = 1600,
    [int]$ImageHeight = 1200,
    [int]$TimeoutSeconds = 90
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $repo $OutputDirectory
$imageDirectory = Join-Path $outputRoot "images"
$markdownPath = Join-Path $outputRoot "image-preview.md"
$readyPath = Join-Path $outputRoot "ready.json"
$reportPath = Join-Path $outputRoot "latest.json"

New-Item -ItemType Directory -Force -Path $imageDirectory | Out-Null
Add-Type -AssemblyName System.Drawing

$markdown = [System.Text.StringBuilder]::new()
[void]$markdown.AppendLine("# 图片密集型预览基准")
[void]$markdown.AppendLine()
[void]$markdown.AppendLine("用于测量离屏本地图片对 WebView 首屏和内存的影响。")
for ($index = 1; $index -le $ImageCount; $index++) {
    $fileName = "image-{0:D2}.png" -f $index
    $imagePath = Join-Path $imageDirectory $fileName
    $bitmap = [System.Drawing.Bitmap]::new($ImageWidth, $ImageHeight)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $color = [System.Drawing.Color]::FromArgb(
            255,
            (($index * 31) % 200) + 30,
            (($index * 47) % 200) + 30,
            (($index * 67) % 200) + 30
        )
        $graphics.Clear($color)
        $font = [System.Drawing.Font]::new("Arial", 96)
        try {
            $graphics.DrawString("Image $index", $font, [System.Drawing.Brushes]::White, 100, 100)
        }
        finally {
            $font.Dispose()
        }
        [System.IO.File]::Delete($imagePath)
        $bitmap.Save($imagePath, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
    [void]$markdown.AppendLine()
    [void]$markdown.AppendLine("![测试图片 $index](images/$fileName)")
}
[void]$markdown.AppendLine()
[void]$markdown.AppendLine("## 文档末尾")
[System.IO.File]::WriteAllText($markdownPath, $markdown.ToString(), [System.Text.Encoding]::UTF8)
[System.IO.File]::Delete($readyPath)

function Get-ProcessTreeSample([int]$RootProcessId) {
    $all = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, WorkingSetSize, PrivatePageCount, Name)
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($RootProcessId)
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($processInfo in $all) {
            if ($ids.Contains([int]$processInfo.ParentProcessId) -and $ids.Add([int]$processInfo.ProcessId)) {
                $changed = $true
            }
        }
    }
    $tree = @($all | Where-Object { $ids.Contains([int]$_.ProcessId) })
    [pscustomobject]@{
        WorkingSetMiB = [double](($tree | Measure-Object WorkingSetSize -Sum).Sum) / 1MB
        PrivateMiB = [double](($tree | Measure-Object PrivatePageCount -Sum).Sum) / 1MB
        ProcessCount = $tree.Count
        WebViewProcessCount = @($tree | Where-Object { $_.Name -like "msedgewebview2*" }).Count
    }
}

Push-Location $repo
try {
    cargo build --release --bin markdown-editor
    if ($LASTEXITCODE -ne 0) { throw "构建 WebView 图片基准程序失败" }
    $appPath = Join-Path $repo "target/release/markdown-editor.exe"
    $appProcess = Start-Process -FilePath $appPath `
        -ArgumentList @($markdownPath, "--benchmark-webview-report", $readyPath) `
        -WindowStyle Hidden `
        -PassThru
    $samples = [System.Collections.Generic.List[object]]::new()
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        while (!$appProcess.HasExited -and !(Test-Path -LiteralPath $readyPath)) {
            $samples.Add((Get-ProcessTreeSample $appProcess.Id))
            if ($watch.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
                throw "图片 WebView 基准超时"
            }
            Start-Sleep -Milliseconds 75
            $appProcess.Refresh()
        }
        if (!(Test-Path -LiteralPath $readyPath)) {
            throw "应用退出前没有生成图片 WebView 报告"
        }
        1..12 | ForEach-Object {
            if (!$appProcess.HasExited) {
                $samples.Add((Get-ProcessTreeSample $appProcess.Id))
                Start-Sleep -Milliseconds 75
                $appProcess.Refresh()
            }
        }
    }
    finally {
        if (!$appProcess.HasExited) {
            Stop-Process -Id $appProcess.Id -Force -ErrorAction SilentlyContinue
            $appProcess.WaitForExit(5000) | Out-Null
        }
    }

    $ready = Get-Content -Raw -Encoding utf8 -LiteralPath $readyPath | ConvertFrom-Json
    if ($ready.error) { throw "WebView 图片基准失败：$($ready.error)" }
    $report = [pscustomobject]@{
        SchemaVersion = 1
        GeneratedAt = [DateTimeOffset]::Now.ToString("o")
        GitCommit = (git rev-parse HEAD).Trim()
        ImageCount = $ImageCount
        ImageWidth = $ImageWidth
        ImageHeight = $ImageHeight
        ReadyMs = [math]::Round([double]$ready.startup_to_webview_ready_ms, 3)
        ImageRequestsBeforeReady = [int]$ready.local_image_requests_before_ready
        PeakWorkingSetMiB = [math]::Round(($samples | Measure-Object WorkingSetMiB -Maximum).Maximum, 3)
        PeakPrivateMiB = [math]::Round(($samples | Measure-Object PrivateMiB -Maximum).Maximum, 3)
        MaxProcessCount = ($samples | Measure-Object ProcessCount -Maximum).Maximum
        MaxWebViewProcessCount = ($samples | Measure-Object WebViewProcessCount -Maximum).Maximum
        Samples = $samples.Count
    }
    $report | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 -LiteralPath $reportPath
    $report | Format-List
}
finally {
    Pop-Location
}
