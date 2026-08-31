param(
    [string]$OutputDirectory = "artifacts/performance/mermaid-preview",
    [int]$TargetKiB = 600,
    [int]$TimeoutSeconds = 90
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $repo $OutputDirectory
$markdownPath = Join-Path $outputRoot "mermaid-at-end.md"
$readyPath = Join-Path $outputRoot "ready.json"
$reportPath = Join-Path $outputRoot "latest.json"

New-Item -ItemType Directory -Force -Path $outputRoot | Out-Null
$markdown = [System.Text.StringBuilder]::new()
[void]$markdown.AppendLine("# Mermaid deferred-loading benchmark")
[void]$markdown.AppendLine()
$section = 1
while ([System.Text.Encoding]::UTF8.GetByteCount($markdown.ToString()) -lt ($TargetKiB * 1KB)) {
    [void]$markdown.AppendLine("## Text section $section")
    [void]$markdown.AppendLine()
    [void]$markdown.AppendLine("This paragraph keeps the Mermaid diagram in an offscreen virtual chunk while measuring startup requests.")
    [void]$markdown.AppendLine()
    $section++
}
[void]$markdown.AppendLine("## Diagram at document end")
[void]$markdown.AppendLine()
[void]$markdown.AppendLine('```mermaid')
[void]$markdown.AppendLine('flowchart LR')
[void]$markdown.AppendLine('    Start --> Deferred --> Ready')
[void]$markdown.AppendLine('```')
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
    if ($LASTEXITCODE -ne 0) { throw "Failed to build the Mermaid WebView benchmark binary" }
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
                throw "Mermaid WebView benchmark timed out"
            }
            Start-Sleep -Milliseconds 75
            $appProcess.Refresh()
        }
        if (!(Test-Path -LiteralPath $readyPath)) {
            throw "The application exited before writing the Mermaid WebView report"
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
    if ($ready.error) { throw "Mermaid WebView benchmark failed: $($ready.error)" }
    $report = [pscustomobject]@{
        SchemaVersion = 1
        GeneratedAt = [DateTimeOffset]::Now.ToString("o")
        GitCommit = (git rev-parse HEAD).Trim()
        SourceKiB = [math]::Round(([System.IO.FileInfo]$markdownPath).Length / 1KB, 3)
        MermaidRuntimeBytes = ([System.IO.FileInfo](Join-Path $repo "assets/mermaid-11.16.0.min.js")).Length
        ReadyMs = [math]::Round([double]$ready.startup_to_webview_ready_ms, 3)
        MermaidRuntimeRequestsBeforeReady = [int]$ready.mermaid_runtime_requests_before_ready
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
