[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Project,
    [ValidateRange(1, 10000)]
    [int]$Iterations = 20,
    [ValidateRange(0, 1000)]
    [int]$Warmup = 3,
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results"),
    [string]$FixtureName,
    [string]$FixtureHash,
    [string]$FixtureManifestPath,
    [string]$FixtureManifestCase,
    [string[]]$FixtureInputFile,
    [Alias("Property")]
    [string[]]$PropertyName,
    [Alias("Item")]
    [string[]]$ItemType,
    [string[]]$ItemMetadata,
    [switch]$ParityOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "path-normalization.ps1")
. (Join-Path $PSScriptRoot "performance-common.ps1")

function Get-ObjectPropertyValue {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Normalize-EvaluationValue {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory = $true)][string]$ProjectDirectory,
        [AllowEmptyString()][string]$SdkPath
    )

    if ($null -eq $Value) {
        return ""
    }
    $normalized = Replace-PathForComparison ([string]$Value) $ProjectDirectory "<PROJECT_DIRECTORY>"
    if (-not [string]::IsNullOrWhiteSpace($SdkPath)) {
        $normalized = Replace-PathForComparison $normalized $SdkPath "<MSBUILD_SDKS_PATH>"
    }
    return $normalized
}

function Write-EvaluationMismatchDiagnostic {
    param(
        [Parameter(Mandatory = $true)][string]$Expected,
        [Parameter(Mandatory = $true)][string]$Actual,
        [Parameter(Mandatory = $true)][string]$Path
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
    $lines = for ($index = $start; $index -le $end; $index++) {
        $expectedLine = if ($index -lt $expectedLines.Count) { $expectedLines[$index] } else { "<end of file>" }
        $actualLine = if ($index -lt $actualLines.Count) { $actualLines[$index] } else { "<end of file>" }
        "line $($index + 1):`n  dotnet: $expectedLine`n  rust:   $actualLine"
    }
    Write-PerformanceUtf8File $Path ([string]::Join("`n", $lines) + "`n")
    return "Evaluation query output differs at line $($firstDifference + 1). See $Path"
}

if ([string]::IsNullOrWhiteSpace($RustExecutable)) {
    $executableName = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        "msbuild-rs.exe"
    }
    else {
        "msbuild-rs"
    }
    $RustExecutable = Join-Path $PSScriptRoot "..\target\release\$executableName"
}

$properties = @($PropertyName | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$itemTypes = @($ItemType | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if ($properties.Count -eq 0 -and $itemTypes.Count -eq 0) {
    throw "Select at least one property or item type for the evaluation query."
}
$metadataByItem = @{}
foreach ($selection in @($ItemMetadata)) {
    if ([string]::IsNullOrWhiteSpace($selection) -or -not $selection.Contains("=")) {
        throw "ItemMetadata entries must use the ItemType=MetadataName form."
    }
    $parts = $selection.Split("=", 2)
    if ([string]::IsNullOrWhiteSpace($parts[0]) -or [string]::IsNullOrWhiteSpace($parts[1])) {
        throw "ItemMetadata entries must use the ItemType=MetadataName form."
    }
    if (-not $metadataByItem.ContainsKey($parts[0])) {
        $metadataByItem[$parts[0]] = [System.Collections.Generic.List[string]]::new()
    }
    $metadataByItem[$parts[0]].Add($parts[1])
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$projectPath = (Resolve-Path -LiteralPath $Project -ErrorAction Stop).Path
$projectDirectory = Split-Path -Parent $projectPath
$rustPath = (Resolve-Path -LiteralPath $RustExecutable -ErrorAction Stop).Path
$dotnetCommandInfo = Get-Command dotnet -CommandType Application -ErrorAction Stop | Select-Object -First 1
$dotnetPath = $dotnetCommandInfo.Source
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
$rawPath = Join-Path $outputPath "raw"
$normalizedPath = Join-Path $outputPath "normalized"
foreach ($path in @($rawPath, $normalizedPath)) {
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Recurse -Force
    }
    [System.IO.Directory]::CreateDirectory($path) | Out-Null
}
foreach ($staleFile in @(
        "samples.csv",
        "summary.csv",
        "summary.json",
        "parity.json",
        "run-metadata.json",
        "evaluation-mismatch.txt"
    )) {
    $stalePath = Join-Path $outputPath $staleFile
    if (Test-Path -LiteralPath $stalePath) {
        Remove-Item -LiteralPath $stalePath -Force
    }
}

if ([string]::IsNullOrWhiteSpace($FixtureName)) {
    $FixtureName = [System.IO.Path]::GetFileNameWithoutExtension($projectPath)
}
$expectedFixtureHash = $FixtureHash
$dotnetSdkVersion = (& $dotnetPath --version).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query dotnet SDK version (exit code $LASTEXITCODE)."
}
$sdkPath = (& $dotnetPath msbuild $projectPath -nologo -getProperty:MSBuildSDKsPath | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query MSBuildSDKsPath (exit code $LASTEXITCODE)."
}

$fixtureIdentityArguments = @{
    ProjectPath = $projectPath
    ManifestPath = $FixtureManifestPath
    ManifestCase = $FixtureManifestCase
    ExplicitInputPaths = $FixtureInputFile
    SdkPath = $sdkPath
    ExpectedHash = $expectedFixtureHash
}
if ([string]::IsNullOrWhiteSpace($FixtureManifestPath) -and @($FixtureInputFile).Count -eq 0) {
    $identityPreprocessPath = Join-Path $rawPath "dotnet-identity-preprocessed.xml"
    $identityResult = Invoke-PerformanceProcess `
        -Executable $dotnetPath `
        -Arguments @("msbuild", $projectPath, "-nologo", "-pp:$identityPreprocessPath") `
        -WorkingDirectory $projectDirectory `
        -StdoutPath (Join-Path $rawPath "dotnet-identity.stdout.txt") `
        -StderrPath (Join-Path $rawPath "dotnet-identity.stderr.txt")
    if ($identityResult.exitCode -ne 0 -or -not (Test-Path -LiteralPath $identityPreprocessPath)) {
        throw "Untimed fixture identity discovery failed with exit code $($identityResult.exitCode)."
    }
    $fixtureIdentityArguments.AggregatePreprocessPath = $identityPreprocessPath
}
$fixtureIdentity = Get-PerformanceFixtureIdentity @fixtureIdentityArguments
$FixtureHash = $fixtureIdentity.fixtureHash

$commitSha = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query the current commit SHA."
}
$repositoryDirty = @(& git -C $repositoryRoot status --porcelain).Count -gt 0
$hostKey = Get-PerformanceHostKey
$hostOs = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
$hostArchitecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$hostCpu = Get-PerformanceHostCpuName
$startedAtUtc = [DateTime]::UtcNow.ToString("o")

$dotnetArguments = @("msbuild", $projectPath, "-nologo")
if ($properties.Count -gt 0) {
    $dotnetProperties = @($properties)
    if ($itemTypes.Count -eq 0 -and $dotnetProperties.Count -eq 1) {
        $dotnetProperties += "__MsbuildRsPerformanceSentinel"
    }
    $dotnetArguments += "-getProperty:$($dotnetProperties -join ',')"
}
if ($itemTypes.Count -gt 0) {
    $dotnetArguments += "-getItem:$($itemTypes -join ',')"
}
$rustArguments = @("--project", $projectPath)
foreach ($property in $properties) {
    $rustArguments += @("--get-property", $property)
}
foreach ($itemTypeName in $itemTypes) {
    $rustArguments += @("--get-item", $itemTypeName)
}
$dotnetCommand = Format-PerformanceCommand $dotnetPath $dotnetArguments
$rustCommand = Format-PerformanceCommand $rustPath $rustArguments

$queryItems = @(
    foreach ($itemTypeName in $itemTypes) {
        [pscustomobject][ordered]@{
            type = $itemTypeName
            metadata = if ($metadataByItem.ContainsKey($itemTypeName)) {
                @($metadataByItem[$itemTypeName])
            }
            else {
                @()
            }
        }
    }
)
$query = [pscustomobject][ordered]@{
    properties = $properties
    items = $queryItems
}

function ConvertTo-NormalizedEvaluation {
    param([Parameter(Mandatory = $true)]$Result)

    $propertiesContainer = Get-ObjectPropertyValue $Result "Properties"
    $itemsContainer = Get-ObjectPropertyValue $Result "Items"
    $normalizedProperties = [ordered]@{}
    foreach ($property in $properties) {
        $normalizedProperties[$property] = Normalize-EvaluationValue `
            (Get-ObjectPropertyValue $propertiesContainer $property) `
            $projectDirectory `
            $sdkPath
    }
    $normalizedItems = [ordered]@{}
    foreach ($itemDefinition in $queryItems) {
        $selected = Get-ObjectPropertyValue $itemsContainer $itemDefinition.type
        $normalizedList = @(
            foreach ($item in @($selected)) {
                $identity = Get-ObjectPropertyValue $item "Identity"
                if ($null -eq $identity) {
                    $identity = Get-ObjectPropertyValue $item "identity"
                }
                $nestedMetadata = Get-ObjectPropertyValue $item "metadata"
                $metadata = [ordered]@{}
                foreach ($metadataName in @($itemDefinition.metadata)) {
                    $value = Get-ObjectPropertyValue $item $metadataName
                    if ($null -ne $nestedMetadata) {
                        $nestedValue = Get-ObjectPropertyValue $nestedMetadata $metadataName
                        if ($null -ne $nestedValue) {
                            $value = $nestedValue
                        }
                    }
                    $metadata[$metadataName] = Normalize-EvaluationValue $value $projectDirectory $sdkPath
                }
                [pscustomobject][ordered]@{
                    identity = Normalize-EvaluationValue $identity $projectDirectory $sdkPath
                    metadata = $metadata
                }
            }
        )
        $normalizedItems[$itemDefinition.type] = $normalizedList
    }
    return [pscustomobject][ordered]@{
        properties = $normalizedProperties
        items = $normalizedItems
    }
}

Write-Host "Fixture: $FixtureName"
Write-Host "Fixture identity: $FixtureHash ($($fixtureIdentity.source), $($fixtureIdentity.inputCount) input file(s))"
Write-Host ".NET SDK: $dotnetSdkVersion"
Write-Host "Establishing selected evaluation-query parity before timing..."

$dotnetParityPath = Join-Path $rawPath "dotnet-parity.stdout.json"
$rustParityPath = Join-Path $rawPath "rust-parity.stdout.json"
$dotnetParity = Invoke-PerformanceProcess `
    -Executable $dotnetPath `
    -Arguments $dotnetArguments `
    -WorkingDirectory $projectDirectory `
    -StdoutPath $dotnetParityPath `
    -StderrPath (Join-Path $rawPath "dotnet-parity.stderr.txt")
$rustParity = Invoke-PerformanceProcess `
    -Executable $rustPath `
    -Arguments $rustArguments `
    -WorkingDirectory $projectDirectory `
    -StdoutPath $rustParityPath `
    -StderrPath (Join-Path $rawPath "rust-parity.stderr.txt")

$parityPassed = $false
$parityMessage = ""
if ($dotnetParity.exitCode -ne 0 -or $rustParity.exitCode -ne 0) {
    $parityMessage = "Evaluation query parity commands failed: dotnet=$($dotnetParity.exitCode), rust=$($rustParity.exitCode)."
}
else {
    try {
        $dotnetResult = [System.IO.File]::ReadAllText($dotnetParityPath) | ConvertFrom-Json
        $rustResult = [System.IO.File]::ReadAllText($rustParityPath) | ConvertFrom-Json
        $dotnetNormalized = ConvertTo-NormalizedEvaluation $dotnetResult
        $rustNormalized = ConvertTo-NormalizedEvaluation $rustResult
        $dotnetNormalizedJson = $dotnetNormalized | ConvertTo-Json -Depth 12
        $rustNormalizedJson = $rustNormalized | ConvertTo-Json -Depth 12
        Write-PerformanceUtf8File (Join-Path $normalizedPath "dotnet-evaluation.json") ($dotnetNormalizedJson + "`n")
        Write-PerformanceUtf8File (Join-Path $normalizedPath "rust-evaluation.json") ($rustNormalizedJson + "`n")
        if ($dotnetNormalizedJson -ceq $rustNormalizedJson) {
            $parityPassed = $true
            $parityMessage = "Selected evaluation-query parity passed."
        }
        else {
            $parityMessage = Write-EvaluationMismatchDiagnostic `
                $dotnetNormalizedJson `
                $rustNormalizedJson `
                (Join-Path $outputPath "evaluation-mismatch.txt")
        }
    }
    catch {
        $parityMessage = "Could not normalize evaluation query output: $($_.Exception.Message)"
    }
}

$parity = [pscustomobject][ordered]@{
    status = if ($parityPassed) { "passed" } else { "failed" }
    message = $parityMessage
    mode = "evaluation-query"
    fixture = $FixtureName
    fixtureHash = $FixtureHash
    query = $query
    dotnet = [ordered]@{
        command = $dotnetCommand
        exitCode = $dotnetParity.exitCode
        elapsedWallMs = [Math]::Round($dotnetParity.elapsedWallMs, 3)
        peakWorkingSetBytes = $dotnetParity.peakWorkingSetBytes
    }
    rust = [ordered]@{
        command = $rustCommand
        exitCode = $rustParity.exitCode
        elapsedWallMs = [Math]::Round($rustParity.elapsedWallMs, 3)
        peakWorkingSetBytes = $rustParity.peakWorkingSetBytes
    }
}
Write-PerformanceJsonFile (Join-Path $outputPath "parity.json") $parity

$runMetadata = [pscustomobject][ordered]@{
    schemaVersion = 1
    startedAtUtc = $startedAtUtc
    mode = "evaluation-query"
    measurementKind = "fresh-process end-to-end evaluation-query"
    fixture = $FixtureName
    fixtureHash = $FixtureHash
    fixtureIdentity = $fixtureIdentity
    project = $projectPath
    query = $query
    commitSha = $commitSha
    repositoryDirty = $repositoryDirty
    host = [ordered]@{
        key = $hostKey
        os = $hostOs
        architecture = $hostArchitecture
        cpu = $hostCpu
    }
    dotnetSdkVersion = $dotnetSdkVersion
    msbuildSdksPath = $sdkPath
    warmupIterations = $Warmup
    measuredIterations = $Iterations
    parityOnly = [bool]$ParityOnly
    parityStatus = $parity.status
    commands = [ordered]@{
        dotnet = $dotnetCommand
        rust = $rustCommand
    }
}
Write-PerformanceJsonFile (Join-Path $outputPath "run-metadata.json") $runMetadata

if (-not $parityPassed) {
    Write-Warning $parityMessage
    throw "Fixture '$FixtureName' is ineligible for evaluation-query timing because parity failed."
}
Write-Host $parityMessage
if ($ParityOnly) {
    Write-Host "Parity-only mode completed without collecting timed samples."
    return
}

$samplesPath = Join-Path $outputPath "samples.csv"
$samples = [System.Collections.Generic.List[object]]::new()
$sequence = 0

function Invoke-EvaluationBenchmarkSample {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("warmup", "measurement")][string]$Phase,
        [Parameter(Mandatory = $true)][int]$Iteration,
        [Parameter(Mandatory = $true)][int]$OrderInIteration,
        [Parameter(Mandatory = $true)][ValidateSet("dotnet-msbuild", "msbuild-rs")][string]$Implementation,
        [Parameter(Mandatory = $true)][int]$Sequence
    )

    if ($Implementation -eq "dotnet-msbuild") {
        $executable = $dotnetPath
        $arguments = $dotnetArguments
        $command = $dotnetCommand
        $slug = "dotnet"
    }
    else {
        $executable = $rustPath
        $arguments = $rustArguments
        $command = $rustCommand
        $slug = "rust"
    }
    $logPrefix = "{0:D4}-{1}-{2:D4}-{3}" -f $Sequence, $Phase, $Iteration, $slug
    $result = Invoke-PerformanceProcess `
        -Executable $executable `
        -Arguments $arguments `
        -WorkingDirectory $projectDirectory `
        -StdoutPath (Join-Path $rawPath "$logPrefix.stdout.json") `
        -StderrPath (Join-Path $rawPath "$logPrefix.stderr.txt")
    return [pscustomobject][ordered]@{
        Sequence = $Sequence
        Phase = $Phase
        Iteration = $Iteration
        OrderInIteration = $OrderInIteration
        Mode = "evaluation-query"
        MeasurementKind = "fresh-process end-to-end evaluation-query"
        Implementation = $Implementation
        ElapsedWallMs = [Math]::Round($result.elapsedWallMs, 3)
        PeakWorkingSetBytes = $result.peakWorkingSetBytes
        PeakWorkingSetMiB = [Math]::Round($result.peakWorkingSetBytes / 1MB, 3)
        ExitCode = $result.exitCode
        ParityStatus = "passed"
        Valid = ($result.exitCode -eq 0)
        Measured = ($Phase -eq "measurement")
        Fixture = $FixtureName
        FixtureHash = $FixtureHash
        CommitSha = $commitSha
        RepositoryDirty = $repositoryDirty
        HostKey = $hostKey
        HostOs = $hostOs
        HostArchitecture = $hostArchitecture
        HostCpu = $hostCpu
        DotnetSdkVersion = $dotnetSdkVersion
        Command = $command
    }
}

function Invoke-EvaluationBenchmarkPhase {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("warmup", "measurement")][string]$Phase,
        [Parameter(Mandatory = $true)][int]$Count
    )

    for ($iteration = 1; $iteration -le $Count; $iteration++) {
        $implementationOrder = if (($iteration % 2) -eq 1) {
            @("dotnet-msbuild", "msbuild-rs")
        }
        else {
            @("msbuild-rs", "dotnet-msbuild")
        }
        for ($order = 0; $order -lt $implementationOrder.Count; $order++) {
            $script:sequence++
            $sample = Invoke-EvaluationBenchmarkSample `
                -Phase $Phase `
                -Iteration $iteration `
                -OrderInIteration ($order + 1) `
                -Implementation $implementationOrder[$order] `
                -Sequence $script:sequence
            $samples.Add($sample) | Out-Null
            $samples | Export-Csv -LiteralPath $samplesPath -NoTypeInformation
            if ($sample.ExitCode -ne 0) {
                throw "$($sample.Implementation) exited with code $($sample.ExitCode) during $Phase iteration $iteration."
            }
        }
    }
}

if ($Warmup -gt 0) {
    Write-Host "Running $Warmup interleaved warmup iteration(s) per implementation..."
    Invoke-EvaluationBenchmarkPhase -Phase warmup -Count $Warmup
}
Write-Host "Running $Iterations interleaved measured iteration(s) per implementation..."
Invoke-EvaluationBenchmarkPhase -Phase measurement -Count $Iterations

$measuredSamples = @($samples | Where-Object { $_.Measured -and $_.Valid -and $_.ParityStatus -eq "passed" })
$implementationSummaries = [System.Collections.Generic.List[object]]::new()
foreach ($implementation in @("dotnet-msbuild", "msbuild-rs")) {
    $implementationSamples = @($measuredSamples | Where-Object { $_.Implementation -eq $implementation })
    if ($implementationSamples.Count -ne $Iterations) {
        throw "Expected $Iterations valid measured samples for $implementation, found $($implementationSamples.Count)."
    }
    $wallValues = [double[]]@($implementationSamples | ForEach-Object { [double]$_.ElapsedWallMs })
    $peakValues = [double[]]@($implementationSamples | ForEach-Object { [double]$_.PeakWorkingSetBytes })
    $wall = Get-PerformanceDistribution $wallValues
    $peakMedian = Get-PerformanceMedian $peakValues
    $peakMaximum = [double](($peakValues | Measure-Object -Maximum).Maximum)
    $implementationSummaries.Add([pscustomobject][ordered]@{
            implementation = $implementation
            command = $implementationSamples[0].Command
            sampleCount = $implementationSamples.Count
            wallTimeMs = $wall
            peakWorkingSet = [ordered]@{
                medianBytes = [long][Math]::Round($peakMedian)
                maxBytes = [long][Math]::Round($peakMaximum)
                medianMiB = [Math]::Round($peakMedian / 1MB, 3)
                maxMiB = [Math]::Round($peakMaximum / 1MB, 3)
            }
        }) | Out-Null
}

$dotnetSummary = $implementationSummaries | Where-Object { $_.implementation -eq "dotnet-msbuild" }
$rustSummary = $implementationSummaries | Where-Object { $_.implementation -eq "msbuild-rs" }
$medianSpeedRatio = [double]$dotnetSummary.wallTimeMs.median / [double]$rustSummary.wallTimeMs.median
$summary = [pscustomobject][ordered]@{
    schemaVersion = 1
    mode = "evaluation-query"
    measurementKind = "fresh-process end-to-end evaluation-query"
    fixture = $FixtureName
    fixtureHash = $FixtureHash
    fixtureIdentity = $fixtureIdentity
    query = $query
    eligible = $true
    parityStatus = "passed"
    commitSha = $commitSha
    repositoryDirty = $repositoryDirty
    host = $runMetadata.host
    dotnetSdkVersion = $dotnetSdkVersion
    warmupIterations = $Warmup
    measuredIterations = $Iterations
    implementations = @($implementationSummaries)
    medianSpeedRatioDotnetOverRust = [Math]::Round($medianSpeedRatio, 4)
}
Write-PerformanceJsonFile (Join-Path $outputPath "summary.json") $summary

$summaryRows = @(
    $implementationSummaries | ForEach-Object {
        [pscustomobject][ordered]@{
            Mode = "evaluation-query"
            MeasurementKind = "fresh-process end-to-end evaluation-query"
            Fixture = $FixtureName
            FixtureHash = $FixtureHash
            Implementation = $_.implementation
            SampleCount = $_.sampleCount
            WallMinMs = $_.wallTimeMs.min
            WallMedianMs = $_.wallTimeMs.median
            WallMeanMs = $_.wallTimeMs.mean
            WallP95Ms = $_.wallTimeMs.p95
            WallMaxMs = $_.wallTimeMs.max
            WallMadMs = $_.wallTimeMs.mad
            PeakWorkingSetMedianBytes = $_.peakWorkingSet.medianBytes
            PeakWorkingSetMaxBytes = $_.peakWorkingSet.maxBytes
            PeakWorkingSetMedianMiB = $_.peakWorkingSet.medianMiB
            PeakWorkingSetMaxMiB = $_.peakWorkingSet.maxMiB
            MedianSpeedRatioDotnetOverRust = [Math]::Round($medianSpeedRatio, 4)
            ParityStatus = "passed"
            CommitSha = $commitSha
            RepositoryDirty = $repositoryDirty
            HostKey = $hostKey
            DotnetSdkVersion = $dotnetSdkVersion
            Command = $_.command
        }
    }
)
$summaryRows | Export-Csv -LiteralPath (Join-Path $outputPath "summary.csv") -NoTypeInformation
$summaryRows |
    Select-Object Implementation, WallMinMs, WallMedianMs, WallMeanMs, WallP95Ms, WallMaxMs, WallMadMs, PeakWorkingSetMedianMiB, PeakWorkingSetMaxMiB |
    Format-Table -AutoSize
Write-Host ("Median speed ratio (dotnet / rust): {0:N2}x" -f $medianSpeedRatio)
Write-Host "Raw samples: $samplesPath"
Write-Host "Summary: $(Join-Path $outputPath "summary.json")"
