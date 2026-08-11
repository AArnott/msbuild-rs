function ConvertTo-SemanticXmlProjection {
    param([Parameter(Mandatory = $true)][string]$Content)

    $document = [System.Xml.XmlDocument]::new()
    $document.PreserveWhitespace = $true
    $document.XmlResolver = $null
    $document.LoadXml($Content)
    $lines = [System.Collections.Generic.List[string]]::new()
    $structuralContainers = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase)
    foreach ($name in @(
            "Project",
            "PropertyGroup",
            "ItemGroup",
            "ItemDefinitionGroup",
            "ImportGroup",
            "Target",
            "Choose",
            "When",
            "Otherwise",
            "UsingTask",
            "ParameterGroup",
            "Sdk"
        )) {
        $structuralContainers.Add($name) | Out-Null
    }
    $escapeValue = {
        param([AllowEmptyString()][string]$Value)

        return $Value.Replace("\", "\\").Replace("`r`n", "`n").Replace("`r", "`n").Replace("`n", "\n")
    }
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
                        "$($_.LocalName)=$(& $escapeValue $_.Value)"
                    } |
                    Sort-Object
            )
            $lines.Add("$("  " * $Depth)E:$($Node.LocalName)|$($attributes -join "|")") | Out-Null
            foreach ($child in $Node.ChildNodes) {
                & $visit $child ($Depth + 1)
            }
            $lines.Add("$("  " * $Depth)X:$($Node.LocalName)") | Out-Null
            return
        }

        if ($Node.NodeType -notin @(
                [System.Xml.XmlNodeType]::Text,
                [System.Xml.XmlNodeType]::CDATA,
                [System.Xml.XmlNodeType]::Whitespace,
                [System.Xml.XmlNodeType]::SignificantWhitespace
            )) {
            return
        }

        $hasElementSibling = @(
            $Node.ParentNode.ChildNodes |
                Where-Object { $_.NodeType -eq [System.Xml.XmlNodeType]::Element }
        ).Count -gt 0
        $hasContentSibling = @(
            $Node.ParentNode.ChildNodes |
                Where-Object {
                    $_ -ne $Node -and
                    $_.NodeType -in @(
                        [System.Xml.XmlNodeType]::Text,
                        [System.Xml.XmlNodeType]::CDATA,
                        [System.Xml.XmlNodeType]::SignificantWhitespace
                    ) -and
                    -not [string]::IsNullOrWhiteSpace($_.Value)
                }
        ).Count -gt 0
        $parentIsStructuralChild =
            $null -ne $Node.ParentNode.ParentNode -and
            $Node.ParentNode.ParentNode.LocalName -in @(
                "Target",
                "ItemGroup",
                "ItemDefinitionGroup",
                "UsingTask",
                "ParameterGroup"
            )
        $isStructuralIndentation =
            $Node.NodeType -eq [System.Xml.XmlNodeType]::Whitespace -and
            ($hasElementSibling -or $hasContentSibling -or
                $structuralContainers.Contains($Node.ParentNode.LocalName) -or
                $parentIsStructuralChild)
        if (-not $isStructuralIndentation) {
            $lines.Add("$("  " * $Depth)T:$(& $escapeValue $Node.Value)") | Out-Null
        }
    }
    & $visit $document.DocumentElement 0
    return [string]::Join("`n", $lines)
}
