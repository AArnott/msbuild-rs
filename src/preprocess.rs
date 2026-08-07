use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::expression::ExpressionEvaluator;
use crate::object_model::ProjectModel;

const BOUNDARY: &str = "============================================================================================================================================";

pub struct ProjectPreprocessor<'a> {
    model: &'a ProjectModel,
}

struct ImportAttributes {
    project: String,
    condition: Option<String>,
}

impl ImportAttributes {
    fn parse(element: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<Self> {
        Ok(Self {
            project: attribute_value(element, reader, b"Project")?
                .ok_or_else(|| anyhow!("Import is missing its Project attribute"))?,
            condition: attribute_value(element, reader, b"Condition")?,
        })
    }
}

impl<'a> ProjectPreprocessor<'a> {
    pub fn new(model: &'a ProjectModel) -> Self {
        Self { model }
    }

    pub fn write<P: AsRef<Path>>(&self, output_path: P) -> Result<()> {
        let project_path = self
            .model
            .project_file_path
            .as_ref()
            .ok_or_else(|| anyhow!("Cannot preprocess a project that has not been loaded"))?;
        let project_path = project_path.canonicalize()?;
        let mut import_stack = HashSet::new();
        let body = self.expand_file(&project_path, true, &mut import_stack)?;
        let output = format!(
            "<!--\n{BOUNDARY}\n{}\n{BOUNDARY}\n-->\n{body}",
            display_path(&project_path)
        );
        fs::write(output_path, normalize_output(&output))?;
        Ok(())
    }

    fn expand_file(
        &self,
        path: &Path,
        include_project_element: bool,
        import_stack: &mut HashSet<PathBuf>,
    ) -> Result<String> {
        let canonical_path = path.canonicalize()?;
        if !import_stack.insert(canonical_path.clone()) {
            bail!("Circular import detected at {}", canonical_path.display());
        }

        let source = fs::read_to_string(&canonical_path)
            .with_context(|| format!("Failed to read project {}", canonical_path.display()))?;
        let source = normalize_source(&source);
        let (content_start, content_end) =
            project_content_bounds(&source, include_project_element)?;
        let mut reader = Reader::from_str(&source);
        reader.config_mut().trim_text(false);
        let mut output = String::new();
        let mut cursor = content_start;

        loop {
            let event_start = reader.buffer_position() as usize;
            let event = reader.read_event()?;
            let event_end = reader.buffer_position() as usize;

            match event {
                Event::Empty(element)
                    if element.name().as_ref() == b"Import"
                        && event_start >= content_start
                        && event_end <= content_end =>
                {
                    output.push_str(&source[cursor..event_start]);
                    let line_start = source[..event_start]
                        .rfind(['\r', '\n'])
                        .map_or(0, |position| position + 1);
                    let indentation = &source[line_start..event_start];
                    let import = ImportAttributes::parse(&element, &reader)?;
                    self.expand_import(
                        &source[event_start..event_end],
                        indentation,
                        import,
                        &canonical_path,
                        import_stack,
                        &mut output,
                    )?;
                    cursor = event_end;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        output.push_str(&source[cursor..content_end]);
        import_stack.remove(&canonical_path);
        Ok(output)
    }

    fn expand_import(
        &self,
        source_element: &str,
        indentation: &str,
        import: ImportAttributes,
        importing_path: &Path,
        import_stack: &mut HashSet<PathBuf>,
        output: &mut String,
    ) -> Result<()> {
        let evaluated_project = ExpressionEvaluator::new(self.model).evaluate(&import.project)?;
        let import_path = importing_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(evaluated_project);

        if let Some(condition) = import.condition
            && !evaluate_import_condition(&condition, &import_path, self.model)?
        {
            output.push_str(source_element);
            return Ok(());
        }

        let canonical_import_path = import_path.canonicalize().with_context(|| {
            format!(
                "Imported project '{}' was not found from {}",
                import_path.display(),
                importing_path.display()
            )
        })?;
        let declaration = source_element
            .trim_end()
            .strip_suffix("/>")
            .map(|value| format!("{}{}>", indentation, value.trim_end()))
            .unwrap_or_else(|| source_element.to_string());

        output.push_str(&format!(
            "<!--\n{BOUNDARY}\n{declaration}\n\n{}\n{BOUNDARY}\n-->\n",
            display_path(&canonical_import_path)
        ));
        let imported = self.expand_file(&canonical_import_path, false, import_stack)?;
        output.push_str(imported.trim_matches(['\r', '\n']));
        output.push_str(&format!(
            "\n{indentation}<!--\n{BOUNDARY}\n{indentation}</Import>\n\n{}\n{BOUNDARY}\n-->",
            display_path(importing_path)
        ));
        Ok(())
    }
}

fn project_content_bounds(source: &str, include_project_element: bool) -> Result<(usize, usize)> {
    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(false);
    let mut content_start = None;

    loop {
        let event_start = reader.buffer_position() as usize;
        let event = reader.read_event()?;
        let event_end = reader.buffer_position() as usize;
        match event {
            Event::Start(element) if element.name().as_ref() == b"Project" => {
                content_start = Some(if include_project_element {
                    event_start
                } else {
                    event_end
                });
            }
            Event::End(element) if element.name().as_ref() == b"Project" => {
                let start =
                    content_start.ok_or_else(|| anyhow!("Project root start was not found"))?;
                let end = if include_project_element {
                    event_end
                } else {
                    event_start
                };
                return Ok((start, end));
            }
            Event::Eof => bail!("Project root element was not found"),
            _ => {}
        }
    }
}

fn attribute_value(
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    name: &[u8],
) -> Result<Option<String>> {
    for attribute in element.attributes() {
        let attribute = attribute?;
        if attribute.key.as_ref() == name {
            return Ok(Some(
                attribute
                    .decoded_and_normalized_value(
                        quick_xml::XmlVersion::Implicit1_0,
                        reader.decoder(),
                    )?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn evaluate_import_condition(
    condition: &str,
    import_path: &Path,
    model: &ProjectModel,
) -> Result<bool> {
    let exists = regex::Regex::new(r#"(?i)^\s*Exists\(\s*['\"][^'\"]*['\"]\s*\)\s*$"#).unwrap();
    if exists.is_match(condition) {
        return Ok(import_path.exists());
    }
    ExpressionEvaluator::new(model).evaluate_condition(condition)
}

fn display_path(path: &Path) -> String {
    let display = path.display().to_string();
    if let Some(path) = display.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        display
            .strip_prefix(r"\\?\")
            .unwrap_or(&display)
            .to_string()
    }
}

fn normalize_output(output: &str) -> String {
    let normalized = output.replace("\r\n", "\n").replace('\r', "\n");
    if cfg!(windows) {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

fn normalize_source(source: &str) -> String {
    source
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ProjectParser;
    use quick_xml::Reader;
    use tempfile::TempDir;

    #[test]
    fn preserves_source_and_inlines_property_evaluated_imports() -> Result<()> {
        let directory = TempDir::new()?;
        let project_path = directory.path().join("main.proj");
        let import_directory = directory.path().join("imports");
        let import_path = import_directory.join("common.props");
        let output_path = directory.path().join("out.xml");
        fs::create_dir(&import_directory)?;
        fs::write(
            &project_path,
            r#"<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build">
  <!-- Keep this comment. -->
    <PropertyGroup>
        <ImportDirectory>imports</ImportDirectory>
        <ImportFile>common.props</ImportFile>
    </PropertyGroup>
    <Import Project="$(ImportDirectory)/$(ImportFile)" Condition="Exists('$(ImportDirectory)/$(ImportFile)')" />
  <Target Name="Build"><Message Text="$(Greeting): @(Compile)" /></Target>
</Project>"#,
        )?;
        fs::write(
            &import_path,
            r#"<Project>
  <PropertyGroup><Greeting>A &amp; B</Greeting></PropertyGroup>
  <ItemGroup><Compile Include="one.cs;two.cs" /></ItemGroup>
</Project>"#,
        )?;

        let mut parser = ProjectParser::new();
        let mut model = parser.parse_file(&project_path)?;
        model.set_project_file_path(project_path.clone());
        ProjectPreprocessor::new(&model).write(&output_path)?;

        let output = fs::read_to_string(output_path)?;
        assert!(output.contains("<!-- Keep this comment. -->"));
        assert!(output.contains("Project=\"$(ImportDirectory)/$(ImportFile)\""));
        assert!(output.contains("Condition=\"Exists('$(ImportDirectory)/$(ImportFile)')\""));
        assert!(output.contains("<Greeting>A &amp; B</Greeting>"));
        assert!(output.contains("Text=\"$(Greeting): @(Compile)\""));
        assert!(!output.contains("<Project>\n  <PropertyGroup>"));
        assert_eq!(output.matches("<Project").count(), 1);
        assert_eq!(output.matches("</Project>").count(), 1);

        let mut reader = Reader::from_str(&output);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(error) => panic!("preprocessed output is invalid XML: {error}"),
            }
        }
        Ok(())
    }
}
