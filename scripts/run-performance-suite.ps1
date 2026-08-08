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
if (-not $SkipSimple) {
    $simpleProject = Join-Path $repositoryRoot "sample_projects\simple.proj"
    $cases.Add([pscustomobject][ordered]@{
            name = "simple"
            kind = "sample"
            projectPath = $simpleProject
            fixtureHash = (Get-FileHash -LiteralPath $simpleProject -Algorithm SHA256).Hash.ToLowerInvariant()
        }) | Out-Null
}
foreach ($case in @($manifest.cases)) {
    $cases.Add([pscustomobject][ordered]@{
            name = [string]$case.name
            kind = [string]$case.kind
            projectPath = Join-Path $generatedOutput ([string]$case.project)
            fixtureHash = [string]$case.fixtureHash
        }) | Out-Null
}

if ($null -ne $Fixture -and $Fixture.Count -gt 0) {
    $requested = @($Fixture | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    $available = @($cases | ForEach-Object { $_.name })
    $unknown = @($requested | Where-Object { $available -notcontains $_ })
    if ($unknown.Count -gt 0) {
        throw "Unknown performance fixture(s): $($unknown -join ', '). Available fixtures: $($available -join ', ')."
    }
    $cases = [System.Collections.Generic.List[object]]@($cases | Where-Object { $requested -contains $_.name })
}
if ($cases.Count -eq 0) {
    throw "No performance fixtures were selected."
}

Write-Host "Running performance suite on $hostKey with preset $Preset."
Write-Host "Each fixture must pass normalized preprocess parity before warmups or measurements."

$results = [System.Collections.Generic.List[object]]::new()
$failures = [System.Collections.Generic.List[object]]::new()
foreach ($case in $cases) {
    $caseOutput = Join-Path $fixtureOutput $case.name
    try {
        & (Join-Path $PSScriptRoot "compare-preprocess.ps1") `
            -Project $case.projectPath `
            -FixtureName $case.name `
            -FixtureHash $case.fixtureHash `
            -Iterations $Iterations `
            -Warmup $Warmup `
            -RustExecutable $rustPath `
            -OutputDirectory $caseOutput

        $caseSummary = [System.IO.File]::ReadAllText((Join-Path $caseOutput "summary.json")) | ConvertFrom-Json
        $results.Add([pscustomobject][ordered]@{
                fixture = $case.name
                kind = $case.kind
                fixtureHash = $case.fixtureHash
                parityStatus = $caseSummary.parityStatus
                medianSpeedRatioDotnetOverRust = $caseSummary.medianSpeedRatioDotnetOverRust
                implementations = @($caseSummary.implementations)
                summary = "fixtures/$($case.name)/summary.json"
                samples = "fixtures/$($case.name)/samples.csv"
            }) | Out-Null
    }
    catch {
        $parityPath = Join-Path $caseOutput "parity.json"
        $parityStatus = if (Test-Path -LiteralPath $parityPath) {
            ([System.IO.File]::ReadAllText($parityPath) | ConvertFrom-Json).status
        }
        else {
            "not-recorded"
        }
        $failures.Add([pscustomobject][ordered]@{
                fixture = $case.name
                kind = $case.kind
                fixtureHash = $case.fixtureHash
                parityStatus = $parityStatus
                error = $_.Exception.Message
            }) | Out-Null
        Write-Warning "Performance fixture '$($case.name)' failed: $($_.Exception.Message)"
    }
}

$suiteSummary = [pscustomobject][ordered]@{
    schemaVersion = 1
    hostKey = $hostKey
    preset = $Preset
    seed = $Seed
    warmupIterations = $Warmup
    measuredIterations = $Iterations
    generatedManifest = "generated-fixtures/manifest.json"
    generatedManifestSha256 = ([System.IO.File]::ReadAllText((Join-Path $generatedOutput "manifest.sha256"))).Trim()
    performanceThresholdApplied = $false
    results = @($results)
    failures = @($failures)
}
Write-SuiteJson (Join-Path $hostOutput "suite-summary.json") $suiteSummary

$suiteRows = @(
    foreach ($result in $results) {
        foreach ($implementation in @($result.implementations)) {
            [pscustomobject][ordered]@{
                HostKey = $hostKey
                Preset = $Preset
                Fixture = $result.fixture
                Kind = $result.kind
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
                Command = $implementation.command
            }
        }
    }
)
$suiteRows | Export-Csv -LiteralPath (Join-Path $hostOutput "suite-summary.csv") -NoTypeInformation

if ($failures.Count -gt 0) {
    throw "$($failures.Count) performance fixture(s) failed. See $(Join-Path $hostOutput "suite-summary.json")."
}

Write-Host "Performance suite completed without applying a pass/fail speed threshold."
Write-Host "Suite summary: $(Join-Path $hostOutput "suite-summary.json")"
