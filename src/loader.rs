use anyhow::{Context, Result, anyhow, bail};
use log::debug;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::evaluation::EvaluationContext;
use crate::expression::ExpressionEvaluator;
use crate::object_model::{Import, Item, ProjectModel, PropertyMap, Target, Task};
use crate::properties::{display_path, is_reserved_property, set_reserved_project_properties};

const BOUNDARY: &str = "============================================================================================================================================";

pub(crate) struct LoadOutput {
    pub model: ProjectModel,
    pub preprocessed: Option<String>,
}

struct EvaluationState {
    model: ProjectModel,
    global_properties: PropertyMap,
    import_stack: HashSet<PathBuf>,
    render_preprocessed: bool,
    sdk_root: Option<PathBuf>,
    // A no-allocation seam for a future opt-in diagnostic sink.
    _diagnostics: DiagnosticMode,
}

#[derive(Default)]
enum DiagnosticMode {
    #[default]
    None,
}

#[derive(Debug)]
struct PendingProperty {
    name: String,
    value: String,
    eligible: bool,
}

#[derive(Debug)]
struct PendingItem {
    item_type: String,
    include: Option<String>,
    condition: Option<String>,
    metadata: HashMap<String, String>,
}

#[derive(Debug)]
struct ImportAttributes {
    project: String,
    condition: Option<String>,
}

pub(crate) fn load_project(
    context: &EvaluationContext,
    path: &Path,
    render_preprocessed: bool,
) -> Result<LoadOutput> {
    let project_path = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve project {}", path.display()))?;
    let mut state = EvaluationState::new(context, &project_path, render_preprocessed)?;
    let body = state.evaluate_file(&project_path, true)?;
    let preprocessed = render_preprocessed.then(|| {
        normalize_output(&format!(
            "<!--\n{BOUNDARY}\n{}\n{BOUNDARY}\n-->\n{body}",
            display_path(&project_path)
        ))
    });
    Ok(LoadOutput {
        model: state.model,
        preprocessed,
    })
}

impl EvaluationState {
    fn new(
        context: &EvaluationContext,
        project_path: &Path,
        render_preprocessed: bool,
    ) -> Result<Self> {
        let mut model = ProjectModel::new();
        model.set_project_file_path(project_path.to_path_buf());

        for (name, value) in context.environment().iter() {
            model.set_property(name.clone(), value.clone());
        }

        if let Some(sdk_root) = context.sdk_root()
            && let Some(extensions_path) = sdk_root.parent()
        {
            set_default_property(
                &mut model,
                "MSBuildExtensionsPath",
                display_path(extensions_path),
            );
            set_default_property(&mut model, "MSBuildToolsVersion", "Current".to_string());
            set_default_property(&mut model, "MSBuildSDKsPath", display_path(sdk_root));
            if let Some(version) = extensions_path.file_name() {
                set_default_property(
                    &mut model,
                    "NETCoreSdkVersion",
                    version.to_string_lossy().into_owned(),
                );
            }
        }

        set_reserved_project_properties(&mut model, project_path);

        let mut global_properties = PropertyMap::new();
        for (name, value) in context.global_properties().iter() {
            if is_reserved_property(name) {
                bail!("Invalid global property '{name}': the property is reserved");
            }
            model.set_property(name.clone(), value.clone());
            global_properties.insert(name.clone(), String::new());
        }

        Ok(Self {
            model,
            global_properties,
            import_stack: HashSet::new(),
            render_preprocessed,
            sdk_root: context.sdk_root().map(Path::to_path_buf),
            _diagnostics: DiagnosticMode::None,
        })
    }

    fn evaluate_file(&mut self, path: &Path, include_project_element: bool) -> Result<String> {
        let canonical_path = path
            .canonicalize()
            .with_context(|| format!("Failed to resolve project {}", path.display()))?;
        if !self.import_stack.insert(canonical_path.clone()) {
            bail!("Circular import detected at {}", canonical_path.display());
        }

        let raw_source = fs::read_to_string(&canonical_path)
            .with_context(|| format!("Failed to read project {}", canonical_path.display()))?;
        let source = if self.render_preprocessed {
            Cow::Owned(normalize_source(&raw_source))
        } else {
            Cow::Borrowed(raw_source.trim_start_matches('\u{feff}'))
        };
        let (content_start, content_end) =
            project_content_bounds(&source, include_project_element)?;
        let sdk = project_sdk(&source)?;

        let (sdk_props, sdk_targets, sdk_name) = if let Some(sdk) = sdk {
            let sdk_directory = self.resolve_sdk(&sdk)?;
            (
                Some(sdk_directory.join("Sdk.props")),
                Some(sdk_directory.join("Sdk.targets")),
                Some(sdk),
            )
        } else {
            (None, None, None)
        };

        let rendered_sdk_props = if let Some(props_path) = &sdk_props {
            Some(self.evaluate_file(props_path, false)?)
        } else {
            None
        };

        let mut reader = Reader::from_str(&source);
        reader.config_mut().trim_text(false);
        let mut output = String::new();
        let mut cursor = content_start;
        let mut property_group = None;
        let mut item_group = None;
        let mut import_group = None;
        let mut import_group_indentation: Option<String> = None;
        let mut current_property: Option<PendingProperty> = None;
        let mut current_item: Option<PendingItem> = None;
        let mut current_metadata: Option<(String, String)> = None;
        let mut current_target: Option<Target> = None;
        let mut current_task: Option<Task> = None;
        let mut nonempty_import: Option<(usize, ImportAttributes, bool)> = None;

        loop {
            let event_start = reader.buffer_position() as usize;
            let event = reader.read_event()?;
            let event_end = reader.buffer_position() as usize;

            match event {
                Event::Start(element) if element.name().as_ref() == b"Project" => {}
                Event::Start(element)
                    if element.name().as_ref() == b"PropertyGroup" && current_target.is_none() =>
                {
                    property_group = Some(self.evaluate_optional_condition(
                        &element,
                        &reader,
                        &canonical_path,
                    )?);
                }
                Event::Start(element)
                    if element.name().as_ref() == b"ItemGroup" && current_target.is_none() =>
                {
                    item_group = Some(self.evaluate_optional_condition(
                        &element,
                        &reader,
                        &canonical_path,
                    )?);
                }
                Event::Start(element)
                    if element.name().as_ref() == b"ImportGroup" && current_target.is_none() =>
                {
                    import_group = Some(self.evaluate_optional_condition(
                        &element,
                        &reader,
                        &canonical_path,
                    )?);
                    if self.render_preprocessed {
                        output.push_str(&source[cursor..event_start]);
                        output.push_str("<!--");
                        output.push_str(&source[event_start..event_end]);
                        output.push_str("-->");
                        cursor = event_end;
                    }
                    import_group_indentation =
                        Some(line_indentation(&source, event_start).to_string());
                }
                Event::Start(element)
                    if element.name().as_ref() == b"Import" && current_target.is_none() =>
                {
                    let import = parse_import(&element, &reader)?;
                    self.model.add_import(Import {
                        project: import.project.clone(),
                        condition: import.condition.clone(),
                    });
                    nonempty_import = Some((event_start, import, import_group.unwrap_or(true)));
                }
                Event::Start(element)
                    if element.name().as_ref() == b"Target" && current_target.is_none() =>
                {
                    let attributes = parse_attributes(&element, &reader)?;
                    let target_name = attributes
                        .get("Name")
                        .ok_or_else(|| anyhow!("Target missing Name attribute"))?
                        .clone();
                    current_target = Some(Target {
                        name: target_name,
                        depends_on: attributes
                            .get("DependsOnTargets")
                            .map(|value| {
                                value
                                    .split(';')
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        condition: attributes.get("Condition").cloned(),
                        tasks: Vec::new(),
                        source_file: canonical_path.clone(),
                    });
                }
                Event::Start(element)
                    if element.name().as_ref() == b"UsingTask" && current_target.is_none() =>
                {
                    let attributes = parse_attributes(&element, &reader)?;
                    if let (Some(task_name), Some(assembly)) =
                        (attributes.get("TaskName"), attributes.get("AssemblyName"))
                    {
                        self.model
                            .add_using_task(task_name.clone(), assembly.clone());
                    }
                }
                Event::Start(element) if current_target.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    current_task = Some(Task {
                        name: xml_name(&element),
                        condition: attributes.get("Condition").cloned(),
                        attributes,
                    });
                }
                Event::Start(element) if property_group.is_some() => {
                    let group_eligible = property_group.unwrap_or(false);
                    let eligible = group_eligible
                        && self.evaluate_optional_condition(&element, &reader, &canonical_path)?;
                    current_property = Some(PendingProperty {
                        name: xml_name(&element),
                        value: String::new(),
                        eligible,
                    });
                }
                Event::Start(element)
                    if item_group.is_some()
                        && current_item.is_some()
                        && current_metadata.is_none() =>
                {
                    current_metadata = Some((xml_name(&element), String::new()));
                }
                Event::Start(element) if item_group.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    current_item = Some(PendingItem {
                        item_type: xml_name(&element),
                        include: attributes.get("Include").cloned(),
                        condition: attributes.get("Condition").cloned(),
                        metadata: HashMap::new(),
                    });
                }
                Event::Empty(element)
                    if element.name().as_ref() == b"Import"
                        && current_target.is_none()
                        && event_start >= content_start
                        && event_end <= content_end =>
                {
                    let import = parse_import(&element, &reader)?;
                    self.model.add_import(Import {
                        project: import.project.clone(),
                        condition: import.condition.clone(),
                    });
                    self.process_import(
                        &source,
                        event_start,
                        event_end,
                        import,
                        import_group.unwrap_or(true),
                        import_group_indentation.as_deref(),
                        &canonical_path,
                        &mut cursor,
                        &mut output,
                    )?;
                }
                Event::Empty(element) if current_target.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    if let Some(target) = &mut current_target {
                        target.tasks.push(Task {
                            name: xml_name(&element),
                            condition: attributes.get("Condition").cloned(),
                            attributes,
                        });
                    }
                }
                Event::Empty(element) if property_group.is_some() => {
                    let eligible = property_group.unwrap_or(false)
                        && self.evaluate_optional_condition(&element, &reader, &canonical_path)?;
                    if eligible {
                        self.assign_property(xml_name(&element), "", &canonical_path)?;
                    }
                }
                Event::Empty(element) if item_group.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    self.add_item(
                        PendingItem {
                            item_type: xml_name(&element),
                            include: attributes.get("Include").cloned(),
                            condition: attributes.get("Condition").cloned(),
                            metadata: HashMap::new(),
                        },
                        item_group.unwrap_or(false),
                        &canonical_path,
                    )?;
                }
                Event::Text(text) if current_metadata.is_some() => {
                    current_metadata
                        .as_mut()
                        .unwrap()
                        .1
                        .push_str(&text.decode()?);
                }
                Event::Text(text) if current_property.is_some() => {
                    current_property
                        .as_mut()
                        .unwrap()
                        .value
                        .push_str(&text.decode()?);
                }
                Event::GeneralRef(reference) if current_metadata.is_some() => {
                    let encoded = format!("&{};", reference.decode()?);
                    current_metadata
                        .as_mut()
                        .unwrap()
                        .1
                        .push_str(&quick_xml::escape::unescape(&encoded)?);
                }
                Event::GeneralRef(reference) if current_property.is_some() => {
                    let encoded = format!("&{};", reference.decode()?);
                    current_property
                        .as_mut()
                        .unwrap()
                        .value
                        .push_str(&quick_xml::escape::unescape(&encoded)?);
                }
                Event::CData(data) if current_metadata.is_some() => {
                    current_metadata
                        .as_mut()
                        .unwrap()
                        .1
                        .push_str(&data.decode()?);
                }
                Event::CData(data) if current_property.is_some() => {
                    current_property
                        .as_mut()
                        .unwrap()
                        .value
                        .push_str(&data.decode()?);
                }
                Event::End(element)
                    if element.name().as_ref() == b"Import" && nonempty_import.is_some() =>
                {
                    let (start, import, enabled) = nonempty_import.take().unwrap();
                    self.process_import(
                        &source,
                        start,
                        event_end,
                        import,
                        enabled,
                        import_group_indentation.as_deref(),
                        &canonical_path,
                        &mut cursor,
                        &mut output,
                    )?;
                }
                Event::End(element) if element.name().as_ref() == b"PropertyGroup" => {
                    property_group = None;
                }
                Event::End(element) if element.name().as_ref() == b"ItemGroup" => {
                    item_group = None;
                }
                Event::End(element) if element.name().as_ref() == b"ImportGroup" => {
                    if self.render_preprocessed {
                        output.push_str(&source[cursor..event_start]);
                        output.push_str("<!--");
                        output.push_str(&source[event_start..event_end]);
                        output.push_str("-->");
                        cursor = event_end;
                    }
                    import_group = None;
                    import_group_indentation = None;
                }
                Event::End(element) if element.name().as_ref() == b"Target" => {
                    if let Some(target) = current_target.take() {
                        self.model.add_target(target);
                    }
                }
                Event::End(element)
                    if current_task
                        .as_ref()
                        .is_some_and(|task| task.name.as_bytes() == element.name().as_ref()) =>
                {
                    if let Some(task) = current_task.take()
                        && let Some(target) = &mut current_target
                    {
                        target.tasks.push(task);
                    }
                }
                Event::End(element)
                    if current_property.as_ref().is_some_and(|property| {
                        property.name.as_bytes() == element.name().as_ref()
                    }) =>
                {
                    let property = current_property.take().unwrap();
                    if property.eligible {
                        self.assign_property(
                            property.name,
                            property.value.trim(),
                            &canonical_path,
                        )?;
                    }
                }
                Event::End(element)
                    if current_metadata
                        .as_ref()
                        .is_some_and(|(name, _)| name.as_bytes() == element.name().as_ref()) =>
                {
                    let (name, value) = current_metadata.take().unwrap();
                    if let Some(item) = &mut current_item {
                        item.metadata.insert(name, value.trim().to_string());
                    }
                }
                Event::End(element)
                    if current_item.as_ref().is_some_and(|item| {
                        item.item_type.as_bytes() == element.name().as_ref()
                    }) =>
                {
                    let item = current_item.take().unwrap();
                    self.add_item(item, item_group.unwrap_or(false), &canonical_path)?;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        if self.render_preprocessed {
            output.push_str(&source[cursor..content_end]);
        }

        let rendered_sdk_targets = if let Some(targets_path) = &sdk_targets {
            Some(self.evaluate_file(targets_path, false)?)
        } else {
            None
        };

        self.import_stack.remove(&canonical_path);

        if let (Some(sdk), Some(props_path), Some(targets_path)) =
            (sdk_name, sdk_props, sdk_targets)
            && self.render_preprocessed
        {
            let props = rendered_sdk_props.unwrap_or_default();
            let targets = rendered_sdk_targets.unwrap_or_default();
            let props_marker = implicit_sdk_marker("Sdk.props", &sdk, &props_path, &props, true);
            let targets_marker =
                implicit_sdk_marker("Sdk.targets", &sdk, &targets_path, &targets, false);
            if include_project_element {
                let opening_end = output
                    .find('>')
                    .ok_or_else(|| anyhow!("Project root opening tag was not found"))?
                    + 1;
                let closing_start = output
                    .rfind("</Project>")
                    .ok_or_else(|| anyhow!("Project root closing tag was not found"))?;
                return Ok(format!(
                    "{}\n{}{}\n{}{}",
                    &output[..opening_end],
                    props_marker,
                    &output[opening_end..closing_start],
                    targets_marker,
                    &output[closing_start..]
                ));
            }
            return Ok(format!(
                "{props_marker}{output}\n{targets_marker}<!-- SDK project: {} -->",
                display_path(&canonical_path)
            ));
        }

        Ok(output)
    }

    fn evaluate_optional_condition(
        &self,
        element: &BytesStart<'_>,
        reader: &Reader<&[u8]>,
        current_file: &Path,
    ) -> Result<bool> {
        let condition = attribute_value(element, reader, b"Condition")?;
        match condition {
            Some(condition) => ExpressionEvaluator::with_current_file(&self.model, current_file)
                .evaluate_condition(&condition)
                .with_context(|| {
                    format!(
                        "Failed to evaluate condition '{condition}' in {}",
                        current_file.display()
                    )
                }),
            None => Ok(true),
        }
    }

    fn assign_property(
        &mut self,
        name: String,
        raw_value: &str,
        current_file: &Path,
    ) -> Result<()> {
        if is_reserved_property(&name) {
            bail!(
                "The '{name}' property is reserved and cannot be modified in {}",
                current_file.display()
            );
        }
        if self.global_properties.contains_key(&name) {
            return Ok(());
        }
        let value = ExpressionEvaluator::with_current_file(&self.model, current_file)
            .evaluate(raw_value)
            .with_context(|| {
                format!(
                    "Failed to evaluate property '{name}' in {}",
                    current_file.display()
                )
            })?;
        self.model.set_property(name, value);
        Ok(())
    }

    fn add_item(
        &mut self,
        item: PendingItem,
        group_eligible: bool,
        current_file: &Path,
    ) -> Result<()> {
        if !group_eligible {
            return Ok(());
        }
        let evaluator = ExpressionEvaluator::with_current_file(&self.model, current_file);
        if let Some(condition) = &item.condition
            && !evaluator.evaluate_condition(condition)?
        {
            return Ok(());
        }
        let Some(include) = item.include else {
            return Ok(());
        };
        let include = evaluator.evaluate(&include)?;
        let metadata = item
            .metadata
            .into_iter()
            .map(|(name, value)| Ok((name, evaluator.evaluate(&value)?)))
            .collect::<Result<HashMap<_, _>>>()?;
        for identity in include.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.model.add_item(Item {
                item_type: item.item_type.clone(),
                name: identity.to_string(),
                metadata: metadata.clone(),
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_import(
        &mut self,
        source: &str,
        event_start: usize,
        event_end: usize,
        import: ImportAttributes,
        group_enabled: bool,
        group_indentation: Option<&str>,
        importing_path: &Path,
        cursor: &mut usize,
        output: &mut String,
    ) -> Result<()> {
        if self.render_preprocessed {
            let preceding = &source[*cursor..event_start];
            if let Some(indentation) = group_indentation {
                let line_start = preceding
                    .rfind(['\r', '\n'])
                    .map_or(0, |position| position + 1);
                output.push_str(&preceding[..line_start]);
                output.push_str(indentation);
            } else {
                output.push_str(preceding);
            }
        }
        let source_element = &source[event_start..event_end];

        if !group_enabled {
            if self.render_preprocessed {
                output.push_str(source_element);
            }
            *cursor = event_end;
            return Ok(());
        }

        let evaluator = ExpressionEvaluator::with_current_file(&self.model, importing_path);
        if let Some(condition) = import.condition
            && !evaluator.evaluate_condition(&condition).with_context(|| {
                format!(
                    "Failed to evaluate import condition '{condition}' in {}",
                    importing_path.display()
                )
            })?
        {
            if self.render_preprocessed {
                output.push_str(source_element);
            }
            *cursor = event_end;
            return Ok(());
        }

        let evaluated_project = evaluator.evaluate(&import.project)?;
        if evaluated_project.contains("$(") || evaluated_project.contains("@(") {
            bail!(
                "Import path '{evaluated_project}' contains an unexpanded expression in {}",
                importing_path.display()
            );
        }
        let import_path = importing_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(evaluated_project);
        let paths = resolve_import_paths(&import_path, importing_path)?;
        debug!(
            "Import '{}' from {} matched {} file(s)",
            import.project,
            importing_path.display(),
            paths.len()
        );

        for path in paths {
            let imported = self.evaluate_file(&path, false)?;
            if self.render_preprocessed {
                let indentation =
                    group_indentation.unwrap_or_else(|| line_indentation(source, event_start));
                let declaration = source_element
                    .trim_end()
                    .strip_suffix("/>")
                    .map(|value| format!("{}{}>", indentation, value.trim_end()))
                    .unwrap_or_else(|| source_element.to_string());
                output.push_str(&format!(
                    "<!--\n{BOUNDARY}\n{declaration}\n\n{}\n{BOUNDARY}\n-->\n",
                    display_path(&path)
                ));
                output.push_str(imported.trim_matches(['\r', '\n']));
                output.push_str(&format!(
                    "\n{indentation}<!--\n{BOUNDARY}\n{indentation}</Import>\n\n{}\n{BOUNDARY}\n-->",
                    display_path(importing_path)
                ));
            }
        }
        *cursor = event_end;
        Ok(())
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
}

fn set_default_property(model: &mut ProjectModel, name: &str, value: String) {
    if !model.properties.contains_key(name) {
        model.set_property(name.to_string(), value);
    }
}

fn parse_import(element: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<ImportAttributes> {
    Ok(ImportAttributes {
        project: attribute_value(element, reader, b"Project")?
            .ok_or_else(|| anyhow!("Import is missing its Project attribute"))?,
        condition: attribute_value(element, reader, b"Condition")?,
    })
}

fn parse_attributes(
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
) -> Result<HashMap<String, String>> {
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute?;
        attributes.insert(
            String::from_utf8_lossy(attribute.key.as_ref()).into_owned(),
            attribute
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())?
                .into_owned(),
        );
    }
    Ok(attributes)
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

fn xml_name(element: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(element.name().as_ref()).into_owned()
}

fn line_indentation(source: &str, position: usize) -> &str {
    let line_start = source[..position]
        .rfind(['\r', '\n'])
        .map_or(0, |position| position + 1);
    &source[line_start..position]
}

fn resolve_import_paths(import_path: &Path, importing_path: &Path) -> Result<Vec<PathBuf>> {
    let pattern = import_path.to_string_lossy();
    if pattern.contains(['*', '?', '[']) {
        let pattern = pattern.replace('\\', "/");
        let mut paths = glob::glob(&pattern)?
            .filter_map(Result::ok)
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
                return Ok((
                    start,
                    if include_project_element {
                        event_end
                    } else {
                        event_start
                    },
                ));
            }
            Event::Eof => bail!("Project root element was not found"),
            _ => {}
        }
    }
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
