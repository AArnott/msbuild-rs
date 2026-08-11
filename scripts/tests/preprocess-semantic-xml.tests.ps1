$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "..\preprocess-semantic-xml.ps1")

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)
    if ($Expected -cne $Actual) {
        throw "$Message`nExpected: $Expected`nActual:   $Actual"
    }
}

function Assert-NotEqual {
    param($Left, $Right, [string]$Message)
    if ($Left -ceq $Right) {
        throw "$Message`nBoth projections:`n$Left"
    }
}

$spacedProperty = ConvertTo-SemanticXmlProjection "<Project><PropertyGroup><P> x </P></PropertyGroup></Project>"
$plainProperty = ConvertTo-SemanticXmlProjection "<Project><PropertyGroup><P>x</P></PropertyGroup></Project>"
Assert-NotEqual $spacedProperty $plainProperty "Property content padding must be significant."

$spacedMetadata = ConvertTo-SemanticXmlProjection "<Project><ItemGroup><I Include='i'><M> y </M></I></ItemGroup></Project>"
$plainMetadata = ConvertTo-SemanticXmlProjection "<Project><ItemGroup><I Include='i'><M>y</M></I></ItemGroup></Project>"
Assert-NotEqual $spacedMetadata $plainMetadata "Metadata content padding must be significant."

$formatted = ConvertTo-SemanticXmlProjection @"
<Project>
  <PropertyGroup>
    <P>x</P>
  </PropertyGroup>
</Project>
"@
$compact = ConvertTo-SemanticXmlProjection "<Project><PropertyGroup><P>x</P></PropertyGroup></Project>"
Assert-Equal $compact $formatted "Structural indentation should not affect the projection."

$whitespaceOnly = ConvertTo-SemanticXmlProjection "<Project><PropertyGroup><P> </P></PropertyGroup></Project>"
$empty = ConvertTo-SemanticXmlProjection "<Project><PropertyGroup><P /></PropertyGroup></Project>"
Assert-NotEqual $whitespaceOnly $empty "Whitespace-only leaf content must not be discarded."

Write-Host "Semantic XML projection tests passed."
