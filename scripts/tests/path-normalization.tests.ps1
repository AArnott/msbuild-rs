$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "..\path-normalization.ps1")

function Assert-Equal {
    param([string]$Expected, [string]$Actual, [string]$Message)

    if ($Expected -cne $Actual) {
        throw "$Message Expected '$Expected', got '$Actual'."
    }
}

$input = "C:\Fixture\file.txt"
Assert-Equal "<ROOT>\file.txt" (
    Replace-PathForComparison -InputText $input -Path "c:\fixture" -Token "<ROOT>" -UseCaseInsensitivePaths $true
) "Windows path replacement should ignore case."
Assert-Equal $input (
    Replace-PathForComparison -InputText $input -Path "c:\fixture" -Token "<ROOT>" -UseCaseInsensitivePaths $false
) "Non-Windows path replacement should preserve case distinctions."
Assert-Equal "<ROOT>\file.txt" (
    Replace-PathForComparison -InputText $input -Path "C:\Fixture" -Token "<ROOT>" -UseCaseInsensitivePaths $false
) "Non-Windows path replacement should replace an exact-case path."

Write-Host "Path normalization tests passed."
