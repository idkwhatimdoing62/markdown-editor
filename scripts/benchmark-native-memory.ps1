param(
    [string]$OutputDirectory = "artifacts/performance/native-memory",
    [int]$DurationSeconds = 4,
    [int]$SampleIntervalMilliseconds = 75
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $repo $OutputDirectory
$reportPath = Join-Path $outputRoot "latest.json"
New-Item -ItemType Directory -Force -Path $outputRoot | Out-Null

function Get-ProcessTreeSample([int]$RootProcessId) {
    $all = @(Get-CimInstance Win32_Process |
        Select-Object ProcessId, ParentProcessId, WorkingSetSize, PrivatePageCount, Name)
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($RootProcessId)
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($processInfo in $all) {
            if ($ids.Contains([int]$processInfo.ParentProcessId) -and
                $ids.Add([int]$processInfo.ProcessId)) {
                $changed = $true
            }
        }
    }
    $tree = @($all | Where-Object { $ids.Contains([int]$_.ProcessId) })
    [pscustomobject]@{
        WorkingSetMiB = [double](($tree | Measure-Object WorkingSetSize -Sum).Sum) / 1MB
        PrivateMiB = [double](($tree | Measure-Object PrivatePageCount -Sum).Sum) / 1MB
        ProcessCount = $tree.Count
    }
}

Push-Location $repo
try {
    cargo build --release --bin markdown-editor
    if ($LASTEXITCODE -ne 0) { throw "Failed to build the native memory benchmark binary" }
    $appPath = Join-Path $repo "target/release/markdown-editor.exe"
    $appProcess = Start-Process -FilePath $appPath `
        -ArgumentList @("--new-window") `
        -WindowStyle Hidden `
        -PassThru
    $samples = [System.Collections.Generic.List[object]]::new()
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        while (!$appProcess.HasExited -and $watch.Elapsed.TotalSeconds -lt $DurationSeconds) {
            $samples.Add((Get-ProcessTreeSample $appProcess.Id))
            Start-Sleep -Milliseconds $SampleIntervalMilliseconds
            $appProcess.Refresh()
        }
        if ($appProcess.HasExited) { throw "Native benchmark application exited early" }
    }
    finally {
        if (!$appProcess.HasExited) {
            Stop-Process -Id $appProcess.Id -Force -ErrorAction SilentlyContinue
            $appProcess.WaitForExit(5000) | Out-Null
        }
    }

    $steady = @($samples | Select-Object -Last ([math]::Min(10, $samples.Count)))
    $report = [pscustomobject]@{
        SchemaVersion = 1
        GeneratedAt = [DateTimeOffset]::Now.ToString("o")
        GitCommit = (git rev-parse HEAD).Trim()
        ExecutableMiB = [math]::Round(([System.IO.FileInfo]$appPath).Length / 1MB, 3)
        PeakWorkingSetMiB = [math]::Round(($samples | Measure-Object WorkingSetMiB -Maximum).Maximum, 3)
        PeakPrivateMiB = [math]::Round(($samples | Measure-Object PrivateMiB -Maximum).Maximum, 3)
        SteadyWorkingSetMiB = [math]::Round(($steady | Measure-Object WorkingSetMiB -Average).Average, 3)
        SteadyPrivateMiB = [math]::Round(($steady | Measure-Object PrivateMiB -Average).Average, 3)
        MaxProcessCount = ($samples | Measure-Object ProcessCount -Maximum).Maximum
        Samples = $samples.Count
    }
    $report | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 -LiteralPath $reportPath
    $report | Format-List
}
finally {
    Pop-Location
}
