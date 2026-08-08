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
    [switch]$ParityOnly,
    [switch]$CompareOutput,
    [switch]$FailOnMismatch,
    [switch]$SemanticXmlComparison
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "path-normalization.ps1")

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Write-Utf8File {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [AllowNull()][AllowEmptyString()][string]$Content
    )

    $parent = Split-Path -Parent $Path
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        [System.IO.Directory]::CreateDirectory($parent) | Out-Null
    }
    if ($null -eq $Content) {
        $Content = ""
    }
    [System.IO.File]::WriteAllText($Path, $Content, $script:Utf8NoBom)
}

function Write-JsonFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$Value
    )

    $json = ($Value | ConvertTo-Json -Depth 12).Replace("`r`n", "`n").Replace("`r", "`n") + "`n"
    Write-Utf8File $Path $json
}

function Normalize-PreprocessedOutput {
    param(
        [Parameter(Mandatory = $true)][string]$InputPath,
        [Parameter(Mandatory = $true)][string]$NormalizedPath,
        [Parameter(Mandatory = $true)][string]$ProjectDirectory,
        [AllowEmptyString()][string]$SdkPath
    )

    $content = [System.IO.File]::ReadAllText($InputPath)
    $content = $content.TrimStart([char]0xFEFF).Replace("`r`n", "`n").Replace("`r", "`n")
    foreach ($replacement in @(
            @{ Path = $ProjectDirectory; Token = "<PROJECT_DIRECTORY>" },
            @{ Path = $SdkPath; Token = "<MSBUILD_SDKS_PATH>" }
        )) {
        if (-not [string]::IsNullOrWhiteSpace($replacement.Path)) {
            $content = Replace-PathForComparison $content $replacement.Path $replacement.Token
        }

    }
    Write-Utf8File $NormalizedPath ($content.Replace("`n", [Environment]::NewLine))
    return $content
}

function ConvertTo-SemanticXmlProjection {
    param([Parameter(Mandatory = $true)][string]$Content)

    $document = [System.Xml.XmlDocument]::new()
    $document.PreserveWhitespace = $true
    $document.XmlResolver = $null
    $document.LoadXml($Content)
    $lines = [System.Collections.Generic.List[string]]::new()
    $visit = {
        param([System.Xml.XmlNode]$Node, [int]$Depth)

        if ($Node.NodeType -eq [System.Xml.XmlNodeType]::Element) {
            $attributes = @(
                $Node.Attributes |
                    Where-Object {
                        $_.Prefix -ne "xmlns" -and
                        $_.Name -ne "xmlns" -and
                        -not ($Depth -eq 0 -and $Node.LocalName -eq "Project" -and
                            $_.LocalName -in @("Sdk", "DefaultTargets"))
                    } |
                    ForEach-Object {
                        "$($_.LocalName)=$($_.Value.Replace("`r`n", "`n").Replace("`r", "`n"))"
                    } |
                    Sort-Object
            )
            $lines.Add("$("  " * $Depth)E:$($Node.LocalName)|$($attributes -join "|")") | Out-Null
            foreach ($child in $Node.ChildNodes) {
                & $visit $child ($Depth + 1)
            }
            $lines.Add("$("  " * $Depth)X:$($Node.LocalName)") | Out-Null
        }
        elseif ($Node.NodeType -in @(
                [System.Xml.XmlNodeType]::Text,
                [System.Xml.XmlNodeType]::CDATA
            )) {
            $value = $Node.Value.Replace("`r`n", "`n").Replace("`r", "`n")
            if (-not [string]::IsNullOrWhiteSpace($value)) {
                $lines.Add("$("  " * $Depth)T:$($value.Trim())") | Out-Null
            }
        }
    }
    & $visit $document.DocumentElement 0
    return [string]::Join("`n", $lines)
}

function Write-PreprocessMismatchDiagnostic {
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
    $diagnostic = [System.Collections.Generic.List[string]]::new()
    for ($index = $start; $index -le $end; $index++) {
        $expectedLine = if ($index -lt $expectedLines.Count) { $expectedLines[$index] } else { "<end of file>" }
        $actualLine = if ($index -lt $actualLines.Count) { $actualLines[$index] } else { "<end of file>" }
        $diagnostic.Add("line $($index + 1):`n  dotnet: $expectedLine`n  rust:   $actualLine") | Out-Null
    }
    Write-Utf8File $Path ([string]::Join("`n", $diagnostic) + "`n")
    return "Preprocessed output differs at line $($firstDifference + 1). See $Path"
}

function Format-Command {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $formatted = @($Executable) + @(
        $Arguments | ForEach-Object {
            if ($_ -match '[\s"]') {
                '"' + $_.Replace('"', '\"') + '"'
            }
            else {
                $_
            }
        }
    )
    return [string]::Join(" ", $formatted)
}

function Invoke-DirectProcess {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$StdoutPath,
        [Parameter(Mandatory = $true)][string]$StderrPath
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) {
        [void]$startInfo.ArgumentList.Add([string]$argument)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $process.Start()) {
            throw "Could not start '$Executable'."
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $peakWorkingSet = 0L
        do {
            try {
                $process.Refresh()
                $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            }
            catch {
                # A very short-lived process may exit between the wait and refresh.
            }
            $exited = $process.WaitForExit(10)
        } while (-not $exited)
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $stopwatch.Stop()
        $exitCode = $process.ExitCode
    }
    finally {
        if ($stopwatch.IsRunning) {
            $stopwatch.Stop()
        }
        $process.Dispose()
    }

    Write-Utf8File $StdoutPath $stdout
    Write-Utf8File $StderrPath $stderr
    return [pscustomobject][ordered]@{
        elapsedWallMs = $stopwatch.Elapsed.TotalMilliseconds
        peakWorkingSetBytes = [long]$peakWorkingSet
        exitCode = [int]$exitCode
    }
}

function Get-Median {
    param([Parameter(Mandatory = $true)][double[]]$Values)

    $sorted = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 0) {
        return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2
    }
    return [double]$sorted[$middle]
}

function Get-Distribution {
    param([Parameter(Mandatory = $true)][double[]]$Values)

    if ($Values.Count -eq 0) {
        throw "Cannot summarize an empty sample set."
    }
    $sorted = @($Values | Sort-Object)
    $median = Get-Median $Values
    $deviations = [double[]]@($Values | ForEach-Object { [Math]::Abs($_ - $median) })
    $p95Index = [Math]::Min($sorted.Count - 1, [int][Math]::Ceiling($sorted.Count * 0.95) - 1)
    return [pscustomobject][ordered]@{
        min = [Math]::Round([double]$sorted[0], 3)
        median = [Math]::Round($median, 3)
        mean = [Math]::Round([double](($Values | Measure-Object -Average).Average), 3)
        p95 = [Math]::Round([double]$sorted[$p95Index], 3)
        max = [Math]::Round([double]$sorted[-1], 3)
        mad = [Math]::Round((Get-Median $deviations), 3)
    }
}

function Get-HostCpuName {
    if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        try {
            $name = (Get-CimInstance Win32_Processor -ErrorAction Stop | Select-Object -First 1 -ExpandProperty Name).Trim()
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                return $name
            }
        }
        catch {
            # Fall through to environment metadata.
        }
    }
    elseif ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Linux)) {
        try {
            $line = Get-Content -LiteralPath "/proc/cpuinfo" |
                Where-Object { $_ -match '^model name\s*:' } |
                Select-Object -First 1
            if ($line) {
                return ($line -replace '^model name\s*:\s*', '').Trim()
            }
        }
        catch {
            # Fall through to environment metadata.
        }
    }
    elseif ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::OSX)) {
        try {
            $name = (& sysctl -n machdep.cpu.brand_string 2>$null | Out-String).Trim()
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                return $name
            }
        }
        catch {
            # Fall through to environment metadata.
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_IDENTIFIER)) {
        return $env:PROCESSOR_IDENTIFIER
    }
    return [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
}

function Get-HostKey {
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

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$projectPath = (Resolve-Path $Project).Path
$projectDirectory = Split-Path -Parent $projectPath
$rustPath = (Resolve-Path $RustExecutable -ErrorAction Stop).Path
$dotnetCommandInfo = Get-Command dotnet -CommandType Application -ErrorAction Stop | Select-Object -First 1
$dotnetPath = $dotnetCommandInfo.Source
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
$rawPath = Join-Path $outputPath "raw"
$normalizedPath = Join-Path $outputPath "normalized"
$workPath = Join-Path $outputPath "work"
foreach ($path in @($rawPath, $normalizedPath, $workPath)) {
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Recurse -Force
    }
    [System.IO.Directory]::CreateDirectory($path) | Out-Null
}
foreach ($staleFile in @("samples.csv", "summary.csv", "summary.json", "parity.json", "run-metadata.json", "preprocess-mismatch.txt")) {
    $stalePath = Join-Path $outputPath $staleFile
    if (Test-Path -LiteralPath $stalePath) {
        Remove-Item -LiteralPath $stalePath -Force
    }
}

if ([string]::IsNullOrWhiteSpace($FixtureName)) {
    $FixtureName = [System.IO.Path]::GetFileNameWithoutExtension($projectPath)
}
if ([string]::IsNullOrWhiteSpace($FixtureHash)) {
    $FixtureHash = (Get-FileHash -LiteralPath $projectPath -Algorithm SHA256).Hash.ToLowerInvariant()
}

$dotnetSdkVersion = (& $dotnetPath --version).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query dotnet SDK version (exit code $LASTEXITCODE)."
}
$sdkPath = (& $dotnetPath msbuild $projectPath -nologo -getProperty:MSBuildSDKsPath | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query MSBuildSDKsPath (exit code $LASTEXITCODE)."
}

$commitSha = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not query the current commit SHA."
}
$repositoryDirty = @(& git -C $repositoryRoot status --porcelain).Count -gt 0
$hostKey = Get-HostKey
$hostOs = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
$hostArchitecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$hostCpu = Get-HostCpuName
$startedAtUtc = [DateTime]::UtcNow.ToString("o")

$dotnetRawOutput = Join-Path $rawPath "dotnet-preprocessed.xml"
$rustRawOutput = Join-Path $rawPath "rust-preprocessed.xml"
$dotnetParityArguments = @("msbuild", $projectPath, "-nologo", "-pp:$dotnetRawOutput")
$rustParityArguments = @("--project", $projectPath, "--preprocess", $rustRawOutput)
$dotnetParityCommand = Format-Command $dotnetPath $dotnetParityArguments
$rustParityCommand = Format-Command $rustPath $rustParityArguments

Write-Host "Fixture: $FixtureName ($FixtureHash)"
Write-Host ".NET SDK: $dotnetSdkVersion"
Write-Host "MSBuildSDKsPath: $sdkPath"
Write-Host "Establishing normalized preprocess parity before timing..."

$dotnetParity = Invoke-DirectProcess `
    -Executable $dotnetPath `
    -Arguments $dotnetParityArguments `
    -WorkingDirectory $projectDirectory `
    -StdoutPath (Join-Path $rawPath "dotnet-parity.stdout.txt") `
    -StderrPath (Join-Path $rawPath "dotnet-parity.stderr.txt")
$rustParity = Invoke-DirectProcess `
    -Executable $rustPath `
    -Arguments $rustParityArguments `
    -WorkingDirectory $projectDirectory `
    -StdoutPath (Join-Path $rawPath "rust-parity.stdout.txt") `
    -StderrPath (Join-Path $rawPath "rust-parity.stderr.txt")

$parityPassed = $false
$parityMessage = ""
if ($dotnetParity.exitCode -ne 0 -or $rustParity.exitCode -ne 0) {
    $parityMessage = "Preprocess parity commands failed: dotnet=$($dotnetParity.exitCode), rust=$($rustParity.exitCode)."
}
elseif (-not (Test-Path -LiteralPath $dotnetRawOutput) -or -not (Test-Path -LiteralPath $rustRawOutput)) {
    $parityMessage = "A preprocess command did not create its expected output."
}
else {
    $dotnetNormalizedOutput = Join-Path $normalizedPath "dotnet-preprocessed.xml"
    $rustNormalizedOutput = Join-Path $normalizedPath "rust-preprocessed.xml"
    $dotnetNormalized = Normalize-PreprocessedOutput $dotnetRawOutput $dotnetNormalizedOutput $projectDirectory $sdkPath
    $rustNormalized = Normalize-PreprocessedOutput $rustRawOutput $rustNormalizedOutput $projectDirectory $sdkPath
    if ($SemanticXmlComparison) {
        $dotnetNormalized = ConvertTo-SemanticXmlProjection $dotnetNormalized
        $rustNormalized = ConvertTo-SemanticXmlProjection $rustNormalized
    }
    if ($dotnetNormalized -ceq $rustNormalized) {
        $parityPassed = $true
        $parityMessage = if ($SemanticXmlComparison) {
            "Semantic XML preprocess parity passed."
        } else {
            "Normalized preprocess parity passed."
        }
    }
    else {
        $diagnosticPath = Join-Path $outputPath "preprocess-mismatch.txt"
        $parityMessage = Write-PreprocessMismatchDiagnostic $dotnetNormalized $rustNormalized $diagnosticPath
    }
}

$parity = [pscustomobject][ordered]@{
    status = if ($parityPassed) { "passed" } else { "failed" }
    message = $parityMessage
    fixture = $FixtureName
    fixtureHash = $FixtureHash
    comparison = if ($SemanticXmlComparison) { "semanticXml" } else { "normalizedText" }
    dotnet = [ordered]@{
        command = $dotnetParityCommand
        exitCode = $dotnetParity.exitCode
        elapsedWallMs = [Math]::Round($dotnetParity.elapsedWallMs, 3)
        peakWorkingSetBytes = $dotnetParity.peakWorkingSetBytes
    }
    rust = [ordered]@{
        command = $rustParityCommand
        exitCode = $rustParity.exitCode
        elapsedWallMs = [Math]::Round($rustParity.elapsedWallMs, 3)
        peakWorkingSetBytes = $rustParity.peakWorkingSetBytes
    }
}
Write-JsonFile (Join-Path $outputPath "parity.json") $parity

$runMetadata = [pscustomobject][ordered]@{
    schemaVersion = 1
    startedAtUtc = $startedAtUtc
    fixture = $FixtureName
    fixtureHash = $FixtureHash
    project = $projectPath
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
        dotnet = $dotnetParityCommand
        rust = $rustParityCommand
    }
}
Write-JsonFile (Join-Path $outputPath "run-metadata.json") $runMetadata

if (-not $parityPassed) {
    Write-Warning $parityMessage
    throw "Fixture '$FixtureName' is ineligible for timing because normalized preprocess parity failed."
}
Write-Host $parityMessage

if ($ParityOnly) {
    Write-Host "Parity-only mode completed without collecting timed samples."
    return
}

$dotnetWorkOutput = Join-Path $workPath "dotnet-preprocessed.xml"
$rustWorkOutput = Join-Path $workPath "rust-preprocessed.xml"
$dotnetArguments = @("msbuild", $projectPath, "-nologo", "-pp:$dotnetWorkOutput")
$rustArguments = @("--project", $projectPath, "--preprocess", $rustWorkOutput)
$dotnetCommand = Format-Command $dotnetPath $dotnetArguments
$rustCommand = Format-Command $rustPath $rustArguments
$samplesPath = Join-Path $outputPath "samples.csv"
$samples = [System.Collections.Generic.List[object]]::new()
$sequence = 0

function Invoke-BenchmarkSample {
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
    $result = Invoke-DirectProcess `
        -Executable $executable `
        -Arguments $arguments `
        -WorkingDirectory $projectDirectory `
        -StdoutPath (Join-Path $rawPath "$logPrefix.stdout.txt") `
        -StderrPath (Join-Path $rawPath "$logPrefix.stderr.txt")

    return [pscustomobject][ordered]@{
        Sequence = $Sequence
        Phase = $Phase
        Iteration = $Iteration
        OrderInIteration = $OrderInIteration
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

function Invoke-BenchmarkPhase {
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
            $sample = Invoke-BenchmarkSample `
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
    Invoke-BenchmarkPhase -Phase warmup -Count $Warmup
}
Write-Host "Running $Iterations interleaved measured iteration(s) per implementation..."
Invoke-BenchmarkPhase -Phase measurement -Count $Iterations

$measuredSamples = @($samples | Where-Object { $_.Measured -and $_.Valid -and $_.ParityStatus -eq "passed" })
$implementationSummaries = [System.Collections.Generic.List[object]]::new()
foreach ($implementation in @("dotnet-msbuild", "msbuild-rs")) {
    $implementationSamples = @($measuredSamples | Where-Object { $_.Implementation -eq $implementation })
    if ($implementationSamples.Count -ne $Iterations) {
        throw "Expected $Iterations valid measured samples for $implementation, found $($implementationSamples.Count)."
    }
    $wallValues = [double[]]@($implementationSamples | ForEach-Object { [double]$_.ElapsedWallMs })
    $peakValues = [double[]]@($implementationSamples | ForEach-Object { [double]$_.PeakWorkingSetBytes })
    $wall = Get-Distribution $wallValues
    $peakMedian = Get-Median $peakValues
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
    fixture = $FixtureName
    fixtureHash = $FixtureHash
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
Write-JsonFile (Join-Path $outputPath "summary.json") $summary

$summaryRows = @(
    $implementationSummaries | ForEach-Object {
        [pscustomobject][ordered]@{
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
Write-Host "Raw preprocess output: $rawPath"
Write-Host "Normalized preprocess output: $normalizedPath"

if (Test-Path -LiteralPath $workPath) {
    Remove-Item -LiteralPath $workPath -Recurse -Force
}
