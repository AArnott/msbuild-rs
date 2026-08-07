param(
    [Parameter(Mandatory = $true)]
    [string]$Project,
    [ValidateRange(1, 10000)]
    [int]$Iterations = 20,
    [ValidateRange(0, 1000)]
    [int]$Warmup = 3,
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results"),
    [switch]$CompareOutput,
    [switch]$FailOnMismatch
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($RustExecutable)) {
    $executableName = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        "msbuild-rs.exe"
    } else {
        "msbuild-rs"
    }
    $RustExecutable = Join-Path $PSScriptRoot "..\target\release\$executableName"
}
$projectPath = (Resolve-Path $Project).Path
$rustPath = (Resolve-Path $RustExecutable -ErrorAction Stop).Path
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
[System.IO.Directory]::CreateDirectory($outputPath) | Out-Null
$dotnetSdkVersion = (& dotnet --version).Trim()
$sdkPath = (& dotnet msbuild $projectPath -nologo -getProperty:MSBuildSDKsPath).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query MSBuildSDKsPath (exit code $LASTEXITCODE)"
}
Write-Host ".NET SDK: $dotnetSdkVersion"
Write-Host "MSBuildSDKsPath: $sdkPath"

$dotnetOutput = Join-Path $outputPath "dotnet-preprocessed.xml"
$rustOutput = Join-Path $outputPath "rust-preprocessed.xml"

function Normalize-PreprocessedOutput {
    param(
        [string]$InputPath,
        [string]$NormalizedPath,
        [string]$ProjectDirectory,
        [string]$SdkPath
    )

    $content = [System.IO.File]::ReadAllText($InputPath)
    $content = $content.TrimStart([char]0xFEFF).Replace("`r`n", "`n").Replace("`r", "`n")
    foreach ($replacement in @(
            @{ Path = $ProjectDirectory; Token = "<PROJECT_DIRECTORY>" },
            @{ Path = $SdkPath; Token = "<MSBUILD_SDKS_PATH>" }
        )) {
        if (-not [string]::IsNullOrWhiteSpace($replacement.Path)) {
            $content = [regex]::Replace(
                $content,
                [regex]::Escape($replacement.Path),
                $replacement.Token,
                [System.Text.RegularExpressions.RegexOptions]::IgnoreCase)
        }
    }
    [System.IO.File]::WriteAllText($NormalizedPath, $content.Replace("`n", [Environment]::NewLine))
    return $content
}

function Write-PreprocessMismatchDiagnostic {
    param(
        [string]$Expected,
        [string]$Actual,
        [string]$Path
    )

    $expectedLines = $Expected -split "`n"
    $actualLines = $Actual -split "`n"
    $lineCount = [Math]::Max($expectedLines.Count, $actualLines.Count)
    $firstDifference = 0
    for ($index = 0; $index -lt $lineCount; $index++) {
        $expectedLine = if ($index -lt $expectedLines.Count) { $expectedLines[$index] } else { "<end of file>" }
        $actualLine = if ($index -lt $actualLines.Count) { $actualLines[$index] } else { "<end of file>" }
        if ($expectedLine -cne $actualLine) {
            $firstDifference = $index
            break
        }
    }

    $start = [Math]::Max(0, $firstDifference - 2)
    $end = [Math]::Min($lineCount - 1, $firstDifference + 2)
    $diagnostic = for ($index = $start; $index -le $end; $index++) {
        $expectedLine = if ($index -lt $expectedLines.Count) { $expectedLines[$index] } else { "<end of file>" }
        $actualLine = if ($index -lt $actualLines.Count) { $actualLines[$index] } else { "<end of file>" }
        "line $($index + 1):`n  dotnet: $expectedLine`n  rust:   $actualLine"
    }
    $diagnostic | Set-Content -Path $Path -Encoding utf8
    return "Preprocessed output differs at line $($firstDifference + 1). See $Path"
}

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

if ($CompareOutput) {
    $projectDirectory = Split-Path -Parent $projectPath
    $dotnetNormalizedPath = Join-Path $outputPath "dotnet-preprocessed.normalized.xml"
    $rustNormalizedPath = Join-Path $outputPath "rust-preprocessed.normalized.xml"
    $dotnetNormalized = Normalize-PreprocessedOutput $dotnetOutput $dotnetNormalizedPath $projectDirectory $sdkPath
    $rustNormalized = Normalize-PreprocessedOutput $rustOutput $rustNormalizedPath $projectDirectory $sdkPath

    if ($dotnetNormalized -ceq $rustNormalized) {
        Write-Host "Normalized preprocess parity passed."
    } else {
        $diagnosticPath = Join-Path $outputPath "preprocess-mismatch.txt"
        $message = Write-PreprocessMismatchDiagnostic $dotnetNormalized $rustNormalized $diagnosticPath
        Write-Warning $message
        if ($FailOnMismatch) {
            throw $message
        }
    }
    Write-Host "Raw outputs: $dotnetOutput, $rustOutput"
    Write-Host "Normalized outputs: $dotnetNormalizedPath, $rustNormalizedPath"
}
