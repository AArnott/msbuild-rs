$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "..\performance-common.ps1")

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)

    if ($Expected -cne $Actual) {
        throw "$Message Expected '$Expected', got '$Actual'."
    }
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$scratch = Join-Path $repositoryRoot "benchmark-results\identity-test-$PID"
try {
    [System.IO.Directory]::CreateDirectory($scratch) | Out-Null
    $projectPath = Join-Path $scratch "project.proj"
    $importPath = Join-Path $scratch "import.props"
    $aggregatePath = Join-Path $scratch "aggregate.xml"
    [System.IO.File]::WriteAllText($projectPath, '<Project><Import Project="import.props" /></Project>')
    [System.IO.File]::WriteAllText($importPath, "<Project />")
    [System.IO.File]::WriteAllText(
        $aggregatePath,
        "<!--`n$projectPath`n$importPath`n-->`n<Project />")

    $first = Get-PerformanceFixtureIdentity `
        -ProjectPath $projectPath `
        -AggregatePreprocessPath $aggregatePath
    Assert-Equal 2 $first.inputCount "The root and transitive import should both be recorded."
    Assert-Equal "preprocess-import-list" $first.source "Aggregate discovery should identify its source."

    [System.IO.File]::AppendAllText($importPath, "`n<!-- changed -->")
    $second = Get-PerformanceFixtureIdentity `
        -ProjectPath $projectPath `
        -AggregatePreprocessPath $aggregatePath
    if ($first.fixtureHash -ceq $second.fixtureHash) {
        throw "Changing an imported fixture file must change the fixture identity."
    }
    Assert-Equal 2 $second.inputCount "Changing an import must not lose input records."
}
finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}

Write-Host "Performance fixture identity tests passed."
