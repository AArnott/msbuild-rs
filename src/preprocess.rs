use anyhow::{Context, Result, anyhow, bail};
use log::debug;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::cell::RefCell;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::expression::ExpressionEvaluator;
use crate::object_model::ProjectModel;

const BOUNDARY: &str = "============================================================================================================================================";

pub struct ProjectPreprocessor<'a> {
    model: &'a ProjectModel,
    evaluation_state: RefCell<ProjectModel>,
    sdk_root: Option<PathBuf>,
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
        Self {
            model,
            evaluation_state: RefCell::new(model.clone()),
            sdk_root: discover_sdk_root(),
        }
    }

    #[cfg(test)]
    fn with_sdk_root(model: &'a ProjectModel, sdk_root: PathBuf) -> Self {
        Self {
            model,
            evaluation_state: RefCell::new(model.clone()),
            sdk_root: Some(sdk_root),
        }
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
        let mut in_property_group = false;
        let mut current_property: Option<String> = None;
        let mut current_property_value = String::new();

        loop {
            let event_start = reader.buffer_position() as usize;
            let event = reader.read_event()?;
            let event_end = reader.buffer_position() as usize;

            match event {
                Event::Start(element) if element.name().as_ref() == b"PropertyGroup" => {
                    let condition = attribute_value(&element, &reader, b"Condition")?;
                    in_property_group = match condition {
                        Some(condition) => {
                            let model = self.evaluation_model(&canonical_path);
                            ExpressionEvaluator::with_base_directory(
                                &model,
                                canonical_path.parent().unwrap_or_else(|| Path::new("")),
                            )
                            .evaluate_condition(&condition)
                            .with_context(|| {
                                format!(
                                    "Failed to evaluate property group condition '{condition}' in {}",
                                    canonical_path.display()
                                )
                            })?
                        }
                        None => true,
                    };
                }
                Event::Start(element) if in_property_group => {
                    let condition = attribute_value(&element, &reader, b"Condition")?;
                    let should_set = match condition {
                        Some(condition) => {
                            let model = self.evaluation_model(&canonical_path);
                            ExpressionEvaluator::with_base_directory(
                                &model,
                                canonical_path.parent().unwrap_or_else(|| Path::new("")),
                            )
                            .evaluate_condition(&condition)
                            .with_context(|| {
                                format!(
                                    "Failed to evaluate property condition '{condition}' in {}",
                                    canonical_path.display()
                                )
                            })?
                        }
                        None => true,
                    };
                    if should_set {
                        current_property =
                            Some(String::from_utf8_lossy(element.name().as_ref()).into_owned());
                        current_property_value.clear();
                    }
                }
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
                Event::Text(text) if current_property.is_some() => {
                    current_property_value.push_str(&text.decode()?);
                }
                Event::GeneralRef(reference) if current_property.is_some() => {
                    let encoded = format!("&{};", reference.decode()?);
                    current_property_value.push_str(&quick_xml::escape::unescape(&encoded)?);
                }
                Event::CData(data) if current_property.is_some() => {
                    current_property_value.push_str(&data.decode()?);
                }
                Event::End(element)
                    if current_property.as_deref()
                        == Some(String::from_utf8_lossy(element.name().as_ref()).as_ref()) =>
                {
                    let name = current_property.take().unwrap();
                    let raw_value = current_property_value.trim();
                    let model = self.evaluation_model(&canonical_path);
                    let value = ExpressionEvaluator::with_base_directory(
                        &model,
                        canonical_path.parent().unwrap_or_else(|| Path::new("")),
                    )
                    .evaluate(raw_value)?;
                    self.evaluation_state.borrow_mut().set_property(name, value);
                    current_property_value.clear();
                }
                Event::End(element) if element.name().as_ref() == b"PropertyGroup" => {
                    in_property_group = false;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        output.push_str(&source[cursor..content_end]);
        import_stack.remove(&canonical_path);
        if let Some(sdk) = project_sdk(&source)? {
            self.expand_implicit_sdk_imports(
                output,
                &sdk,
                &canonical_path,
                include_project_element,
                import_stack,
            )
        } else {
            Ok(output)
        }
    }

    fn expand_implicit_sdk_imports(
        &self,
        project: String,
        sdk: &str,
        project_path: &Path,
        include_project_element: bool,
        import_stack: &mut HashSet<PathBuf>,
    ) -> Result<String> {
        let sdk_directory = self.resolve_sdk(sdk)?;
        let props_path = sdk_directory.join("Sdk.props");
        let targets_path = sdk_directory.join("Sdk.targets");
        let props = self.expand_file(&props_path, false, import_stack)?;
        let targets = self.expand_file(&targets_path, false, import_stack)?;
        let props_marker = implicit_sdk_marker("Sdk.props", sdk, &props_path, &props, true);
        let targets_marker =
            implicit_sdk_marker("Sdk.targets", sdk, &targets_path, &targets, false);

        if include_project_element {
            let opening_end = project
                .find('>')
                .ok_or_else(|| anyhow!("Project root opening tag was not found"))?
                + 1;
            let closing_start = project
                .rfind("</Project>")
                .ok_or_else(|| anyhow!("Project root closing tag was not found"))?;
            Ok(format!(
                "{}\n{}{}\n{}{}",
                &project[..opening_end],
                props_marker,
                &project[opening_end..closing_start],
                targets_marker,
                &project[closing_start..]
            ))
        } else {
            Ok(format!(
                "{props_marker}{project}\n{targets_marker}<!-- SDK project: {} -->",
                display_path(project_path)
            ))
        }
    }

    fn resolve_sdk(&self, sdk: &str) -> Result<PathBuf> {
        let sdk_name = sdk.split('/').next().unwrap_or(sdk);
        let sdk_root = self.sdk_root.as_ref().ok_or_else(|| {
            anyhow!(
                "Could not locate the .NET SDK directory. Set MSBuildSDKsPath to the installed Sdks directory"
            )
        })?;
        let sdk_directory = sdk_root.join(sdk_name).join("Sdk");
        if !sdk_directory.is_dir() {
            bail!(
                "SDK '{sdk_name}' was not found under {}",
                sdk_root.display()
            );
        }
        Ok(sdk_directory)
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
        let evaluation_model = self.evaluation_model(importing_path);
        let evaluator = ExpressionEvaluator::with_base_directory(
            &evaluation_model,
            importing_path.parent().unwrap_or_else(|| Path::new("")),
        );
        let evaluated_project = evaluator.evaluate(&import.project)?;
        if evaluated_project.contains("$(") || evaluated_project.contains("@(") {
            bail!(
                "Unsupported property function in import path '{evaluated_project}' from {}",
                importing_path.display()
            );
        }
        let import_path = importing_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(evaluated_project);
        debug!(
            "Resolved import '{}' from {} to {}",
            import.project,
            importing_path.display(),
            import_path.display()
        );

        if let Some(condition) = import.condition
            && !evaluator.evaluate_condition(&condition).with_context(|| {
                format!(
                    "Failed to evaluate import condition '{condition}' in {}",
                    importing_path.display()
                )
            })?
        {
            output.push_str(source_element);
            return Ok(());
        }

        let import_paths = resolve_import_paths(&import_path, importing_path)?;
        debug!("Import matched {} file(s)", import_paths.len());
        for canonical_import_path in import_paths {
            self.expand_resolved_import(
                source_element,
                indentation,
                importing_path,
                import_stack,
                output,
                &canonical_import_path,
            )?;
        }
        Ok(())
    }

    fn expand_resolved_import(
        &self,
        source_element: &str,
        indentation: &str,
        importing_path: &Path,
        import_stack: &mut HashSet<PathBuf>,
        output: &mut String,
        canonical_import_path: &Path,
    ) -> Result<()> {
        let declaration = source_element
            .trim_end()
            .strip_suffix("/>")
            .map(|value| format!("{}{}>", indentation, value.trim_end()))
            .unwrap_or_else(|| source_element.to_string());

        output.push_str(&format!(
            "<!--\n{BOUNDARY}\n{declaration}\n\n{}\n{BOUNDARY}\n-->\n",
            display_path(canonical_import_path)
        ));
        let imported = self.expand_file(canonical_import_path, false, import_stack)?;
        output.push_str(imported.trim_matches(['\r', '\n']));
        output.push_str(&format!(
            "\n{indentation}<!--\n{BOUNDARY}\n{indentation}</Import>\n\n{}\n{BOUNDARY}\n-->",
            display_path(importing_path)
        ));
        Ok(())
    }

    fn evaluation_model(&self, importing_path: &Path) -> ProjectModel {
        let mut model = self.evaluation_state.borrow().clone();
        for (name, value) in env::vars() {
            if model.get_property(&name).is_none() {
                model.set_property(name, value);
            }
        }

        if let Some(sdk_root) = &self.sdk_root
            && let Some(extensions_path) = sdk_root.parent()
        {
            model.set_property(
                "MSBuildExtensionsPath".to_string(),
                display_path(extensions_path),
            );
            model.set_property("MSBuildToolsVersion".to_string(), "Current".to_string());
            model.set_property("MSBuildSDKsPath".to_string(), display_path(sdk_root));
            if let Some(version) = extensions_path.file_name() {
                model.set_property(
                    "NETCoreSdkVersion".to_string(),
                    version.to_string_lossy().into_owned(),
                );
            }
        }

        set_path_properties(&mut model, "MSBuildThisFile", importing_path);
        if let Some(project_path) = &self.model.project_file_path {
            set_path_properties(&mut model, "MSBuildProject", project_path);
        }
        model
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

fn resolve_import_paths(import_path: &Path, importing_path: &Path) -> Result<Vec<PathBuf>> {
    let pattern = import_path.to_string_lossy();
    if pattern.contains(['*', '?', '[']) {
        let pattern = pattern.replace('\\', "/");
        let mut paths = glob::glob(&pattern)?
            .filter_map(|entry| entry.ok())
            .filter(|path| path.is_file())
            .filter_map(|path| path.canonicalize().ok())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        return Ok(paths);
    }

    Ok(vec![import_path.canonicalize().with_context(|| {
        format!(
            "Imported project '{}' was not found from {}",
            import_path.display(),
            importing_path.display()
        )
    })?])
}

fn project_sdk(source: &str) -> Result<Option<String>> {
    let mut reader = Reader::from_str(source);
    loop {
        match reader.read_event()? {
            Event::Start(element) if element.name().as_ref() == b"Project" => {
                return attribute_value(&element, &reader, b"Sdk");
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

fn implicit_sdk_marker(
    file_name: &str,
    sdk: &str,
    path: &Path,
    content: &str,
    is_props: bool,
) -> String {
    let suffix = if is_props { "" } else { "\n" };
    format!(
        "  <!--\n{BOUNDARY}\n  <Import Project=\"{file_name}\" Sdk=\"{sdk}\">\n  This import was added implicitly because the Project element's Sdk attribute specified \"{sdk}\".\n\n{}\n{BOUNDARY}\n-->\n{}\n  <!--\n{BOUNDARY}\n  </Import>\n\n{}\n{BOUNDARY}\n-->{suffix}",
        display_path(path),
        content.trim_matches(['\r', '\n']),
        display_path(path)
    )
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

fn set_path_properties(model: &mut ProjectModel, prefix: &str, path: &Path) {
    let full_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let directory = full_path.parent().unwrap_or_else(|| Path::new(""));
    let separator = std::path::MAIN_SEPARATOR;
    model.set_property(format!("{prefix}FullPath"), display_path(&full_path));
    model.set_property(
        format!("{prefix}Directory"),
        format!("{}{separator}", display_path(directory)),
    );
    model.set_property(
        format!("{prefix}Name"),
        full_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );
    model.set_property(
        format!("{prefix}Extension"),
        full_path
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default(),
    );
    if prefix == "MSBuildThisFile" {
        model.set_property(
            "MSBuildThisFile".to_string(),
            full_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
    }
    if prefix == "MSBuildProject" {
        model.set_property(
            "MSBuildProjectDirectory".to_string(),
            display_path(directory),
        );
        model.set_property(
            "MSBuildProjectFile".to_string(),
            full_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
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
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn discover_sdk_root() -> Option<PathBuf> {
    if let Some(path) = env::var_os("MSBuildSDKsPath") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Some(path);
        }
    }

    let dotnet_root = env::var_os("DOTNET_ROOT")
        .map(PathBuf::from)
        .or_else(default_dotnet_root)?;
    let sdk_directory = dotnet_root.join("sdk");
    let mut versions = fs::read_dir(sdk_directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("Sdks").is_dir())
        .collect::<Vec<_>>();
    versions.sort_by_key(|entry| version_key(&entry.file_name().to_string_lossy()));
    versions.last().map(|entry| entry.path().join("Sdks"))
}

fn default_dotnet_root() -> Option<PathBuf> {
    if cfg!(windows) {
        env::var_os("ProgramFiles").map(|path| PathBuf::from(path).join("dotnet"))
    } else {
        [
            PathBuf::from("/usr/share/dotnet"),
            PathBuf::from("/usr/local/share/dotnet"),
        ]
        .into_iter()
        .find(|path| path.is_dir())
    }
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
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

    #[test]
    fn strips_bom_and_inlines_implicit_sdk_imports() -> Result<()> {
        let directory = TempDir::new()?;
        let project_path = directory.path().join("sdk.proj");
        let sdk_root = directory.path().join("Sdks");
        let sdk_directory = sdk_root.join("Test.Sdk").join("Sdk");
        let output_path = directory.path().join("out.xml");
        fs::create_dir_all(&sdk_directory)?;
        fs::write(
            &project_path,
            "\u{feff}<Project Sdk=\"Test.Sdk\">\n  <PropertyGroup><FromProject>true</FromProject></PropertyGroup>\n</Project>",
        )?;
        fs::write(
            sdk_directory.join("Sdk.props"),
            "\u{feff}<Project><PropertyGroup><FromSdkProps>true</FromSdkProps><NestedImport>nested.props</NestedImport></PropertyGroup><Import Project=\"$(NestedImport)\" /></Project>",
        )?;
        fs::write(
            sdk_directory.join("nested.props"),
            "<Project><PropertyGroup><FromNestedSdkImport>true</FromNestedSdkImport></PropertyGroup></Project>",
        )?;
        fs::write(
            sdk_directory.join("Sdk.targets"),
            "<Project><Target Name=\"FromSdkTargets\" /></Project>",
        )?;

        let mut model = ProjectModel::new();
        model.set_project_file_path(project_path);
        ProjectPreprocessor::with_sdk_root(&model, sdk_root).write(&output_path)?;

        let output = fs::read_to_string(output_path)?;
        assert!(!output.contains('\u{feff}'));
        let props = output.find("<FromSdkProps>true</FromSdkProps>").unwrap();
        let nested = output
            .find("<FromNestedSdkImport>true</FromNestedSdkImport>")
            .unwrap();
        let project = output.find("<FromProject>true</FromProject>").unwrap();
        let targets = output.find("<Target Name=\"FromSdkTargets\" />").unwrap();
        assert!(props < project);
        assert!(props < nested);
        assert!(nested < project);
        assert!(project < targets);
        assert!(output.contains("This import was added implicitly"));
        Ok(())
    }
}
