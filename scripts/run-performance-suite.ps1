[CmdletBinding()]
param(
    [ValidateSet("Smoke", "Benchmark")]
    [string]$Preset = "Benchmark",
    [ValidateRange(0, 10000)]
    [int]$Iterations = 0,
    [ValidateRange(-1, 1000)]
    [int]$Warmup = -1,
    [ValidateRange(0, 2147483647)]
    [int]$Seed = 1729,
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results\performance"),
    [string[]]$Fixture,
    [ValidateSet("preprocess", "evaluation-query")]
    [string[]]$Mode = @("preprocess", "evaluation-query"),
    [switch]$SkipSimple,
    [ValidateRange(-1, 100000)]
    [int]$PropertyCount = -1,
    [ValidateRange(-1, 100000)]
    [int]$ItemCount = -1,
    [ValidateRange(-1, 100000)]
    [int]$ConditionCount = -1,
    [ValidateRange(-1, 1000)]
    [int]$ImportDepth = -1,
    [ValidateRange(-1, 100)]
    [int]$ImportWidth = -1,
    [ValidateRange(-1, 100000)]
    [int]$GlobFileCount = -1
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Write-SuiteJson {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$Value
    )

    $json = ($Value | ConvertTo-Json -Depth 14).Replace("`r`n", "`n").Replace("`r", "`n") + "`n"
    [System.IO.File]::WriteAllText($Path, $json, $script:Utf8NoBom)
}

function Get-PerformanceHostKey {
    $os = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        "windows"
    }
    elseif ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Linux)) {
        "linux"
    }
    elseif ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::OSX)) {
        "macos"
    }
    else {
        "unknown"
    }
    return "$os-$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant())"
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([string]::IsNullOrWhiteSpace($RustExecutable)) {
    $executableName = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        "msbuild-rs.exe"
    }
    else {
        "msbuild-rs"
    }
    $RustExecutable = Join-Path $repositoryRoot "target\release\$executableName"
}
$rustPath = (Resolve-Path $RustExecutable -ErrorAction Stop).Path

if ($Iterations -eq 0) {
    $Iterations = if ($Preset -eq "Smoke") { 3 } else { 20 }
}
if ($Warmup -lt 0) {
    $Warmup = if ($Preset -eq "Smoke") { 1 } else { 3 }
}

$hostKey = Get-PerformanceHostKey
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)
$hostOutput = Join-Path $outputRoot $hostKey
if (Test-Path -LiteralPath $hostOutput) {
    Remove-Item -LiteralPath $hostOutput -Recurse -Force
}
[System.IO.Directory]::CreateDirectory($hostOutput) | Out-Null
$generatedOutput = Join-Path $hostOutput "generated-fixtures"
$fixtureOutput = Join-Path $hostOutput "fixtures"
[System.IO.Directory]::CreateDirectory($fixtureOutput) | Out-Null

$generatorArguments = @{
    Preset = $Preset
    OutputDirectory = $generatedOutput
    Seed = $Seed
}
foreach ($override in @(
        @{ Name = "PropertyCount"; Value = $PropertyCount },
        @{ Name = "ItemCount"; Value = $ItemCount },
        @{ Name = "ConditionCount"; Value = $ConditionCount },
        @{ Name = "ImportDepth"; Value = $ImportDepth },
        @{ Name = "ImportWidth"; Value = $ImportWidth },
        @{ Name = "GlobFileCount"; Value = $GlobFileCount }
    )) {
    if ($override.Value -ge 0) {
        $generatorArguments[$override.Name] = $override.Value
    }
}

& (Join-Path $PSScriptRoot "generate-performance-fixtures.ps1") @generatorArguments
& (Join-Path $PSScriptRoot "generate-performance-fixtures.ps1") `
    -OutputDirectory $generatedOutput `
    -VerifyOnly

$manifestPath = Join-Path $generatedOutput "manifest.json"
$manifest = [System.IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json
$cases = [System.Collections.Generic.List[object]]::new()
$simpleProject = Join-Path $repositoryRoot "sample_projects\simple.proj"
$cases.Add([pscustomobject][ordered]@{
        name = "simple"
        kind = "sample"
        projectPath = $simpleProject
        fixtureHash = $null
        fixtureManifestPath = $null
        fixtureManifestCase = $null
        fixtureInputFiles = @($simpleProject)
        query = [pscustomobject][ordered]@{
            properties = @("Configuration", "OutputPath")
            items = @(
                [pscustomobject][ordered]@{
                    type = "Compile"
                    metadata = @()
                }
            )
        }
    }) | Out-Null
foreach ($case in @($manifest.cases)) {
    $cases.Add([pscustomobject][ordered]@{
            name = [string]$case.name
            kind = [string]$case.kind
            projectPath = Join-Path $generatedOutput ([string]$case.project)
            fixtureHash = [string]$case.fixtureHash
            fixtureManifestPath = $manifestPath
            fixtureManifestCase = [string]$case.name
            fixtureInputFiles = @()
            query = $case.query
        }) | Out-Null
}

$requested = @()
if ($null -ne $Fixture -and $Fixture.Count -gt 0) {
    $requested = @($Fixture | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    $available = @($cases | ForEach-Object { $_.name })
    $unknown = @($requested | Where-Object { $available -notcontains $_ })
    if ($unknown.Count -gt 0) {
        throw "Unknown performance fixture(s): $($unknown -join ', '). Available fixtures: $($available -join ', ')."
    }
}

Write-Host "Running performance suite on $hostKey with preset $Preset."
Write-Host "Each selected mode must pass parity before warmups or measurements."

$results = [System.Collections.Generic.List[object]]::new()
$failures = [System.Collections.Generic.List[object]]::new()
$skipped = [System.Collections.Generic.List[object]]::new()
$caseSummaries = [System.Collections.Generic.List[object]]::new()
foreach ($case in $cases) {
    $fixtureSkipReason = if ($case.name -eq "simple" -and $SkipSimple) {
        "-SkipSimple was specified."
    }
    elseif ($requested.Count -gt 0 -and $requested -notcontains $case.name) {
        "Excluded by the -Fixture filter."
    }
    else {
        $null
    }

    foreach ($benchmarkMode in @("preprocess", "evaluation-query")) {
        $caseOutput = Join-Path (Join-Path $fixtureOutput $case.name) $benchmarkMode
        $measurementKind = "fresh-process end-to-end $benchmarkMode"
        $modeSkipReason = if ($Mode -notcontains $benchmarkMode) {
            "Mode '$benchmarkMode' was not selected."
        }
        elseif (-not [string]::IsNullOrWhiteSpace($fixtureSkipReason)) {
            $fixtureSkipReason
        }
        elseif ($benchmarkMode -eq "evaluation-query" -and $null -eq $case.query) {
            "The fixture manifest does not declare an evaluation query."
        }
        else {
            $null
        }

        if (-not [string]::IsNullOrWhiteSpace($modeSkipReason)) {
            $record = [pscustomobject][ordered]@{
                fixture = $case.name
                kind = $case.kind
                mode = $benchmarkMode
                measurementKind = $measurementKind
                status = "skipped"
                skippedReason = $modeSkipReason
                fixtureHash = $case.fixtureHash
                parityStatus = "not-run"
                medianSpeedRatioDotnetOverRust = $null
                implementations = @()
                summary = $null
                samples = $null
                error = $null
            }
            $skipped.Add($record) | Out-Null
            $caseSummaries.Add($record) | Out-Null
            continue
        }

        try {
            $commonArguments = @{
                Project = $case.projectPath
                FixtureName = $case.name
                Iterations = $Iterations
                Warmup = $Warmup
                RustExecutable = $rustPath
                OutputDirectory = $caseOutput
            }
            if (-not [string]::IsNullOrWhiteSpace([string]$case.fixtureHash)) {
                $commonArguments.FixtureHash = $case.fixtureHash
            }
            if (-not [string]::IsNullOrWhiteSpace([string]$case.fixtureManifestPath)) {
                $commonArguments.FixtureManifestPath = $case.fixtureManifestPath
                $commonArguments.FixtureManifestCase = $case.fixtureManifestCase
            }

            if ($benchmarkMode -eq "preprocess") {
                & (Join-Path $PSScriptRoot "compare-preprocess.ps1") @commonArguments
            }
            else {
                $commonArguments.FixtureInputFile = @($case.fixtureInputFiles)
                $commonArguments.PropertyName = @($case.query.properties)
                $commonArguments.ItemType = @($case.query.items | ForEach-Object { [string]$_.type })
                $commonArguments.ItemMetadata = @(
                    foreach ($item in @($case.query.items)) {
                        foreach ($metadataName in @($item.metadata)) {
                            "$($item.type)=$metadataName"
                        }
                    }
                )
                & (Join-Path $PSScriptRoot "compare-evaluation-performance.ps1") @commonArguments
            }

            $caseSummary = [System.IO.File]::ReadAllText((Join-Path $caseOutput "summary.json")) | ConvertFrom-Json
            $record = [pscustomobject][ordered]@{
                fixture = $case.name
                kind = $case.kind
                mode = $benchmarkMode
                measurementKind = $measurementKind
                status = "passed"
                skippedReason = $null
                fixtureHash = [string]$caseSummary.fixtureHash
                parityStatus = [string]$caseSummary.parityStatus
                medianSpeedRatioDotnetOverRust = $caseSummary.medianSpeedRatioDotnetOverRust
                implementations = @($caseSummary.implementations)
                summary = "fixtures/$($case.name)/$benchmarkMode/summary.json"
                samples = "fixtures/$($case.name)/$benchmarkMode/samples.csv"
                error = $null
            }
            $results.Add($record) | Out-Null
            $caseSummaries.Add($record) | Out-Null
        }
        catch {
            $parityPath = Join-Path $caseOutput "parity.json"
            $parityStatus = if (Test-Path -LiteralPath $parityPath) {
                ([System.IO.File]::ReadAllText($parityPath) | ConvertFrom-Json).status
            }
            else {
                "not-recorded"
            }
            $record = [pscustomobject][ordered]@{
                fixture = $case.name
                kind = $case.kind
                mode = $benchmarkMode
                measurementKind = $measurementKind
                status = "failed"
                skippedReason = $null
                fixtureHash = $case.fixtureHash
                parityStatus = $parityStatus
                medianSpeedRatioDotnetOverRust = $null
                implementations = @()
                summary = $null
                samples = $null
                error = $_.Exception.Message
            }
            $failures.Add($record) | Out-Null
            $caseSummaries.Add($record) | Out-Null
            Write-Warning "Performance fixture '$($case.name)' mode '$benchmarkMode' failed: $($_.Exception.Message)"
        }
    }
}

$expectedCaseSummaryCount = $cases.Count * 2
if ($caseSummaries.Count -ne $expectedCaseSummaryCount) {
    throw "Internal coverage error: expected $expectedCaseSummaryCount fixture/mode records, found $($caseSummaries.Count)."
}

$suiteSummary = [pscustomobject][ordered]@{
    schemaVersion = 2
    hostKey = $hostKey
    preset = $Preset
    seed = $Seed
    warmupIterations = $Warmup
    measuredIterations = $Iterations
    generatedManifest = "generated-fixtures/manifest.json"
    generatedManifestSha256 = ([System.IO.File]::ReadAllText((Join-Path $generatedOutput "manifest.sha256"))).Trim()
    performanceThresholdApplied = $false
    declaredFixtures = @("simple") + @($manifest.cases | ForEach-Object { [string]$_.name })
    declaredModes = @("preprocess", "evaluation-query")
    cases = @($caseSummaries)
    results = @($results)
    failures = @($failures)
    skipped = @($skipped)
}
Write-SuiteJson (Join-Path $hostOutput "suite-summary.json") $suiteSummary

$suiteRows = @(
    foreach ($result in $caseSummaries) {
        $implementations = if (@($result.implementations).Count -eq 0) {
            @(
                [pscustomobject]@{
                    implementation = $null
                    sampleCount = $null
                    wallTimeMs = [pscustomobject]@{
                        min = $null
                        median = $null
                        mean = $null
                        p95 = $null
                        max = $null
                        mad = $null
                    }
                    peakWorkingSet = [pscustomobject]@{
                        medianBytes = $null
                        maxBytes = $null
                        medianMiB = $null
                        maxMiB = $null
                    }
                    command = $null
                }
            )
        }
        else {
            @($result.implementations)
        }
        foreach ($implementation in $implementations) {
            [pscustomobject][ordered]@{
                HostKey = $hostKey
                Preset = $Preset
                Fixture = $result.fixture
                Kind = $result.kind
                Mode = $result.mode
                MeasurementKind = $result.measurementKind
                Status = $result.status
                SkippedReason = $result.skippedReason
                FixtureHash = $result.fixtureHash
                Implementation = $implementation.implementation
                SampleCount = $implementation.sampleCount
                WallMinMs = $implementation.wallTimeMs.min
                WallMedianMs = $implementation.wallTimeMs.median
                WallMeanMs = $implementation.wallTimeMs.mean
                WallP95Ms = $implementation.wallTimeMs.p95
                WallMaxMs = $implementation.wallTimeMs.max
                WallMadMs = $implementation.wallTimeMs.mad
                PeakWorkingSetMedianBytes = $implementation.peakWorkingSet.medianBytes
                PeakWorkingSetMaxBytes = $implementation.peakWorkingSet.maxBytes
                PeakWorkingSetMedianMiB = $implementation.peakWorkingSet.medianMiB
                PeakWorkingSetMaxMiB = $implementation.peakWorkingSet.maxMiB
                MedianSpeedRatioDotnetOverRust = $result.medianSpeedRatioDotnetOverRust
                ParityStatus = $result.parityStatus
                Error = $result.error
                Command = $implementation.command
            }
        }
    }
)
$suiteRows | Export-Csv -LiteralPath (Join-Path $hostOutput "suite-summary.csv") -NoTypeInformation

if ($failures.Count -gt 0) {
    throw "$($failures.Count) performance fixture mode(s) failed. See $(Join-Path $hostOutput "suite-summary.json")."
}

Write-Host "Performance suite completed without applying a pass/fail speed threshold."
Write-Host "Suite summary: $(Join-Path $hostOutput "suite-summary.json")"
