$script:PerformanceUtf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Write-PerformanceUtf8File {
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
    [System.IO.File]::WriteAllText($Path, $Content, $script:PerformanceUtf8NoBom)
}

function Write-PerformanceJsonFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$Value
    )

    $json = ($Value | ConvertTo-Json -Depth 16).Replace("`r`n", "`n").Replace("`r", "`n") + "`n"
    Write-PerformanceUtf8File $Path $json
}

function Get-PerformanceSha256Text {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return ([System.BitConverter]::ToString($sha256.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha256.Dispose()
    }
}

function Get-PerformanceRecordsHash {
    param([Parameter(Mandatory = $true)][object[]]$Records)

    $canonical = [System.Text.StringBuilder]::new()
    foreach ($record in @($Records | Sort-Object path)) {
        [void]$canonical.Append([string]$record.path)
        [void]$canonical.Append("`n")
        [void]$canonical.Append([long]$record.bytes)
        [void]$canonical.Append("`n")
        [void]$canonical.Append([string]$record.sha256)
        [void]$canonical.Append("`n")
    }
    return Get-PerformanceSha256Text $canonical.ToString()
}

function ConvertTo-PerformanceInputPath {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ProjectDirectory,
        [AllowEmptyString()][string]$SdkPath
    )

    $fullPath = [System.IO.Path]::GetFullPath($Path)
    foreach ($root in @(
            @{ Path = $ProjectDirectory; Token = "<PROJECT_DIRECTORY>" },
            @{ Path = $SdkPath; Token = "<MSBUILD_SDKS_PATH>" }
        )) {
        if ([string]::IsNullOrWhiteSpace($root.Path)) {
            continue
        }
        $rootPath = [System.IO.Path]::GetFullPath([string]$root.Path)
        $relative = [System.IO.Path]::GetRelativePath($rootPath, $fullPath)
        if ($relative -ne ".." -and
            -not $relative.StartsWith("..$([System.IO.Path]::DirectorySeparatorChar)") -and
            -not [System.IO.Path]::IsPathRooted($relative)) {
            $suffix = if ($relative -eq ".") { "" } else { "/$($relative.Replace('\', '/'))" }
            return "$($root.Token)$suffix"
        }
    }
    return $fullPath.Replace("\", "/")
}

function Get-PerformanceFixtureIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$ProjectPath,
        [AllowEmptyString()][string]$ManifestPath,
        [AllowEmptyString()][string]$ManifestCase,
        [AllowEmptyString()][string]$AggregatePreprocessPath,
        [string[]]$ExplicitInputPaths,
        [AllowEmptyString()][string]$SdkPath,
        [AllowEmptyString()][string]$ExpectedHash
    )

    $resolvedProject = (Resolve-Path -LiteralPath $ProjectPath -ErrorAction Stop).Path
    $projectDirectory = Split-Path -Parent $resolvedProject
    if (-not [string]::IsNullOrWhiteSpace($ManifestPath)) {
        if ([string]::IsNullOrWhiteSpace($ManifestCase)) {
            throw "FixtureManifestCase is required with FixtureManifestPath."
        }
        $resolvedManifest = (Resolve-Path -LiteralPath $ManifestPath -ErrorAction Stop).Path
        $manifestRoot = Split-Path -Parent $resolvedManifest
        $manifest = [System.IO.File]::ReadAllText($resolvedManifest) | ConvertFrom-Json
        $case = @($manifest.cases | Where-Object { [string]$_.name -ceq $ManifestCase })
        if ($case.Count -ne 1) {
            throw "Generated manifest must contain exactly one case named '$ManifestCase'."
        }
        $prefix = "$ManifestCase/"
        $manifestRecords = @(
            $manifest.files |
                Where-Object { ([string]$_.path).StartsWith($prefix, [System.StringComparison]::Ordinal) } |
                Sort-Object path
        )
        if ($manifestRecords.Count -eq 0) {
            throw "Generated manifest case '$ManifestCase' has no input files."
        }
        $records = @(
            foreach ($record in $manifestRecords) {
                $inputPath = Join-Path $manifestRoot ([string]$record.path)
                $resolvedInput = (Resolve-Path -LiteralPath $inputPath -ErrorAction Stop).Path
                $actualLength = (Get-Item -LiteralPath $resolvedInput).Length
                $actualHash = (Get-FileHash -LiteralPath $resolvedInput -Algorithm SHA256).Hash.ToLowerInvariant()
                if ($actualLength -ne [long]$record.bytes -or $actualHash -cne [string]$record.sha256) {
                    throw "Generated fixture input '$($record.path)' does not match its manifest record."
                }
                [pscustomobject][ordered]@{
                    path = [string]$record.path
                    resolvedPath = $resolvedInput.Replace("\", "/")
                    bytes = [long]$actualLength
                    sha256 = $actualHash
                }
            }
        )
        $fixtureHash = Get-PerformanceRecordsHash $records
        if ($fixtureHash -cne [string]$case[0].fixtureHash) {
            throw "Generated fixture hash mismatch for '$ManifestCase': expected $($case[0].fixtureHash), actual $fixtureHash."
        }
        $manifestHash = (Get-FileHash -LiteralPath $resolvedManifest -Algorithm SHA256).Hash.ToLowerInvariant()
        $manifestHashPath = Join-Path $manifestRoot "manifest.sha256"
        if (Test-Path -LiteralPath $manifestHashPath) {
            $expectedManifestHash = ([System.IO.File]::ReadAllText($manifestHashPath)).Trim()
            if ($manifestHash -cne $expectedManifestHash) {
                throw "Generated fixture manifest hash mismatch: expected $expectedManifestHash, actual $manifestHash."
            }
        }
        if (-not [string]::IsNullOrWhiteSpace($ExpectedHash) -and $fixtureHash -cne $ExpectedHash) {
            throw "Fixture hash mismatch: expected $ExpectedHash, actual $fixtureHash."
        }
        return [pscustomobject][ordered]@{
            source = "generated-manifest"
            fixtureHash = $fixtureHash
            manifestPath = $resolvedManifest.Replace("\", "/")
            manifestSha256 = $manifestHash
            manifestCase = $ManifestCase
            aggregatePreprocessSha256 = $null
            inputCount = $records.Count
            inputs = @($records)
        }
    }

    $comparison = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        [System.StringComparer]::OrdinalIgnoreCase
    }
    else {
        [System.StringComparer]::Ordinal
    }
    $inputPaths = [System.Collections.Generic.HashSet[string]]::new($comparison)
    [void]$inputPaths.Add($resolvedProject)
    foreach ($explicitInput in @($ExplicitInputPaths)) {
        if (-not [string]::IsNullOrWhiteSpace($explicitInput)) {
            [void]$inputPaths.Add((Resolve-Path -LiteralPath $explicitInput -ErrorAction Stop).Path)
        }
    }
    $aggregateHash = $null
    if (-not [string]::IsNullOrWhiteSpace($AggregatePreprocessPath)) {
        $resolvedAggregate = (Resolve-Path -LiteralPath $AggregatePreprocessPath -ErrorAction Stop).Path
        $aggregate = [System.IO.File]::ReadAllText($resolvedAggregate).TrimStart([char]0xFEFF)
        $aggregate = $aggregate.Replace("`r`n", "`n").Replace("`r", "`n")
        foreach ($line in $aggregate -split "`n") {
            $candidate = $line.Trim()
            if (-not [string]::IsNullOrWhiteSpace($candidate) -and
                [System.IO.Path]::IsPathRooted($candidate) -and
                (Test-Path -LiteralPath $candidate -PathType Leaf)) {
                [void]$inputPaths.Add((Resolve-Path -LiteralPath $candidate).Path)
            }
        }
        $normalizedAggregate = $aggregate
        foreach ($replacement in @(
                @{ Path = $projectDirectory; Token = "<PROJECT_DIRECTORY>" },
                @{ Path = $SdkPath; Token = "<MSBUILD_SDKS_PATH>" }
            )) {
            if (-not [string]::IsNullOrWhiteSpace($replacement.Path)) {
                $normalizedAggregate = $normalizedAggregate.Replace(
                    [string]$replacement.Path,
                    [string]$replacement.Token,
                    [System.StringComparison]::OrdinalIgnoreCase)
                $normalizedAggregate = $normalizedAggregate.Replace(
                    ([string]$replacement.Path).Replace("\", "/"),
                    [string]$replacement.Token,
                    [System.StringComparison]::OrdinalIgnoreCase)
            }
        }
        $aggregateHash = Get-PerformanceSha256Text $normalizedAggregate
    }

    $records = @(
        foreach ($inputPath in $inputPaths) {
            $item = Get-Item -LiteralPath $inputPath
            [pscustomobject][ordered]@{
                path = ConvertTo-PerformanceInputPath $item.FullName $projectDirectory $SdkPath
                resolvedPath = $item.FullName.Replace("\", "/")
                bytes = [long]$item.Length
                sha256 = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
    ) | Sort-Object path
    $fixtureHash = Get-PerformanceRecordsHash $records
    if (-not [string]::IsNullOrWhiteSpace($ExpectedHash) -and $fixtureHash -cne $ExpectedHash) {
        throw "Fixture hash mismatch: expected $ExpectedHash, actual $fixtureHash."
    }
    return [pscustomobject][ordered]@{
        source = if ($null -ne $aggregateHash) {
            "preprocess-import-list"
        }
        elseif (@($ExplicitInputPaths).Count -gt 0) {
            "explicit-input-list"
        }
        else {
            "root-project"
        }
        fixtureHash = $fixtureHash
        manifestPath = $null
        manifestSha256 = $null
        manifestCase = $null
        aggregatePreprocessSha256 = $aggregateHash
        inputCount = $records.Count
        inputs = @($records)
    }
}

function Format-PerformanceCommand {
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

function Invoke-PerformanceProcess {
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

    Write-PerformanceUtf8File $StdoutPath $stdout
    Write-PerformanceUtf8File $StderrPath $stderr
    return [pscustomobject][ordered]@{
        elapsedWallMs = $stopwatch.Elapsed.TotalMilliseconds
        peakWorkingSetBytes = [long]$peakWorkingSet
        exitCode = [int]$exitCode
    }
}

function Get-PerformanceMedian {
    param([Parameter(Mandatory = $true)][double[]]$Values)

    $sorted = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 0) {
        return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2
    }
    return [double]$sorted[$middle]
}

function Get-PerformanceDistribution {
    param([Parameter(Mandatory = $true)][double[]]$Values)

    if ($Values.Count -eq 0) {
        throw "Cannot summarize an empty sample set."
    }
    $sorted = @($Values | Sort-Object)
    $median = Get-PerformanceMedian $Values
    $deviations = [double[]]@($Values | ForEach-Object { [Math]::Abs($_ - $median) })
    $p95Index = [Math]::Min($sorted.Count - 1, [int][Math]::Ceiling($sorted.Count * 0.95) - 1)
    return [pscustomobject][ordered]@{
        min = [Math]::Round([double]$sorted[0], 3)
        median = [Math]::Round($median, 3)
        mean = [Math]::Round([double](($Values | Measure-Object -Average).Average), 3)
        p95 = [Math]::Round([double]$sorted[$p95Index], 3)
        max = [Math]::Round([double]$sorted[-1], 3)
        mad = [Math]::Round((Get-PerformanceMedian $deviations), 3)
    }
}

function Get-PerformanceHostCpuName {
    if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)) {
        try {
            $name = (Get-CimInstance Win32_Processor -ErrorAction Stop |
                    Select-Object -First 1 -ExpandProperty Name).Trim()
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                return $name
            }
        }
        catch {
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
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_IDENTIFIER)) {
        return $env:PROCESSOR_IDENTIFIER
    }
    return [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
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
