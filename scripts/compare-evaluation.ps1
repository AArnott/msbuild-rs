[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Fixture,
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results\evaluation")
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "path-normalization.ps1")

if ([string]::IsNullOrWhiteSpace($RustExecutable)) {
    $executableName = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        "msbuild-rs.exe"
    } else {
        "msbuild-rs"
    }
    $RustExecutable = Join-Path $PSScriptRoot "..\target\debug\$executableName"
}

function Normalize-Value {
    param(
        [AllowNull()]
        [string]$Value,
        [string]$FixtureDirectory
    )

    if ($null -eq $Value) {
        return ""
    }

    # Only replace machine-specific roots. All other text, ordering, and
    # separators remain significant to the comparison.
    $normalized = Replace-PathForComparison $Value $FixtureDirectory "<FIXTURE_DIRECTORY>"
    if (-not [string]::IsNullOrWhiteSpace($script:SdkPath)) {
        $normalized = Replace-PathForComparison $normalized $script:SdkPath "<MSBUILD_SDKS_PATH>"
    }
    return $normalized
}

function Get-PropertyValue {
    param($Object, [string]$Name)
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return ""
    }
    return [string]$property.Value
}

function Write-MismatchDiagnostic {
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
    $lines = for ($index = $start; $index -le $end; $index++) {
        $expectedLine = if ($index -lt $expectedLines.Count) { $expectedLines[$index] } else { "<end of file>" }
        $actualLine = if ($index -lt $actualLines.Count) { $actualLines[$index] } else { "<end of file>" }
        "line $($index + 1):`n  dotnet: $expectedLine`n  rust:   $actualLine"
    }
    $lines | Set-Content -Path $Path -Encoding utf8
    return "Evaluation output differs at line $($firstDifference + 1). See $Path"
}

$fixturePath = (Resolve-Path $Fixture).Path
$fixtureDirectory = Split-Path -Parent $fixturePath
$fixtureDefinition = Get-Content -Raw $fixturePath | ConvertFrom-Json
$projectPath = (Resolve-Path (Join-Path $fixtureDirectory $fixtureDefinition.project)).Path
$rustPath = (Resolve-Path $RustExecutable -ErrorAction Stop).Path
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
[System.IO.Directory]::CreateDirectory($outputPath) | Out-Null
$dotnetSdkVersion = (& dotnet --version).Trim()
$script:SdkPath = (& dotnet msbuild $projectPath -nologo -getProperty:MSBuildSDKsPath).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query MSBuildSDKsPath (exit code $LASTEXITCODE)"
}
Write-Host ".NET SDK: $dotnetSdkVersion"
Write-Host "MSBuildSDKsPath: $script:SdkPath"

$properties = @($fixtureDefinition.properties)
$itemDefinitions = @($fixtureDefinition.items)
$itemTypes = @($itemDefinitions | ForEach-Object { $_.type })
if ($properties.Count -eq 0 -and $itemTypes.Count -eq 0) {
    throw "Fixture must select at least one property or item type."
}

$dotnetArguments = @("msbuild", $projectPath, "-nologo")
if ($properties.Count -gt 0) {
    $dotnetProperties = @($properties)
    if ($itemTypes.Count -eq 0 -and $dotnetProperties.Count -eq 1) {
        # MSBuild prints a bare value for one property; add an ignored property to
        # keep the raw protocol JSON without changing the normalized projection.
        $dotnetProperties += "__MsbuildRsCompatibilitySentinel"
    }
    $dotnetArguments += "-getProperty:$($dotnetProperties -join ',')"
}
if ($itemTypes.Count -gt 0) {
    $dotnetArguments += "-getItem:$($itemTypes -join ',')"
}
$dotnetErrorPath = Join-Path $outputPath "dotnet-evaluation.stderr.txt"
$dotnetRaw = (& dotnet @dotnetArguments 2>$dotnetErrorPath | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "dotnet msbuild evaluation query exited with code $LASTEXITCODE. See $dotnetErrorPath"
}

$rustArguments = @("--project", $projectPath)
foreach ($property in $properties) {
    $rustArguments += @("--get-property", $property)
}
foreach ($itemType in $itemTypes) {
    $rustArguments += @("--get-item", $itemType)
}
$rustErrorPath = Join-Path $outputPath "rust-evaluation.stderr.txt"
$rustRaw = (& $rustPath @rustArguments 2>$rustErrorPath | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "msbuild-rs evaluation query exited with code $LASTEXITCODE. See $rustErrorPath"
}

$dotnetRawPath = Join-Path $outputPath "dotnet-evaluation.raw.json"
$rustRawPath = Join-Path $outputPath "rust-evaluation.raw.json"
$dotnetRaw | Set-Content -Path $dotnetRawPath -Encoding utf8
$rustRaw | Set-Content -Path $rustRawPath -Encoding utf8
$dotnetResult = $dotnetRaw | ConvertFrom-Json
$rustResult = $rustRaw | ConvertFrom-Json

function New-NormalizedResult {
    param($Result)

    $normalizedProperties = [ordered]@{}
    foreach ($property in $properties) {
        $normalizedProperties[$property] = Normalize-Value (Get-PropertyValue $Result.Properties $property) $fixtureDirectory
    }

    $normalizedItems = [ordered]@{}
    foreach ($itemDefinition in $itemDefinitions) {
        $normalizedItemList = @()
        $itemsProperty = $Result.Items.PSObject.Properties[$itemDefinition.type]
        $items = if ($null -eq $itemsProperty) { @() } else { @($itemsProperty.Value) }
        foreach ($item in $items) {
            $metadata = [ordered]@{}
            foreach ($metadataName in @($itemDefinition.metadata)) {
                $metadata[$metadataName] = Normalize-Value (Get-PropertyValue $item $metadataName) $fixtureDirectory
                if ($item.PSObject.Properties["metadata"]) {
                    $metadata[$metadataName] = Normalize-Value (Get-PropertyValue $item.metadata $metadataName) $fixtureDirectory
                }
            }
            $normalizedItemList += [ordered]@{
                identity = Normalize-Value (Get-PropertyValue $item "Identity") $fixtureDirectory
                metadata = $metadata
            }
            if ($item.PSObject.Properties["identity"]) {
                $normalizedItemList[-1].identity = Normalize-Value (Get-PropertyValue $item "identity") $fixtureDirectory
            }
        }
        $normalizedItems[$itemDefinition.type] = $normalizedItemList
    }

    return [ordered]@{
        properties = $normalizedProperties
        items = $normalizedItems
    }
}

$dotnetJson = (New-NormalizedResult $dotnetResult | ConvertTo-Json -Depth 10)
$rustJson = (New-NormalizedResult $rustResult | ConvertTo-Json -Depth 10)
$dotnetJsonPath = Join-Path $outputPath "dotnet-evaluation.json"
$rustJsonPath = Join-Path $outputPath "rust-evaluation.json"
$dotnetJson | Set-Content -Path $dotnetJsonPath -Encoding utf8
$rustJson | Set-Content -Path $rustJsonPath -Encoding utf8

if ($dotnetJson -cne $rustJson) {
    $diagnosticPath = Join-Path $outputPath "evaluation-mismatch.txt"
    throw (Write-MismatchDiagnostic $dotnetJson $rustJson $diagnosticPath)
}

Write-Host "Evaluation parity passed: $($fixtureDefinition.name)"
Write-Host "Raw outputs: $dotnetRawPath, $rustRawPath"
Write-Host "Normalized output: $dotnetJsonPath"
