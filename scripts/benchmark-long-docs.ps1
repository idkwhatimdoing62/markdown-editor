param(
    [string]$OutputDirectory = "artifacts/performance",
    [int]$WebViewTimeoutSeconds = 90,
    [ValidateSet("100-kib", "1-mib", "10-mib")]
    [string[]]$Sizes = @("100-kib", "1-mib", "10-mib"),
    [switch]$EnforceBudgets
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $repo $OutputDirectory
$corpus = Join-Path $outputRoot "corpus"
$coreDirectory = Join-Path $outputRoot "core"
$webViewDirectory = Join-Path $outputRoot "webview"

New-Item -ItemType Directory -Force -Path $corpus, $coreDirectory, $webViewDirectory | Out-Null

Push-Location $repo
try {
    cargo build --release --features benchmark --bin markdown-benchmark
    if ($LASTEXITCODE -ne 0) { throw "构建核心基准程序失败" }
    cargo build --release --bin markdown-editor
    if ($LASTEXITCODE -ne 0) { throw "构建 WebView 基准程序失败" }

    $benchmarkExe = Join-Path $repo "target/release/markdown-benchmark.exe"
    $appExe = Join-Path $repo "target/release/markdown-editor.exe"
    & $benchmarkExe --generate-only --corpus-dir $corpus
    if ($LASTEXITCODE -ne 0) { throw "生成基准文档失败" }

    $cases = @(
        @{ Label = "100-kib"; File = "long-document-100-kib.md" },
        @{ Label = "1-mib"; File = "long-document-1-mib.md" },
        @{ Label = "10-mib"; File = "long-document-10-mib.md" }
    ) | Where-Object { $Sizes -contains $_.Label }
    $budgets = @{
        "100-kib" = @{ ParseP95Ms = 20; BrowserHtmlP95Ms = 30; ExportHtmlP95Ms = 200; WebViewReadyMs = 2000; PeakWorkingSetMiB = 900 }
        "1-mib" = @{ ParseP95Ms = 150; BrowserHtmlP95Ms = 200; ExportHtmlP95Ms = 500; WebViewReadyMs = 3000; PeakWorkingSetMiB = 1300 }
        "10-mib" = @{ ParseP95Ms = 1000; BrowserHtmlP95Ms = 6500; ExportHtmlP95Ms = 3500; WebViewReadyMs = 5000; PeakWorkingSetMiB = 2048 }
    }

    function Get-ProcessTreeSample([int]$RootPid) {
        $all = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, WorkingSetSize, PrivatePageCount, Name)
        $ids = [System.Collections.Generic.HashSet[int]]::new()
        [void]$ids.Add($RootPid)
        $changed = $true
        while ($changed) {
            $changed = $false
            foreach ($process in $all) {
                if ($ids.Contains([int]$process.ParentProcessId) -and $ids.Add([int]$process.ProcessId)) {
                    $changed = $true
                }
            }
        }
        $tree = @($all | Where-Object { $ids.Contains([int]$_.ProcessId) })
        $workingSet = ($tree | Measure-Object -Property WorkingSetSize -Sum).Sum
        $privateBytes = ($tree | Measure-Object -Property PrivatePageCount -Sum).Sum
        [pscustomobject]@{
            TimestampMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            ProcessCount = $tree.Count
            WebViewProcessCount = @($tree | Where-Object { $_.Name -like "msedgewebview2*" }).Count
            WorkingSetMiB = [math]::Round(([double]$workingSet / 1MB), 3)
            PrivateMiB = [math]::Round(([double]$privateBytes / 1MB), 3)
        }
    }

    $results = @()
    foreach ($case in $cases) {
        $label = $case.Label
        $inputFile = Join-Path $corpus $case.File
        $coreFile = Join-Path $coreDirectory "$label.json"
        $readyFile = Join-Path $webViewDirectory "$label-ready.json"
        Remove-Item -LiteralPath $coreFile -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $readyFile -Force -ErrorAction SilentlyContinue

        $process = Start-Process -FilePath $appExe `
            -ArgumentList @($inputFile, "--benchmark-webview-report", $readyFile) `
            -PassThru
        $samples = [System.Collections.Generic.List[object]]::new()
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            while (!$process.HasExited -and !(Test-Path -LiteralPath $readyFile)) {
                $samples.Add((Get-ProcessTreeSample $process.Id))
                if ($watch.Elapsed.TotalSeconds -gt $WebViewTimeoutSeconds) {
                    throw "WebView 基准超时：$label"
                }
                Start-Sleep -Milliseconds 75
                $process.Refresh()
            }
            if (!(Test-Path -LiteralPath $readyFile)) {
                throw "应用退出前没有生成 WebView 报告：$label"
            }
            1..8 | ForEach-Object {
                if (!$process.HasExited) {
                    $samples.Add((Get-ProcessTreeSample $process.Id))
                    Start-Sleep -Milliseconds 75
                    $process.Refresh()
                }
            }
        }
        finally {
            if (!$process.HasExited) {
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
                $process.WaitForExit(5000) | Out-Null
            }
        }

        & $benchmarkExe --size $label --corpus-dir $corpus --output $coreFile | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "核心基准失败：$label" }

        $core = Get-Content -Raw -Encoding utf8 -LiteralPath $coreFile | ConvertFrom-Json
        $ready = Get-Content -Raw -Encoding utf8 -LiteralPath $readyFile | ConvertFrom-Json
        if ($ready.error) { throw "WebView benchmark failed for ${label}: $($ready.error)" }
        $processTreeMemory = [pscustomobject]@{
            PeakWorkingSetMiB = [math]::Round(($samples | Measure-Object WorkingSetMiB -Maximum).Maximum, 3)
            PeakPrivateMiB = [math]::Round(($samples | Measure-Object PrivateMiB -Maximum).Maximum, 3)
            MaxProcessCount = ($samples | Measure-Object ProcessCount -Maximum).Maximum
            MaxWebViewProcessCount = ($samples | Measure-Object WebViewProcessCount -Maximum).Maximum
            Samples = $samples.Count
        }
        $budget = $budgets[$label]
        $checks = [pscustomobject]@{
            ParseP95 = ([double]$core.parse.p95_ms -le $budget.ParseP95Ms)
            BrowserHtmlP95 = ([double]$core.browser_html.p95_ms -le $budget.BrowserHtmlP95Ms)
            ExportHtmlP95 = ([double]$core.export_html.p95_ms -le $budget.ExportHtmlP95Ms)
            WebViewReady = ([double]$ready.startup_to_webview_ready_ms -le $budget.WebViewReadyMs)
            PeakWorkingSet = ([double]$processTreeMemory.PeakWorkingSetMiB -le $budget.PeakWorkingSetMiB)
        }
        $passed = @($checks.psobject.Properties.Value) -notcontains $false
        $results += [pscustomobject]@{
            Size = $label
            Core = $core
            WebView = $ready
            ProcessTreeMemory = $processTreeMemory
            Budget = [pscustomobject]$budget
            Checks = $checks
            Passed = $passed
        }
    }

    $computer = Get-CimInstance Win32_ComputerSystem
    $processor = Get-CimInstance Win32_Processor | Select-Object -First 1
    $os = Get-CimInstance Win32_OperatingSystem
    $final = [pscustomobject]@{
        SchemaVersion = 1
        GeneratedAt = [DateTimeOffset]::Now.ToString("o")
        GitCommit = (git rev-parse HEAD).Trim()
        Machine = [pscustomobject]@{
            OS = $os.Caption
            OSVersion = $os.Version
            CPU = $processor.Name.Trim()
            LogicalProcessors = $computer.NumberOfLogicalProcessors
            MemoryGiB = [math]::Round(([double]$computer.TotalPhysicalMemory / 1GB), 2)
        }
        Results = $results
    }
    $latest = Join-Path $outputRoot "latest.json"
    $final | ConvertTo-Json -Depth 12 | Set-Content -Encoding utf8 -LiteralPath $latest
    Write-Output "基准完成：$latest"
    if ($EnforceBudgets -and @($results | Where-Object { !$_.Passed }).Count -gt 0) {
        throw "Long-document performance budget exceeded; inspect $latest"
    }
}
finally {
    Pop-Location
}
