param(
    [Parameter(Mandatory = $true)]
    [string]$Project,
    [ValidateRange(1, 10000)]
    [int]$Iterations = 20,
    [ValidateRange(0, 1000)]
    [int]$Warmup = 3,
    [string]$RustExecutable = (Join-Path $PSScriptRoot "..\target\release\msbuild-rs.exe"),
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results")
)

$ErrorActionPreference = "Stop"
$projectPath = (Resolve-Path $Project).Path
$rustPath = (Resolve-Path $RustExecutable -ErrorAction Stop).Path
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
[System.IO.Directory]::CreateDirectory($outputPath) | Out-Null

$dotnetOutput = Join-Path $outputPath "dotnet-preprocessed.xml"
$rustOutput = Join-Path $outputPath "rust-preprocessed.xml"

function Invoke-TimedCommand {
    param(
        [string]$Name,
        [scriptblock]$Command
    )

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    & $Command *> $null
    $exitCode = $LASTEXITCODE
    $stopwatch.Stop()
    if ($exitCode -ne 0) {
        throw "$Name exited with code $exitCode"
    }
    return $stopwatch.Elapsed.TotalMilliseconds
}

$dotnetCommand = { dotnet build $projectPath "/pp:$dotnetOutput" --nologo }
$rustCommand = { & $rustPath --project $projectPath --preprocess $rustOutput }

Write-Host "Warming up each implementation $Warmup time(s)..."
for ($index = 0; $index -lt $Warmup; $index++) {
    Invoke-TimedCommand "dotnet build" $dotnetCommand | Out-Null
    Invoke-TimedCommand "msbuild-rs" $rustCommand | Out-Null
}

$dotnetSamples = for ($index = 1; $index -le $Iterations; $index++) {
    [pscustomobject]@{
        Iteration = $index
        DotnetMs  = Invoke-TimedCommand "dotnet build" $dotnetCommand
    }
}
$rustSamples = for ($index = 1; $index -le $Iterations; $index++) {
    [pscustomobject]@{
        Iteration = $index
        RustMs    = Invoke-TimedCommand "msbuild-rs" $rustCommand
    }
}
$samples = for ($index = 0; $index -lt $Iterations; $index++) {
    [pscustomobject]@{
        Iteration = $index + 1
        DotnetMs  = $dotnetSamples[$index].DotnetMs
        RustMs    = $rustSamples[$index].RustMs
    }
}

$samplesPath = Join-Path $outputPath "samples.csv"
$samples | Export-Csv -NoTypeInformation $samplesPath

function Get-Summary {
    param([double[]]$Values)

    $sorted = $Values | Sort-Object
    $middle = [int][Math]::Floor($sorted.Count / 2)
    $median = if ($sorted.Count % 2 -eq 0) {
        ($sorted[$middle - 1] + $sorted[$middle]) / 2
    }
    else {
        $sorted[$middle]
    }
    $p95Index = [Math]::Min($sorted.Count - 1, [int][Math]::Ceiling($sorted.Count * 0.95) - 1)
    $deviations = @($Values | ForEach-Object { [Math]::Abs($_ - $median) } | Sort-Object)
    $deviationMiddle = [int][Math]::Floor($deviations.Count / 2)
    $medianAbsoluteDeviation = if ($deviations.Count % 2 -eq 0) {
        ($deviations[$deviationMiddle - 1] + $deviations[$deviationMiddle]) / 2
    }
    else {
        $deviations[$deviationMiddle]
    }
    $outlierThreshold = $median + [Math]::Max(1, 6 * $medianAbsoluteDeviation)

    return [pscustomobject]@{
        MeanMs       = [Math]::Round(($Values | Measure-Object -Average).Average, 3)
        MedianMs     = [Math]::Round($median, 3)
        P95Ms        = [Math]::Round($sorted[$p95Index], 3)
        MinMs        = [Math]::Round(($Values | Measure-Object -Minimum).Minimum, 3)
        MaxMs        = [Math]::Round(($Values | Measure-Object -Maximum).Maximum, 3)
        HighOutliers = @($Values | Where-Object { $_ -gt $outlierThreshold }).Count
    }
}

$dotnetSummary = Get-Summary @($samples.DotnetMs)
$rustSummary = Get-Summary @($samples.RustMs)
$summary = @(
    [pscustomobject]@{ Implementation = "dotnet build /pp"; MeanMs = $dotnetSummary.MeanMs; MedianMs = $dotnetSummary.MedianMs; P95Ms = $dotnetSummary.P95Ms; MinMs = $dotnetSummary.MinMs; MaxMs = $dotnetSummary.MaxMs; HighOutliers = $dotnetSummary.HighOutliers }
    [pscustomobject]@{ Implementation = "msbuild-rs --preprocess"; MeanMs = $rustSummary.MeanMs; MedianMs = $rustSummary.MedianMs; P95Ms = $rustSummary.P95Ms; MinMs = $rustSummary.MinMs; MaxMs = $rustSummary.MaxMs; HighOutliers = $rustSummary.HighOutliers }
)

$summary | Format-Table -AutoSize
Write-Host ("Median speed ratio (dotnet / rust): {0:N2}x" -f ($dotnetSummary.MedianMs / $rustSummary.MedianMs))
Write-Host "Raw samples: $samplesPath"
