[CmdletBinding()]
param(
    [string]$RustExecutable,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "..\benchmark-results")
)

$ErrorActionPreference = "Stop"
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([string]::IsNullOrWhiteSpace($RustExecutable)) {
    $runningOnWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows)
    $executableName = if ($runningOnWindows) { "msbuild-rs.exe" } else { "msbuild-rs" }
    $debugDirectory = Join-Path (Join-Path $repositoryRoot "target") "debug"
    $RustExecutable = Join-Path $debugDirectory $executableName
}
$rustPath = (Resolve-Path $RustExecutable).Path

$fixturesRoot = Join-Path $repositoryRoot "fixtures"
$evaluationRoot = Join-Path $fixturesRoot "evaluation"
$evaluationOutput = Join-Path $OutputDirectory "evaluation"
Get-ChildItem $evaluationRoot -Recurse -Filter fixture.json |
    Sort-Object FullName |
    ForEach-Object {
        $name = Split-Path -Leaf $_.DirectoryName
        & (Join-Path $PSScriptRoot "compare-evaluation.ps1") `
            -Fixture $_.FullName `
            -RustExecutable $rustPath `
            -OutputDirectory (Join-Path $evaluationOutput $name)
    }

$preprocessRoot = Join-Path $fixturesRoot "preprocess"
$preprocessOutput = Join-Path $OutputDirectory "preprocess"
if (Test-Path $preprocessRoot) {
    Get-ChildItem $preprocessRoot -Recurse -Filter fixture.json |
        Sort-Object FullName |
        ForEach-Object {
            $name = Split-Path -Leaf $_.DirectoryName
            & (Join-Path $PSScriptRoot "compare-preprocess-fixture.ps1") `
                -Fixture $_.FullName `
                -RustExecutable $rustPath `
                -OutputDirectory (Join-Path $preprocessOutput $name)
        }
}
