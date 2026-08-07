function Get-PathNormalizationRegexOptions {
    param(
        [bool]$UseCaseInsensitivePaths = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)
    )

    if ($UseCaseInsensitivePaths) {
        return [System.Text.RegularExpressions.RegexOptions]::IgnoreCase
    }

    return [System.Text.RegularExpressions.RegexOptions]::None
}

function Replace-PathForComparison {
    param(
        [string]$InputText,
        [string]$Path,
        [string]$Token,
        [bool]$UseCaseInsensitivePaths = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows)
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $InputText
    }

    return [regex]::Replace(
        $InputText,
        [regex]::Escape($Path),
        $Token,
        (Get-PathNormalizationRegexOptions -UseCaseInsensitivePaths $UseCaseInsensitivePaths))
}
