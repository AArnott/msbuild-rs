[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Fixture,
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results\preprocess")
)

$ErrorActionPreference = "Stop"
$fixturePath = (Resolve-Path $Fixture).Path
$fixtureDirectory = Split-Path -Parent $fixturePath
$definition = Get-Content -Raw $fixturePath | ConvertFrom-Json
$projectPath = (Resolve-Path (Join-Path $fixtureDirectory $definition.project)).Path

$arguments = @{
    Project = $projectPath
    Iterations = 1
    Warmup = 0
    OutputDirectory = $OutputDirectory
    FixtureName = $definition.name
    ParityOnly = $true
    CompareOutput = $true
    FailOnMismatch = $true
}
if (-not [string]::IsNullOrWhiteSpace($RustExecutable)) {
    $arguments.RustExecutable = $RustExecutable
}
if ($definition.PSObject.Properties["comparison"] -and
    $definition.comparison -eq "semanticXml") {
    $arguments.SemanticXmlComparison = $true
}

& (Join-Path $PSScriptRoot "compare-preprocess.ps1") @arguments
Write-Host "Preprocess parity passed: $($definition.name)"
