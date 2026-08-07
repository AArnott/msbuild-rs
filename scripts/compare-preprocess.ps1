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

$samples = for ($index = 1; $index -le $Iterations; $index++) {
    [pscustomobject]@{
        Iteration = $index
        DotnetMs = Invoke-TimedCommand "dotnet build" $dotnetCommand
        RustMs = Invoke-TimedCommand "msbuild-rs" $rustCommand
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
    } else {
        $sorted[$middle]
    }

    return [pscustomobject]@{
        MeanMs = [Math]::Round(($Values | Measure-Object -Average).Average, 3)
        MedianMs = [Math]::Round($median, 3)
        MinMs = [Math]::Round(($Values | Measure-Object -Minimum).Minimum, 3)
        MaxMs = [Math]::Round(($Values | Measure-Object -Maximum).Maximum, 3)
    }
}

$dotnetSummary = Get-Summary @($samples.DotnetMs)
$rustSummary = Get-Summary @($samples.RustMs)
$summary = @(
    [pscustomobject]@{ Implementation = "dotnet build /pp"; MeanMs = $dotnetSummary.MeanMs; MedianMs = $dotnetSummary.MedianMs; MinMs = $dotnetSummary.MinMs; MaxMs = $dotnetSummary.MaxMs }
    [pscustomobject]@{ Implementation = "msbuild-rs --preprocess"; MeanMs = $rustSummary.MeanMs; MedianMs = $rustSummary.MedianMs; MinMs = $rustSummary.MinMs; MaxMs = $rustSummary.MaxMs }
)

$summary | Format-Table -AutoSize
Write-Host "Raw samples: $samplesPath"