[CmdletBinding()]
param(
    [ValidateSet("Smoke", "Benchmark")]
    [string]$Preset = "Benchmark",
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results\generated-fixtures"),
    [ValidateRange(0, 2147483647)]
    [int]$Seed = 1729,
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
    [int]$GlobFileCount = -1,
    [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$script:SeedValue = $Seed

function Get-Sha256Text {
    param([Parameter(Mandatory = $true)][string]$Text)

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return ([System.BitConverter]::ToString($sha256.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha256.Dispose()
    }
}

function Get-DeterministicToken {
    param(
        [Parameter(Mandatory = $true)][string]$Scope,
        [Parameter(Mandatory = $true)][int]$Index
    )

    return (Get-Sha256Text "$($script:SeedValue)|$Scope|$Index").Substring(0, 12)
}

function Write-DeterministicFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Content
    )

    $parent = Split-Path -Parent $Path
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        [System.IO.Directory]::CreateDirectory($parent) | Out-Null
    }
    $normalized = $Content.Replace("`r`n", "`n").Replace("`r", "`n")
    [System.IO.File]::WriteAllText($Path, $normalized, $script:Utf8NoBom)
}

function Join-Lines {
    param([Parameter(Mandatory = $true)][System.Collections.Generic.List[string]]$Lines)
    return ([string]::Join("`n", $Lines) + "`n")
}

function Get-NormalizedRelativePath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Path
    )

    return [System.IO.Path]::GetRelativePath($Root, $Path).Replace("\", "/")
}

function Get-FileRecords {
    param([Parameter(Mandatory = $true)][string]$Root)

    return @(
        Get-ChildItem -LiteralPath $Root -File -Recurse |
            Where-Object { $_.Name -notin @("manifest.json", "manifest.sha256") } |
            ForEach-Object {
                [pscustomobject][ordered]@{
                    path = Get-NormalizedRelativePath $Root $_.FullName
                    bytes = $_.Length
                    sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
                }
            } |
            Sort-Object path
    )
}

function Get-RecordsHash {
    param([Parameter(Mandatory = $true)][object[]]$Records)

    $canonical = [System.Text.StringBuilder]::new()
    foreach ($record in @($Records | Sort-Object path)) {
        [void]$canonical.Append($record.path)
        [void]$canonical.Append("`n")
        [void]$canonical.Append($record.bytes)
        [void]$canonical.Append("`n")
        [void]$canonical.Append($record.sha256)
        [void]$canonical.Append("`n")
    }
    return Get-Sha256Text $canonical.ToString()
}

function Test-GeneratedManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $manifestPath = Join-Path $Root "manifest.json"
    $manifestHashPath = Join-Path $Root "manifest.sha256"
    if (-not (Test-Path -LiteralPath $manifestPath) -or -not (Test-Path -LiteralPath $manifestHashPath)) {
        throw "Generated fixture manifest is incomplete in '$Root'."
    }

    $expectedManifestHash = ([System.IO.File]::ReadAllText($manifestHashPath)).Trim()
    $actualManifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualManifestHash -cne $expectedManifestHash) {
        throw "manifest.json hash mismatch: expected $expectedManifestHash, actual $actualManifestHash."
    }

    $manifest = [System.IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json
    $actualRecords = @(Get-FileRecords $Root)
    $expectedRecords = @($manifest.files)
    if ($actualRecords.Count -ne $expectedRecords.Count) {
        throw "Generated file count mismatch: expected $($expectedRecords.Count), actual $($actualRecords.Count)."
    }

    for ($index = 0; $index -lt $expectedRecords.Count; $index++) {
        $expected = $expectedRecords[$index]
        $actual = $actualRecords[$index]
        if ($expected.path -cne $actual.path -or
            [long]$expected.bytes -ne [long]$actual.bytes -or
            $expected.sha256 -cne $actual.sha256) {
            throw "Generated file mismatch at '$($expected.path)': expected $($expected.sha256), actual $($actual.sha256)."
        }
    }

    $contentHash = Get-RecordsHash $actualRecords
    if ($contentHash -cne $manifest.contentHash) {
        throw "Generated content hash mismatch: expected $($manifest.contentHash), actual $contentHash."
    }

    foreach ($case in @($manifest.cases)) {
        $prefix = "$($case.name)/"
        $caseRecords = @($actualRecords | Where-Object { $_.path.StartsWith($prefix, [System.StringComparison]::Ordinal) })
        $caseHash = Get-RecordsHash $caseRecords
        if ($caseHash -cne $case.fixtureHash) {
            throw "Fixture hash mismatch for '$($case.name)': expected $($case.fixtureHash), actual $caseHash."
        }
    }

    Write-Host "Verified generated fixture manifest: $manifestPath"
    Write-Host "Manifest SHA-256: $actualManifestHash"
    return $manifest
}

$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
if ($VerifyOnly) {
    if (-not (Test-Path -LiteralPath $outputPath)) {
        throw "Cannot verify missing generated fixture directory '$outputPath'."
    }
    Test-GeneratedManifest $outputPath | Out-Null
    return
}

$presetConfiguration = if ($Preset -eq "Smoke") {
    [ordered]@{
        propertyCount = 40
        itemCount = 60
        conditionCount = 40
        importDepth = 3
        importWidth = 2
        globFileCount = 24
    }
}
else {
    [ordered]@{
        propertyCount = 3000
        itemCount = 3000
        conditionCount = 3000
        importDepth = 40
        importWidth = 4
        globFileCount = 800
    }
}

foreach ($override in @(
        @{ Name = "propertyCount"; Value = $PropertyCount },
        @{ Name = "itemCount"; Value = $ItemCount },
        @{ Name = "conditionCount"; Value = $ConditionCount },
        @{ Name = "importDepth"; Value = $ImportDepth },
        @{ Name = "importWidth"; Value = $ImportWidth },
        @{ Name = "globFileCount"; Value = $GlobFileCount }
    )) {
    if ($override.Value -ge 0) {
        $presetConfiguration[$override.Name] = $override.Value
    }
}

foreach ($entry in $presetConfiguration.GetEnumerator()) {
    if ([int]$entry.Value -lt 1) {
        throw "$($entry.Key) must resolve to at least 1."
    }
}

if (Test-Path -LiteralPath $outputPath) {
    Remove-Item -LiteralPath $outputPath -Recurse -Force
}
[System.IO.Directory]::CreateDirectory($outputPath) | Out-Null

$caseSpecifications = [System.Collections.Generic.List[object]]::new()

# Properties
$propertiesDirectory = Join-Path $outputPath "properties"
$propertyLines = [System.Collections.Generic.List[string]]::new()
$propertyLines.Add("<Project>") | Out-Null
$propertyLines.Add("  <PropertyGroup>") | Out-Null
$propertyLines.Add("    <GeneratedSeed>$Seed</GeneratedSeed>") | Out-Null
$propertyLines.Add("    <EscapedValue>left%3Bright%2525%2A.literal&amp;xml</EscapedValue>") | Out-Null
for ($index = 0; $index -lt $presetConfiguration.propertyCount; $index++) {
    $previous = if ($index -eq 0) { "GeneratedSeed" } else { "GeneratedProperty{0:D6}" -f ($index - 1) }
    $token = Get-DeterministicToken "properties" $index
    $propertyLines.Add(('    <GeneratedProperty{0:D6}>value-{1}%3B$({2})%2525</GeneratedProperty{0:D6}>' -f $index, $token, $previous)) | Out-Null
}
$propertyLines.Add("  </PropertyGroup>") | Out-Null
$lastPropertyName = "GeneratedProperty{0:D6}" -f ($presetConfiguration.propertyCount - 1)
$propertyLines.Add("  <ItemGroup>") | Out-Null
$propertyLines.Add('    <BenchmarkProbe Include="properties">') | Out-Null
$propertyLines.Add("      <FinalValue>`$($lastPropertyName)</FinalValue>") | Out-Null
$propertyLines.Add("    </BenchmarkProbe>") | Out-Null
$propertyLines.Add("  </ItemGroup>") | Out-Null
$propertyLines.Add("</Project>") | Out-Null
Write-DeterministicFile (Join-Path $propertiesDirectory "project.proj") (Join-Lines $propertyLines)
$caseSpecifications.Add([pscustomobject][ordered]@{
        name = "properties"
        kind = "properties"
        project = "properties/project.proj"
        parameters = [ordered]@{ propertyCount = $presetConfiguration.propertyCount }
        query = [ordered]@{
            properties = @("EscapedValue", $lastPropertyName)
            items = @(
                [ordered]@{
                    type = "BenchmarkProbe"
                    metadata = @("FinalValue")
                }
            )
        }
    }) | Out-Null

# Items, item definitions, metadata, escaped specs, and globs.
$itemsDirectory = Join-Path $outputPath "items"
for ($index = 0; $index -lt $presetConfiguration.globFileCount; $index++) {
    $bucket = "bucket-{0:D2}" -f ($index % 32)
    $extension = if (($index % 11) -eq 0) { "skip.txt" } else { "txt" }
    $fileName = "asset-{0:D6}-{1}.{2}" -f $index, (Get-DeterministicToken "item-file" $index), $extension
    $filePath = Join-Path (Join-Path (Join-Path $itemsDirectory "tree") $bucket) $fileName
    Write-DeterministicFile $filePath ("fixture=$Seed`nindex=$index`n")
}
$itemLines = [System.Collections.Generic.List[string]]::new()
$itemLines.Add("<Project>") | Out-Null
$itemLines.Add("  <PropertyGroup>") | Out-Null
$itemLines.Add("    <GeneratedSeed>$Seed</GeneratedSeed>") | Out-Null
$itemLines.Add("  </PropertyGroup>") | Out-Null
$itemLines.Add("  <ItemDefinitionGroup>") | Out-Null
$itemLines.Add("    <GeneratedAsset>") | Out-Null
$itemLines.Add('      <DefaultTag>generated-$(GeneratedSeed)</DefaultTag>') | Out-Null
$itemLines.Add("      <DerivedName>%(Filename)%3B%(Extension)</DerivedName>") | Out-Null
$itemLines.Add("    </GeneratedAsset>") | Out-Null
$itemLines.Add("  </ItemDefinitionGroup>") | Out-Null
$itemLines.Add("  <ItemGroup>") | Out-Null
$itemLines.Add('    <GeneratedAsset Include="tree/**/*.txt" Exclude="tree/**/*.skip.txt">') | Out-Null
$itemLines.Add("      <Origin>glob</Origin>") | Out-Null
$itemLines.Add("      <EscapedMetadata>left%3Bright%2525</EscapedMetadata>") | Out-Null
$itemLines.Add("    </GeneratedAsset>") | Out-Null
$itemLines.Add('    <GeneratedQuestion Include="tree/bucket-??/asset-??????-*.txt" />') | Out-Null
$chunkSize = 100
for ($start = 0; $start -lt $presetConfiguration.itemCount; $start += $chunkSize) {
    $end = [Math]::Min($presetConfiguration.itemCount, $start + $chunkSize)
    $identities = [System.Collections.Generic.List[string]]::new()
    for ($index = $start; $index -lt $end; $index++) {
        $identities.Add(("literal/item-{0:D6}-{1}.dat" -f $index, (Get-DeterministicToken "item" $index))) | Out-Null
    }
    if ($start -eq 0) {
        $identities.Add("literal/escaped%3Bvalue.dat") | Out-Null
        $identities.Add("literal/%2A.literal.dat") | Out-Null
        $identities.Add("literal/%3F.literal.dat") | Out-Null
    }
    $itemLines.Add(('    <GeneratedAsset Include="{0}">' -f ([string]::Join(";", $identities)))) | Out-Null
    $itemLines.Add("      <Origin>explicit</Origin>") | Out-Null
    $itemLines.Add("      <Chunk>$start</Chunk>") | Out-Null
    $itemLines.Add('      <Display>%(Filename)%3B$(GeneratedSeed)</Display>') | Out-Null
    $itemLines.Add("    </GeneratedAsset>") | Out-Null
}
$itemLines.Add("    <GeneratedNames Include=`"@(GeneratedAsset->'%(Filename)')`" />") | Out-Null
$itemLines.Add('    <BenchmarkProbe Include="items">') | Out-Null
$itemLines.Add("      <EvaluatedCount>@(GeneratedAsset-&gt;Count())</EvaluatedCount>") | Out-Null
$itemLines.Add("      <ProjectedCount>@(GeneratedNames-&gt;Count())</ProjectedCount>") | Out-Null
$itemLines.Add("    </BenchmarkProbe>") | Out-Null
$itemLines.Add("  </ItemGroup>") | Out-Null
$itemLines.Add("</Project>") | Out-Null
Write-DeterministicFile (Join-Path $itemsDirectory "project.proj") (Join-Lines $itemLines)
$caseSpecifications.Add([pscustomobject][ordered]@{
        name = "items"
        kind = "items"
        project = "items/project.proj"
        parameters = [ordered]@{
            itemCount = $presetConfiguration.itemCount
            globFileCount = $presetConfiguration.globFileCount
        }
        query = [ordered]@{
            properties = @("GeneratedSeed")
            items = @(
                [ordered]@{
                    type = "BenchmarkProbe"
                    metadata = @("EvaluatedCount", "ProjectedCount")
                }
            )
        }
    }) | Out-Null

# Conditions
$conditionsDirectory = Join-Path $outputPath "conditions"
Write-DeterministicFile (Join-Path $conditionsDirectory "exists.marker") "deterministic marker`n"
$conditionLines = [System.Collections.Generic.List[string]]::new()
$conditionLines.Add("<Project>") | Out-Null
$conditionLines.Add("  <PropertyGroup>") | Out-Null
$conditionLines.Add("    <ConditionGate>true</ConditionGate>") | Out-Null
for ($index = 0; $index -lt $presetConfiguration.conditionCount; $index++) {
    $condition = switch ($index % 4) {
        0 { "'`$(ConditionGate)' == 'true' And ('alpha' != 'beta' Or false)" }
        1 { "'1.2.3' &gt; '1.2.0' And true" }
        2 { "Exists('exists.marker') And !HasTrailingSlash('plain')" }
        3 { "`$([MSBuild]::VersionGreaterThanOrEquals('1.2.3','1.2.0')) And true" }
    }
    $token = Get-DeterministicToken "conditions" $index
    $conditionLines.Add(('    <GeneratedCondition{0:D6} Condition="{1}">condition-{2}%3Bvalue</GeneratedCondition{0:D6}>' -f $index, $condition, $token)) | Out-Null
}
$conditionLines.Add("  </PropertyGroup>") | Out-Null
$lastConditionName = "GeneratedCondition{0:D6}" -f ($presetConfiguration.conditionCount - 1)
$conditionLines.Add("  <ItemGroup>") | Out-Null
$conditionLines.Add('    <BenchmarkProbe Include="conditions">') | Out-Null
$conditionLines.Add("      <FinalValue>`$($lastConditionName)</FinalValue>") | Out-Null
$conditionLines.Add("    </BenchmarkProbe>") | Out-Null
$conditionLines.Add("  </ItemGroup>") | Out-Null
$conditionLines.Add("</Project>") | Out-Null
Write-DeterministicFile (Join-Path $conditionsDirectory "project.proj") (Join-Lines $conditionLines)
$caseSpecifications.Add([pscustomobject][ordered]@{
        name = "conditions"
        kind = "conditions"
        project = "conditions/project.proj"
        parameters = [ordered]@{ conditionCount = $presetConfiguration.conditionCount }
        query = [ordered]@{
            properties = @($lastConditionName)
            items = @(
                [ordered]@{
                    type = "BenchmarkProbe"
                    metadata = @("FinalValue")
                }
            )
        }
    }) | Out-Null

# Import graph: independent lanes provide deterministic breadth and each lane has the requested depth.
$importsDirectory = Join-Path $outputPath "imports"
$importFilesDirectory = Join-Path $importsDirectory "graph"
$importRootLines = [System.Collections.Generic.List[string]]::new()
$importRootLines.Add("<Project>") | Out-Null
$importRootLines.Add("  <PropertyGroup>") | Out-Null
$importRootLines.Add("    <ImportGate>enabled</ImportGate>") | Out-Null
$importRootLines.Add("    <ImportSeed>$Seed</ImportSeed>") | Out-Null
$importRootLines.Add("  </PropertyGroup>") | Out-Null
$importRootLines.Add('  <ImportGroup Condition="''$(ImportGate)'' == ''enabled''">') | Out-Null
for ($lane = 0; $lane -lt $presetConfiguration.importWidth; $lane++) {
    $importRootLines.Add(('    <Import Project="graph/depth-000-lane-{0:D3}.props" />' -f $lane)) | Out-Null
}
$importRootLines.Add("  </ImportGroup>") | Out-Null
$lastImportProperty = "ImportLane{0:D3}Depth{1:D3}" -f (
    $presetConfiguration.importWidth - 1), ($presetConfiguration.importDepth - 1)
$importRootLines.Add("  <ItemGroup>") | Out-Null
$importRootLines.Add('    <BenchmarkProbe Include="imports">') | Out-Null
$importRootLines.Add("      <ImportedCount>@(ImportedNode-&gt;Count())</ImportedCount>") | Out-Null
$importRootLines.Add("      <FinalValue>`$($lastImportProperty)</FinalValue>") | Out-Null
$importRootLines.Add("    </BenchmarkProbe>") | Out-Null
$importRootLines.Add("  </ItemGroup>") | Out-Null
$importRootLines.Add("</Project>") | Out-Null
Write-DeterministicFile (Join-Path $importsDirectory "project.proj") (Join-Lines $importRootLines)
for ($lane = 0; $lane -lt $presetConfiguration.importWidth; $lane++) {
    for ($depth = 0; $depth -lt $presetConfiguration.importDepth; $depth++) {
        $importLines = [System.Collections.Generic.List[string]]::new()
        $importLines.Add("<Project>") | Out-Null
        $importLines.Add("  <PropertyGroup Condition=`"'`$(ImportGate)' == 'enabled'`">") | Out-Null
        $previous = if ($depth -eq 0) { "ImportSeed" } else { "ImportLane{0:D3}Depth{1:D3}" -f $lane, ($depth - 1) }
        $propertyName = "ImportLane{0:D3}Depth{1:D3}" -f $lane, $depth
        $token = Get-DeterministicToken "import-$lane" $depth
        $importLines.Add(('    <{0}>$({1})-{2}%3B{3}</{0}>' -f $propertyName, $previous, $token, $depth)) | Out-Null
        $importLines.Add("  </PropertyGroup>") | Out-Null
        $importLines.Add("  <ItemGroup>") | Out-Null
        $importLines.Add(('    <ImportedNode Include="lane-{0:D3}/depth-{1:D3}.node">' -f $lane, $depth)) | Out-Null
        $importLines.Add(('      <Token>{0}</Token>' -f $token)) | Out-Null
        $importLines.Add("    </ImportedNode>") | Out-Null
        $importLines.Add("  </ItemGroup>") | Out-Null
        if (($depth + 1) -lt $presetConfiguration.importDepth) {
            $importLines.Add(('  <Import Project="depth-{0:D3}-lane-{1:D3}.props" Condition="''$(ImportGate)'' == ''enabled''" />' -f ($depth + 1), $lane)) | Out-Null
        }
        $importLines.Add("</Project>") | Out-Null
        $fileName = "depth-{0:D3}-lane-{1:D3}.props" -f $depth, $lane
        Write-DeterministicFile (Join-Path $importFilesDirectory $fileName) (Join-Lines $importLines)
    }
}
$caseSpecifications.Add([pscustomobject][ordered]@{
        name = "imports"
        kind = "import-graph"
        project = "imports/project.proj"
        parameters = [ordered]@{
            importDepth = $presetConfiguration.importDepth
            importWidth = $presetConfiguration.importWidth
            importedFileCount = $presetConfiguration.importDepth * $presetConfiguration.importWidth
        }
        query = [ordered]@{
            properties = @($lastImportProperty)
            items = @(
                [ordered]@{
                    type = "BenchmarkProbe"
                    metadata = @("ImportedCount", "FinalValue")
                }
            )
        }
    }) | Out-Null

# Representative mixed case.
$representativeDirectory = Join-Path $outputPath "representative"
$mixedPropertyCount = [Math]::Max(10, [int][Math]::Ceiling($presetConfiguration.propertyCount / 3.0))
$mixedItemCount = [Math]::Max(10, [int][Math]::Ceiling($presetConfiguration.itemCount / 3.0))
$mixedConditionCount = [Math]::Max(10, [int][Math]::Ceiling($presetConfiguration.conditionCount / 3.0))
$mixedGlobCount = [Math]::Max(10, [int][Math]::Ceiling($presetConfiguration.globFileCount / 3.0))
$mixedImportDepth = [Math]::Max(2, [Math]::Min(12, $presetConfiguration.importDepth))
for ($index = 0; $index -lt $mixedGlobCount; $index++) {
    $directory = "group-{0:D2}" -f ($index % 16)
    $extension = if (($index % 13) -eq 0) { "skip.txt" } else { "txt" }
    $fileName = "mixed-{0:D6}-{1}.{2}" -f $index, (Get-DeterministicToken "mixed-file" $index), $extension
    Write-DeterministicFile (Join-Path (Join-Path (Join-Path $representativeDirectory "tree") $directory) $fileName) "mixed=$index`n"
}
$mixedImportDirectory = Join-Path $representativeDirectory "imports"
for ($depth = 0; $depth -lt $mixedImportDepth; $depth++) {
    $mixedImportLines = [System.Collections.Generic.List[string]]::new()
    $mixedImportLines.Add("<Project>") | Out-Null
    $mixedImportLines.Add("  <PropertyGroup>") | Out-Null
    $mixedImportLines.Add(('    <MixedImport{0:D3}>$({1})-{2}</MixedImport{0:D3}>' -f $depth, $(if ($depth -eq 0) { "MixedSeed" } else { "MixedImport{0:D3}" -f ($depth - 1) }), (Get-DeterministicToken "mixed-import" $depth))) | Out-Null
    $mixedImportLines.Add("  </PropertyGroup>") | Out-Null
    if (($depth + 1) -lt $mixedImportDepth) {
        $mixedImportLines.Add(('  <Import Project="level-{0:D3}.props" Condition="''$(MixedGate)'' == ''true''" />' -f ($depth + 1))) | Out-Null
    }
    $mixedImportLines.Add("</Project>") | Out-Null
    Write-DeterministicFile (Join-Path $mixedImportDirectory ("level-{0:D3}.props" -f $depth)) (Join-Lines $mixedImportLines)
}
$mixedLines = [System.Collections.Generic.List[string]]::new()
$mixedLines.Add("<Project>") | Out-Null
$mixedLines.Add("  <PropertyGroup>") | Out-Null
$mixedLines.Add("    <MixedSeed>$Seed</MixedSeed>") | Out-Null
$mixedLines.Add("    <MixedGate>true</MixedGate>") | Out-Null
$mixedLines.Add("    <MixedEscaped>left%3Bright%2525%2A.literal&amp;xml</MixedEscaped>") | Out-Null
for ($index = 0; $index -lt $mixedPropertyCount; $index++) {
    $previous = if ($index -eq 0) { "MixedSeed" } else { "MixedProperty{0:D6}" -f ($index - 1) }
    $mixedLines.Add(('    <MixedProperty{0:D6}>$({1})-{2}%3Bvalue</MixedProperty{0:D6}>' -f $index, $previous, (Get-DeterministicToken "mixed-property" $index))) | Out-Null
}
$mixedLines.Add("  </PropertyGroup>") | Out-Null
$mixedLines.Add('  <Import Project="imports/level-000.props" Condition="''$(MixedGate)'' == ''true''" />') | Out-Null
$mixedLines.Add("  <PropertyGroup>") | Out-Null
for ($index = 0; $index -lt $mixedConditionCount; $index++) {
    $mixedLines.Add(('    <MixedCondition{0:D6} Condition="''$(MixedGate)'' == ''true'' And (''{1}'' != ''never'' Or false)">{2}</MixedCondition{0:D6}>' -f $index, $index, (Get-DeterministicToken "mixed-condition" $index))) | Out-Null
}
$mixedLines.Add("  </PropertyGroup>") | Out-Null
$mixedLines.Add("  <ItemDefinitionGroup>") | Out-Null
$mixedLines.Add("    <MixedAsset>") | Out-Null
$mixedLines.Add('      <DefaultTag>$(MixedProperty000000)</DefaultTag>') | Out-Null
$mixedLines.Add("      <Derived>%(Filename)%3B%(Extension)</Derived>") | Out-Null
$mixedLines.Add("    </MixedAsset>") | Out-Null
$mixedLines.Add("  </ItemDefinitionGroup>") | Out-Null
$mixedLines.Add("  <ItemGroup>") | Out-Null
$mixedLines.Add('    <MixedAsset Include="tree/**/*.txt" Exclude="tree/**/*.skip.txt">') | Out-Null
$mixedLines.Add("      <Origin>glob</Origin>") | Out-Null
$mixedLines.Add("    </MixedAsset>") | Out-Null
$mixedLines.Add('    <MixedQuestion Include="tree/group-??/mixed-??????-*.txt" />') | Out-Null
for ($start = 0; $start -lt $mixedItemCount; $start += $chunkSize) {
    $end = [Math]::Min($mixedItemCount, $start + $chunkSize)
    $identities = [System.Collections.Generic.List[string]]::new()
    for ($index = $start; $index -lt $end; $index++) {
        $identities.Add(("literal/mixed-{0:D6}-{1}.dat" -f $index, (Get-DeterministicToken "mixed-item" $index))) | Out-Null
    }
    if ($start -eq 0) {
        $identities.Add("literal/mixed%3Bescaped.dat") | Out-Null
        $identities.Add("literal/%2A.mixed.dat") | Out-Null
    }
    $mixedLines.Add(('    <MixedAsset Include="{0}">' -f ([string]::Join(";", $identities)))) | Out-Null
    $mixedLines.Add("      <Origin>explicit</Origin>") | Out-Null
    $mixedLines.Add(('      <Chunk>{0}</Chunk>' -f $start)) | Out-Null
    $mixedLines.Add('      <Display>%(Filename)%3B$(MixedSeed)</Display>') | Out-Null
    $mixedLines.Add("    </MixedAsset>") | Out-Null
}
$mixedLines.Add("    <MixedNames Include=`"@(MixedAsset->'%(Filename)')`" />") | Out-Null
$lastMixedProperty = "MixedProperty{0:D6}" -f ($mixedPropertyCount - 1)
$lastMixedCondition = "MixedCondition{0:D6}" -f ($mixedConditionCount - 1)
$lastMixedImport = "MixedImport{0:D3}" -f ($mixedImportDepth - 1)
$mixedLines.Add('    <BenchmarkProbe Include="representative">') | Out-Null
$mixedLines.Add("      <EvaluatedCount>@(MixedAsset-&gt;Count())</EvaluatedCount>") | Out-Null
$mixedLines.Add("      <ProjectedCount>@(MixedNames-&gt;Count())</ProjectedCount>") | Out-Null
$mixedLines.Add("      <FinalValue>`$($lastMixedProperty)|`$($lastMixedCondition)|`$($lastMixedImport)</FinalValue>") | Out-Null
$mixedLines.Add("    </BenchmarkProbe>") | Out-Null
$mixedLines.Add("  </ItemGroup>") | Out-Null
$mixedLines.Add("</Project>") | Out-Null
Write-DeterministicFile (Join-Path $representativeDirectory "project.proj") (Join-Lines $mixedLines)
$caseSpecifications.Add([pscustomobject][ordered]@{
        name = "representative"
        kind = "mixed"
        project = "representative/project.proj"
        parameters = [ordered]@{
            propertyCount = $mixedPropertyCount
            itemCount = $mixedItemCount
            conditionCount = $mixedConditionCount
            importDepth = $mixedImportDepth
            globFileCount = $mixedGlobCount
        }
        query = [ordered]@{
            properties = @($lastMixedProperty, $lastMixedCondition, $lastMixedImport)
            items = @(
                [ordered]@{
                    type = "BenchmarkProbe"
                    metadata = @("EvaluatedCount", "ProjectedCount", "FinalValue")
                }
            )
        }
    }) | Out-Null

$fileRecords = @(Get-FileRecords $outputPath)
$manifestCases = [System.Collections.Generic.List[object]]::new()
foreach ($case in $caseSpecifications) {
    $prefix = "$($case.name)/"
    $caseRecords = @($fileRecords | Where-Object { $_.path.StartsWith($prefix, [System.StringComparison]::Ordinal) })
    $manifestCases.Add([pscustomobject][ordered]@{
            name = $case.name
            kind = $case.kind
            project = $case.project
            fixtureHash = Get-RecordsHash $caseRecords
            fileCount = $caseRecords.Count
            byteCount = [long](($caseRecords | Measure-Object bytes -Sum).Sum)
            parameters = $case.parameters
            query = $case.query
        }) | Out-Null
}

$manifest = [pscustomobject][ordered]@{
    schemaVersion = 2
    generatorVersion = "1.1.0"
    seed = $Seed
    preset = $Preset
    configuration = $presetConfiguration
    contentHash = Get-RecordsHash $fileRecords
    cases = @($manifestCases)
    files = $fileRecords
}
$manifestJson = ($manifest | ConvertTo-Json -Depth 10).Replace("`r`n", "`n").Replace("`r", "`n") + "`n"
$manifestPath = Join-Path $outputPath "manifest.json"
Write-DeterministicFile $manifestPath $manifestJson
$manifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-DeterministicFile (Join-Path $outputPath "manifest.sha256") "$manifestHash`n"

Test-GeneratedManifest $outputPath | Out-Null
Write-Host "Generated $($manifestCases.Count) deterministic performance fixtures in $outputPath"
