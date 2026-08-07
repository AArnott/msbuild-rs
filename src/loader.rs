use anyhow::{Context, Result, anyhow, bail};
use log::{debug, warn};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::escaping::{ItemSpecKind, classify_item_spec, tokenize_list, unescape_once};
use crate::evaluation::{ActiveToolset, EvaluationContext};
use crate::expression::ExpressionEvaluator;
use crate::object_model::{Import, Item, ProjectModel, PropertyMap, Target, Task};
use crate::properties::{
    display_path, is_reserved_property, lexical_absolute, set_reserved_project_properties,
};

const BOUNDARY: &str = "============================================================================================================================================";

pub(crate) struct LoadOutput {
    pub model: ProjectModel,
    pub preprocessed: Option<String>,
}

struct EvaluationState {
    model: ProjectModel,
    global_properties: PropertyMap,
    active_import_stack: Vec<ActiveImport>,
    completed_imports: HashSet<PathBuf>,
    render_preprocessed: bool,
    sdk_root: Option<PathBuf>,
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
    metadata: Vec<PendingMetadata>,
}

#[derive(Debug)]
struct PendingItemDefinition {
    item_type: String,
    condition: Option<String>,
    metadata: Vec<PendingMetadata>,
}

#[derive(Debug)]
struct PendingMetadata {
    name: String,
    value: String,
    condition: Option<String>,
}

#[derive(Debug)]
struct ImportAttributes {
    project: String,
    condition: Option<String>,
}

#[derive(Debug)]
struct ActiveImport {
    identity: PathBuf,
    lexical_path: PathBuf,
}

#[derive(Debug)]
struct ChooseFrame {
    parent_active: bool,
    branch_active: bool,
    branch_selected: bool,
}

#[derive(Debug)]
struct StructuralFrame {
    name: String,
    choose: Option<ChooseStructure>,
}

#[derive(Debug, Default)]
struct ChooseStructure {
    when_count: usize,
    otherwise_seen: bool,
}

#[derive(Debug, Clone)]
struct SdkReference {
    name: String,
    version: Option<String>,
}

impl SdkReference {
    fn specification(&self) -> String {
        self.version
            .as_ref()
            .map(|version| format!("{}/{version}", self.name))
            .unwrap_or_else(|| self.name.clone())
    }
}

pub(crate) fn load_project(
    context: &EvaluationContext,
    path: &Path,
    render_preprocessed: bool,
) -> Result<LoadOutput> {
    let project_path = lexical_absolute(path)
        .with_context(|| format!("Failed to resolve project {}", path.display()))?;
    if !project_path.is_file() {
        bail!("Project '{}' was not found", project_path.display());
    }
    let toolset = context.resolve_toolset(&project_path);
    let sdk_root = context
        .sdk_root_override()
        .map(Path::to_path_buf)
        .or_else(|| toolset.as_ref().map(|toolset| toolset.sdk_root.clone()));
    let mut state = EvaluationState::new(
        context,
        &project_path,
        render_preprocessed,
        sdk_root,
        toolset,
    )?;
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
        sdk_root: Option<PathBuf>,
        toolset: Option<ActiveToolset>,
    ) -> Result<Self> {
        let mut model = ProjectModel::new();
        model.set_project_file_path(project_path.to_path_buf());

        for (name, value) in context.environment().iter_escaped() {
            if !is_reserved_property(name) {
                model.set_property(name.clone(), value.to_string());
            }
        }

        initialize_reserved_toolset_properties(&mut model, context, toolset.as_ref());

        if let Some(toolset) = &toolset {
            set_default_property(
                &mut model,
                "MSBuildExtensionsPath",
                with_trailing_separator(display_path(&toolset.tools_path)),
            );
            set_default_property(
                &mut model,
                "MSBuildExtensionsPath32",
                display_path(&toolset.tools_path),
            );
            set_default_property(
                &mut model,
                "MSBuildExtensionsPath64",
                display_path(&toolset.tools_path),
            );
            set_default_property(
                &mut model,
                "MSBuildSemanticVersion",
                toolset.msbuild_semantic_version.clone(),
            );
        }
        if let Some(sdk_root) = &sdk_root {
            set_default_property(&mut model, "MSBuildSDKsPath", display_path(sdk_root));
        }

        set_reserved_project_properties(&mut model, project_path);

        let mut global_properties = PropertyMap::new();
        for (name, value) in context.global_properties().iter_escaped() {
            if is_reserved_property(name) {
                bail!("MSB4177: Invalid property. The \"{name}\" property name is reserved.");
            }
            model.set_property(name.clone(), value.to_string());
            global_properties.insert(name.clone(), String::new());
        }

        Ok(Self {
            model,
            global_properties,
            active_import_stack: Vec::new(),
            completed_imports: HashSet::new(),
            render_preprocessed,
            sdk_root,
        })
    }

    fn evaluate_file(&mut self, path: &Path, include_project_element: bool) -> Result<String> {
        let lexical_path = lexical_absolute(path)
            .with_context(|| format!("Failed to resolve project {}", path.display()))?;
        let import_identity = normalized_lexical_file_identity(&lexical_path)?;
        if let Some(cycle_start) = self
            .active_import_stack
            .iter()
            .position(|import| import.identity == import_identity)
        {
            let mut chain = self.active_import_stack[cycle_start..]
                .iter()
                .map(|import| display_path(&import.lexical_path))
                .collect::<Vec<_>>();
            chain.push(display_path(&lexical_path));
            warn!(
                "MSB4210: \"{}\" is attempting to import itself, directly or indirectly. The import will be ignored. Import chain: {}",
                lexical_path.display(),
                chain.join(" -> ")
            );
            return Ok(String::new());
        }
        if self.completed_imports.contains(&import_identity) {
            debug!("Skipping duplicate import {}", lexical_path.display());
            return Ok(String::new());
        }
        self.active_import_stack.push(ActiveImport {
            identity: import_identity.clone(),
            lexical_path: lexical_path.clone(),
        });

        let raw_source = fs::read_to_string(&lexical_path)
            .with_context(|| format!("Failed to read project {}", lexical_path.display()))?;
        let source = if self.render_preprocessed {
            normalize_source(&raw_source)
        } else {
            raw_source.trim_start_matches('\u{feff}').to_string()
        };
        validate_project_structure(&source, &lexical_path)?;
        let (content_start, content_end) =
            project_content_bounds(&source, include_project_element)?;
        let sdk_references = project_sdks(&source)?;
        let mut sdk_imports = Vec::with_capacity(sdk_references.len());
        for sdk in sdk_references {
            let specification = sdk.specification();
            let sdk_directory = self.resolve_sdk(&specification)?;
            sdk_imports.push((
                specification,
                sdk_directory.join("Sdk.props"),
                sdk_directory.join("Sdk.targets"),
            ));
        }

        let mut rendered_sdk_props = Vec::with_capacity(sdk_imports.len());
        for (_, props_path, _) in &sdk_imports {
            rendered_sdk_props.push(self.evaluate_file(props_path, false)?);
        }

        let mut reader = Reader::from_str(&source);
        reader.config_mut().trim_text(false);
        let mut output = String::new();
        let mut cursor = content_start;
        let mut property_group = None;
        let mut item_group = None;
        let mut item_definition_group = None;
        let mut import_group = None;
        let mut import_group_indentation: Option<String> = None;
        let mut current_property: Option<PendingProperty> = None;
        let mut current_item: Option<PendingItem> = None;
        let mut current_item_definition: Option<PendingItemDefinition> = None;
        let mut current_metadata: Option<PendingMetadata> = None;
        let mut current_target: Option<Target> = None;
        let mut current_task: Option<Task> = None;
        let mut nonempty_import: Option<(usize, ImportAttributes, bool)> = None;
        let mut choices = Vec::<ChooseFrame>::new();

        loop {
            let event_start = reader.buffer_position() as usize;
            let event = reader.read_event()?;
            let event_end = reader.buffer_position() as usize;

            match event {
                Event::Start(element)
                    if include_project_element && element.name().as_ref() == b"Project" =>
                {
                    self.evaluate_project_default_targets(&element, &reader, &lexical_path)?;
                }
                Event::Empty(element)
                    if include_project_element && element.name().as_ref() == b"Project" =>
                {
                    self.evaluate_project_default_targets(&element, &reader, &lexical_path)?;
                }
                Event::Empty(element)
                    if element.name().as_ref() == b"When" && current_target.is_none() =>
                {
                    let (parent_active, branch_selected) = choices
                        .last()
                        .map(|choice| (choice.parent_active, choice.branch_selected))
                        .ok_or_else(|| anyhow!("When element has no enclosing Choose"))?;
                    let is_selected = parent_active
                        && !branch_selected
                        && self.evaluate_optional_condition(&element, &reader, &lexical_path)?;
                    let choice = choices.last_mut().expect("choice must still be present");
                    choice.branch_active = is_selected;
                    choice.branch_selected |= is_selected;
                }
                Event::Empty(element)
                    if element.name().as_ref() == b"Otherwise" && current_target.is_none() =>
                {
                    let choice = choices
                        .last_mut()
                        .ok_or_else(|| anyhow!("Otherwise element has no enclosing Choose"))?;
                    choice.branch_active = choice.parent_active && !choice.branch_selected;
                    choice.branch_selected |= choice.branch_active;
                }
                Event::Start(element)
                    if element.name().as_ref() == b"Choose" && current_target.is_none() =>
                {
                    choices.push(ChooseFrame {
                        parent_active: Self::choose_content_active(&choices),
                        branch_active: false,
                        branch_selected: false,
                    });
                }
                Event::Start(element)
                    if element.name().as_ref() == b"When" && current_target.is_none() =>
                {
                    let (parent_active, branch_selected) = choices
                        .last()
                        .map(|choice| (choice.parent_active, choice.branch_selected))
                        .ok_or_else(|| anyhow!("When element has no enclosing Choose"))?;
                    let is_selected = parent_active
                        && !branch_selected
                        && self.evaluate_optional_condition(&element, &reader, &lexical_path)?;
                    let choice = choices.last_mut().expect("choice must still be present");
                    choice.branch_active = is_selected;
                    choice.branch_selected |= is_selected;
                }
                Event::Start(element)
                    if element.name().as_ref() == b"Otherwise" && current_target.is_none() =>
                {
                    let choice = choices
                        .last_mut()
                        .ok_or_else(|| anyhow!("Otherwise element has no enclosing Choose"))?;
                    choice.branch_active = choice.parent_active && !choice.branch_selected;
                    choice.branch_selected |= choice.branch_active;
                }
                Event::Start(element)
                    if element.name().as_ref() == b"PropertyGroup" && current_target.is_none() =>
                {
                    property_group = Some(
                        Self::choose_content_active(&choices)
                            && self.evaluate_optional_condition(
                                &element,
                                &reader,
                                &lexical_path,
                            )?,
                    );
                }
                Event::Start(element)
                    if element.name().as_ref() == b"ItemGroup" && current_target.is_none() =>
                {
                    item_group = Some(
                        Self::choose_content_active(&choices)
                            && self.evaluate_optional_condition(
                                &element,
                                &reader,
                                &lexical_path,
                            )?,
                    );
                }
                Event::Start(element)
                    if element.name().as_ref() == b"ItemDefinitionGroup"
                        && current_target.is_none() =>
                {
                    item_definition_group = Some(
                        Self::choose_content_active(&choices)
                            && self.evaluate_optional_condition(
                                &element,
                                &reader,
                                &lexical_path,
                            )?,
                    );
                }
                Event::Start(element)
                    if element.name().as_ref() == b"ImportGroup" && current_target.is_none() =>
                {
                    import_group = Some(
                        Self::choose_content_active(&choices)
                            && self.evaluate_optional_condition(
                                &element,
                                &reader,
                                &lexical_path,
                            )?,
                    );
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
                    nonempty_import = Some((
                        event_start,
                        import,
                        import_group.unwrap_or(true) && Self::choose_content_active(&choices),
                    ));
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
                                tokenize_list(value).map(|values| {
                                    values.into_iter().map(unescape_once).collect::<Vec<_>>()
                                })
                            })
                            .transpose()?
                            .unwrap_or_default(),
                        condition: attributes.get("Condition").cloned(),
                        tasks: Vec::new(),
                        source_file: lexical_path.clone(),
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
                        && self.evaluate_optional_condition(&element, &reader, &lexical_path)?;
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
                    current_metadata = Some(PendingMetadata {
                        name: xml_name(&element),
                        value: String::new(),
                        condition: attribute_value(&element, &reader, b"Condition")?,
                    });
                }
                Event::Start(element) if item_group.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    current_item = Some(PendingItem {
                        item_type: xml_name(&element),
                        include: attributes.get("Include").cloned(),
                        condition: attributes.get("Condition").cloned(),
                        metadata: Vec::new(),
                    });
                }
                Event::Start(element)
                    if item_definition_group.is_some()
                        && current_item_definition.is_some()
                        && current_metadata.is_none() =>
                {
                    current_metadata = Some(PendingMetadata {
                        name: xml_name(&element),
                        value: String::new(),
                        condition: attribute_value(&element, &reader, b"Condition")?,
                    });
                }
                Event::Start(element) if item_definition_group.is_some() => {
                    current_item_definition = Some(PendingItemDefinition {
                        item_type: xml_name(&element),
                        condition: attribute_value(&element, &reader, b"Condition")?,
                        metadata: Vec::new(),
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
                        import_group.unwrap_or(true) && Self::choose_content_active(&choices),
                        import_group_indentation.as_deref(),
                        &lexical_path,
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
                    let eligible = Self::choose_content_active(&choices)
                        && property_group.unwrap_or(false)
                        && self.evaluate_optional_condition(&element, &reader, &lexical_path)?;
                    if eligible {
                        self.assign_property(xml_name(&element), "", &lexical_path)?;
                    }
                }
                Event::Empty(element) if item_group.is_some() && current_item.is_some() => {
                    if let Some(item) = &mut current_item {
                        item.metadata.push(PendingMetadata {
                            name: xml_name(&element),
                            value: String::new(),
                            condition: attribute_value(&element, &reader, b"Condition")?,
                        });
                    }
                }
                Event::Empty(element) if item_group.is_some() => {
                    let attributes = parse_attributes(&element, &reader)?;
                    self.add_item(
                        PendingItem {
                            item_type: xml_name(&element),
                            include: attributes.get("Include").cloned(),
                            condition: attributes.get("Condition").cloned(),
                            metadata: Vec::new(),
                        },
                        item_group.unwrap_or(false),
                        &lexical_path,
                    )?;
                }
                Event::Empty(element)
                    if item_definition_group.is_some() && current_item_definition.is_some() =>
                {
                    if let Some(definition) = &mut current_item_definition {
                        definition.metadata.push(PendingMetadata {
                            name: xml_name(&element),
                            value: String::new(),
                            condition: attribute_value(&element, &reader, b"Condition")?,
                        });
                    }
                }
                Event::Empty(element) if item_definition_group.is_some() => {
                    self.add_item_definition(
                        PendingItemDefinition {
                            item_type: xml_name(&element),
                            condition: attribute_value(&element, &reader, b"Condition")?,
                            metadata: Vec::new(),
                        },
                        item_definition_group.unwrap_or(false),
                        &lexical_path,
                    )?;
                }
                Event::Text(text) if current_metadata.is_some() => {
                    current_metadata
                        .as_mut()
                        .unwrap()
                        .value
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
                        .value
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
                        .value
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
                        &lexical_path,
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
                Event::End(element) if element.name().as_ref() == b"ItemDefinitionGroup" => {
                    item_definition_group = None;
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
                Event::End(element)
                    if (element.name().as_ref() == b"When"
                        || element.name().as_ref() == b"Otherwise")
                        && current_target.is_none() =>
                {
                    let choice = choices
                        .last_mut()
                        .ok_or_else(|| anyhow!("Choice branch end has no enclosing Choose"))?;
                    choice.branch_active = false;
                }
                Event::End(element)
                    if element.name().as_ref() == b"Choose" && current_target.is_none() =>
                {
                    choices
                        .pop()
                        .ok_or_else(|| anyhow!("Choose end has no opening Choose"))?;
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
                        self.assign_property(property.name, &property.value, &lexical_path)?;
                    }
                }
                Event::End(element)
                    if current_metadata.as_ref().is_some_and(|metadata| {
                        metadata.name.as_bytes() == element.name().as_ref()
                    }) =>
                {
                    let metadata = current_metadata.take().unwrap();
                    if let Some(item) = &mut current_item {
                        item.metadata.push(metadata);
                    } else if let Some(definition) = &mut current_item_definition {
                        definition.metadata.push(metadata);
                    }
                }
                Event::End(element)
                    if current_item.as_ref().is_some_and(|item| {
                        item.item_type.as_bytes() == element.name().as_ref()
                    }) =>
                {
                    let item = current_item.take().unwrap();
                    self.add_item(item, item_group.unwrap_or(false), &lexical_path)?;
                }
                Event::End(element)
                    if current_item_definition.as_ref().is_some_and(|definition| {
                        definition.item_type.as_bytes() == element.name().as_ref()
                    }) =>
                {
                    let definition = current_item_definition.take().unwrap();
                    self.add_item_definition(
                        definition,
                        item_definition_group.unwrap_or(false),
                        &lexical_path,
                    )?;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        if self.render_preprocessed {
            output.push_str(&source[cursor..content_end]);
        }

        let mut rendered_sdk_targets = Vec::with_capacity(sdk_imports.len());
        for (_, _, targets_path) in &sdk_imports {
            rendered_sdk_targets.push(self.evaluate_file(targets_path, false)?);
        }

        let completed = self
            .active_import_stack
            .pop()
            .expect("active import stack underflow");
        debug_assert_eq!(completed.identity, import_identity);
        self.completed_imports.insert(import_identity);

        if !sdk_imports.is_empty() && self.render_preprocessed {
            let mut props_markers = String::new();
            let mut targets_markers = String::new();
            for (index, (sdk, props_path, targets_path)) in sdk_imports.iter().enumerate() {
                props_markers.push_str(&implicit_sdk_marker(
                    "Sdk.props",
                    sdk,
                    props_path,
                    &rendered_sdk_props[index],
                    true,
                ));
                targets_markers.push_str(&implicit_sdk_marker(
                    "Sdk.targets",
                    sdk,
                    targets_path,
                    &rendered_sdk_targets[index],
                    false,
                ));
            }
            if include_project_element {
                return insert_sdk_markers_into_project(&output, &props_markers, &targets_markers);
            }
            return Ok(format!(
                "{props_markers}{output}\n{targets_markers}<!-- SDK project: {} -->",
                display_path(&lexical_path)
            ));
        }

        Ok(output)
    }

    fn choose_content_active(choices: &[ChooseFrame]) -> bool {
        choices
            .iter()
            .all(|choice| choice.parent_active && choice.branch_active)
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

    fn evaluate_project_default_targets(
        &mut self,
        element: &BytesStart<'_>,
        reader: &Reader<&[u8]>,
        current_file: &Path,
    ) -> Result<()> {
        let Some(default_targets) = attribute_value(element, reader, b"DefaultTargets")? else {
            return Ok(());
        };
        let evaluated = ExpressionEvaluator::with_current_file(&self.model, current_file)
            .evaluate(&default_targets)
            .with_context(|| {
                format!(
                    "Failed to evaluate DefaultTargets in {}",
                    current_file.display()
                )
            })?;
        self.model
            .set_property("MSBuildProjectDefaultTargets".to_string(), evaluated);
        Ok(())
    }

    fn assign_property(
        &mut self,
        name: String,
        raw_value: &str,
        current_file: &Path,
    ) -> Result<()> {
        if is_reserved_property(&name) {
            bail!(
                "MSB4004: The \"{name}\" property is reserved, and cannot be modified in {}.",
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
        let Some(include) = item.include else {
            return Ok(());
        };

        let defaults = self.model.item_defaults(&item.item_type);
        let mut candidates = Vec::new();
        for fragment in tokenize_list(&include)? {
            if let Some(source_type) = simple_item_reference(fragment) {
                let source_items = self
                    .model
                    .get_items(source_type)
                    .cloned()
                    .unwrap_or_default();
                candidates.extend(source_items.into_iter().map(|source| {
                    source.copy_for_type(
                        item.item_type.clone(),
                        defaults.clone(),
                        current_file.to_path_buf(),
                    )
                }));
                continue;
            }

            let evaluated = ExpressionEvaluator::with_current_file(&self.model, current_file)
                .evaluate(fragment)?;
            for identity in tokenize_list(&evaluated)? {
                candidates.push(Item::new(
                    item.item_type.clone(),
                    identity.to_string(),
                    defaults.clone(),
                    current_file.to_path_buf(),
                ));
            }
        }

        for mut candidate in candidates {
            if let Some(condition) = &item.condition
                && !ExpressionEvaluator::with_item(&self.model, current_file, &candidate)
                    .evaluate_condition(condition)?
            {
                continue;
            }
            for metadata in &item.metadata {
                if let Some(condition) = &metadata.condition
                    && !ExpressionEvaluator::with_item(&self.model, current_file, &candidate)
                        .evaluate_condition(condition)?
                {
                    continue;
                }
                let value = ExpressionEvaluator::with_item(&self.model, current_file, &candidate)
                    .evaluate(&metadata.value)?;
                candidate.set_metadata(metadata.name.clone(), value);
            }
            self.model.add_item(candidate);
        }
        Ok(())
    }

    fn add_item_definition(
        &mut self,
        definition: PendingItemDefinition,
        group_eligible: bool,
        current_file: &Path,
    ) -> Result<()> {
        if !group_eligible {
            return Ok(());
        }
        if let Some(condition) = &definition.condition
            && !ExpressionEvaluator::with_item_definition(
                &self.model,
                current_file,
                &definition.item_type,
            )
            .evaluate_condition(condition)?
        {
            return Ok(());
        }
        for metadata in definition.metadata {
            if let Some(condition) = &metadata.condition
                && !ExpressionEvaluator::with_item_definition(
                    &self.model,
                    current_file,
                    &definition.item_type,
                )
                .evaluate_condition(condition)?
            {
                continue;
            }
            let value = ExpressionEvaluator::with_item_definition(
                &self.model,
                current_file,
                &definition.item_type,
            )
            .evaluate(&metadata.value)?;
            self.model.set_item_definition_metadata(
                definition.item_type.clone(),
                metadata.name,
                value,
            );
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
                output.push_str("<!--");
                output.push_str(source_element);
                output.push_str("-->");
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
                output.push_str("<!--");
                output.push_str(source_element);
                output.push_str("-->");
            }
            *cursor = event_end;
            return Ok(());
        }

        let evaluated_project_escaped = evaluator.evaluate(&import.project)?;
        if evaluated_project_escaped.contains("$(") || evaluated_project_escaped.contains("@(") {
            bail!(
                "Import path '{evaluated_project_escaped}' contains an unexpanded expression in {}",
                importing_path.display()
            );
        }
        let import_kind = classify_item_spec(&evaluated_project_escaped);
        let evaluated_project = unescape_once(&evaluated_project_escaped);
        let import_path = lexical_absolute(
            &importing_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(evaluated_project),
        )?;
        let paths = resolve_import_paths(
            &import_path,
            importing_path,
            import_kind == ItemSpecKind::Glob,
        )?;
        debug!(
            "Import '{}' from {} matched {} file(s)",
            import.project,
            importing_path.display(),
            paths.len()
        );
        let all_completed = paths
            .iter()
            .map(|path| normalized_lexical_file_identity(path))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .all(|identity| self.completed_imports.contains(identity));
        if all_completed {
            if self.render_preprocessed {
                output.push_str("<!--");
                output.push_str(source_element);
                output.push_str("-->");
            }
            *cursor = event_end;
            return Ok(());
        }

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

fn initialize_reserved_toolset_properties(
    model: &mut ProjectModel,
    context: &EvaluationContext,
    toolset: Option<&ActiveToolset>,
) {
    let tools_path = toolset
        .map(|toolset| display_path(&toolset.tools_path))
        .unwrap_or_default();
    let msbuild_version = toolset
        .map(|toolset| toolset.msbuild_version.clone())
        .unwrap_or_default();
    let assembly_version = msbuild_version
        .split('.')
        .next()
        .filter(|major| !major.is_empty())
        .map(|major| format!("{major}.0"))
        .unwrap_or_default();
    let startup_directory = std::env::current_dir()
        .ok()
        .and_then(|path| lexical_absolute(&path).ok())
        .map(|path| display_path(&path))
        .unwrap_or_default();
    let program_files_32 = context
        .environment()
        .get("ProgramFiles(x86)")
        .cloned()
        .unwrap_or_default();

    for (name, value) in [
        ("MSBuildBinPath", tools_path.clone()),
        ("MSBuildProjectDefaultTargets", String::new()),
        ("MSBuildToolsPath", tools_path),
        ("MSBuildToolsVersion", "Current".to_string()),
        (
            "MSBuildRuntimeType",
            toolset.map(|_| "Core".to_string()).unwrap_or_default(),
        ),
        ("MSBuildStartupDirectory", startup_directory),
        ("MSBuildNodeCount", "1".to_string()),
        ("MSBuildLastTaskResult", String::new()),
        ("MSBuildProgramFiles32", program_files_32),
        ("MSBuildAssemblyVersion", assembly_version),
        ("MSBuildVersion", msbuild_version),
        ("MSBuildInteractive", String::new()),
        ("MSBuildDisableFeaturesFromVersion", "999.999".to_string()),
    ] {
        model.set_property(name.to_string(), value);
    }
}

fn with_trailing_separator(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with(['/', '\\']) {
        value.push(std::path::MAIN_SEPARATOR);
    }
    value
}

fn simple_item_reference(expression: &str) -> Option<&str> {
    let body = expression.strip_prefix("@(")?.strip_suffix(')')?.trim();
    (!body.is_empty()
        && !body.contains("->")
        && !body.contains(',')
        && !body.contains(['(', ')', '\'', '"']))
    .then_some(body)
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

fn validate_project_structure(source: &str, path: &Path) -> Result<()> {
    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(false);
    let mut stack = Vec::<StructuralFrame>::new();
    let mut root_seen = false;

    loop {
        match reader.read_event()? {
            Event::DocType(_) => {
                bail!(
                    "DTD declarations and external entities are disabled in {}",
                    path.display()
                );
            }
            Event::Start(element) => {
                let frame =
                    validate_structural_element(&element, &reader, path, &mut stack, root_seen)?;
                root_seen = true;
                stack.push(frame);
            }
            Event::Empty(element) => {
                let frame =
                    validate_structural_element(&element, &reader, path, &mut stack, root_seen)?;
                root_seen = true;
                finish_structural_element(frame, path)?;
            }
            Event::End(element) => {
                let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
                let frame = stack.pop().ok_or_else(|| {
                    anyhow!(
                        "Unexpected closing element <{name}> while validating {}",
                        path.display()
                    )
                })?;
                if frame.name != name {
                    bail!(
                        "Closing element <{name}> does not match <{}> in {}",
                        frame.name,
                        path.display()
                    );
                }
                finish_structural_element(frame, path)?;
            }
            Event::Text(text)
                if stack
                    .last()
                    .is_some_and(|frame| is_choice_structure_element(&frame.name))
                    && !text.decode()?.trim().is_empty() =>
            {
                bail!(
                    "Text content is not allowed directly beneath <{}> in {}",
                    stack.last().expect("checked above").name,
                    path.display()
                );
            }
            Event::CData(data)
                if stack
                    .last()
                    .is_some_and(|frame| is_choice_structure_element(&frame.name))
                    && !data.decode()?.trim().is_empty() =>
            {
                bail!(
                    "Text content is not allowed directly beneath <{}> in {}",
                    stack.last().expect("checked above").name,
                    path.display()
                );
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !stack.is_empty() {
        bail!(
            "Project XML ended before all elements were closed in {}",
            path.display()
        );
    }
    if !root_seen {
        bail!("Project root element was not found in {}", path.display());
    }
    Ok(())
}

fn validate_structural_element(
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    path: &Path,
    stack: &mut [StructuralFrame],
    root_seen: bool,
) -> Result<StructuralFrame> {
    let name = xml_name(element);
    let mut choose = None;
    if stack.is_empty() {
        if root_seen || name != "Project" {
            bail!(
                "MSB4067: The root element <{name}> is invalid in {}. Expected <Project>.",
                path.display()
            );
        }
    } else {
        let parent = stack.last_mut().expect("checked above");
        if parent.name == "Project" {
            if !matches!(
                name.as_str(),
                "PropertyGroup"
                    | "ItemGroup"
                    | "ItemDefinitionGroup"
                    | "Import"
                    | "ImportGroup"
                    | "Choose"
                    | "Target"
                    | "UsingTask"
                    | "ProjectExtensions"
                    | "Sdk"
            ) {
                return illegal_child(&name, &parent.name, path);
            }
            if name == "Choose" {
                choose = Some(ChooseStructure::default());
            }
        } else if let Some(choice) = parent.choose.as_mut() {
            match name.as_str() {
                "When" => {
                    if choice.otherwise_seen {
                        bail!(
                            "MSB4084: A <When> element may not follow an <Otherwise> element in a <Choose> in {}.",
                            path.display()
                        );
                    }
                    let condition = attribute_value(element, reader, b"Condition")?;
                    if condition.is_none_or(|condition| condition.trim().is_empty()) {
                        bail!(
                            "MSB4035: The required attribute \"Condition\" is empty or missing from the element <When> in {}.",
                            path.display()
                        );
                    }
                    choice.when_count += 1;
                }
                "Otherwise" => {
                    if choice.otherwise_seen {
                        bail!(
                            "MSB4082: Choose has more than one <Otherwise> element in {}.",
                            path.display()
                        );
                    }
                    if attribute_value(element, reader, b"Condition")?.is_some() {
                        bail!(
                            "MSB4066: The attribute \"Condition\" in element <Otherwise> is unrecognized in {}.",
                            path.display()
                        );
                    }
                    choice.otherwise_seen = true;
                }
                _ => return illegal_child(&name, &parent.name, path),
            }
        } else if is_choice_structure_element(&parent.name) {
            if !matches!(
                name.as_str(),
                "PropertyGroup" | "ItemGroup" | "ItemDefinitionGroup" | "Choose"
            ) {
                return illegal_child(&name, &parent.name, path);
            }
            if name == "Choose" {
                choose = Some(ChooseStructure::default());
            }
        }
    }
    Ok(StructuralFrame { name, choose })
}

fn finish_structural_element(frame: StructuralFrame, path: &Path) -> Result<()> {
    if let Some(choice) = frame.choose
        && choice.when_count == 0
    {
        bail!(
            "MSB4085: A <Choose> must contain at least one <When> in {}.",
            path.display()
        );
    }
    Ok(())
}

fn is_choice_structure_element(name: &str) -> bool {
    matches!(name, "When" | "Otherwise")
}

fn illegal_child<T>(name: &str, parent: &str, path: &Path) -> Result<T> {
    bail!(
        "MSB4067: The element <{name}> beneath element <{parent}> is unrecognized in {}.",
        path.display()
    )
}

fn resolve_import_paths(
    import_path: &Path,
    importing_path: &Path,
    expand_glob: bool,
) -> Result<Vec<PathBuf>> {
    let pattern = import_path.to_string_lossy();
    if expand_glob {
        let pattern = pattern.replace('\\', "/");
        let mut paths = glob::glob(&pattern)?
            .filter_map(Result::ok)
            .filter(|path| path.is_file())
            .filter_map(|path| lexical_absolute(&path).ok())
            .collect::<Vec<_>>();
        paths.sort();
        let mut identities = HashSet::new();
        paths.retain(|path| {
            normalized_lexical_file_identity(path)
                .map(|identity| identities.insert(identity))
                .unwrap_or(false)
        });
        return Ok(paths);
    }

    if !import_path.is_file() {
        bail!(
            "Imported project '{}' was not found from {}",
            import_path.display(),
            importing_path.display()
        );
    }
    Ok(vec![lexical_absolute(import_path)?])
}

fn normalized_lexical_file_identity(path: &Path) -> Result<PathBuf> {
    let path = lexical_absolute(path)
        .with_context(|| format!("Failed to resolve project {}", path.display()))?;
    #[cfg(windows)]
    {
        Ok(PathBuf::from(path.to_string_lossy().to_lowercase()))
    }
    #[cfg(not(windows))]
    {
        Ok(path)
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
            Event::Empty(element) if element.name().as_ref() == b"Project" => {
                return Ok(if include_project_element {
                    (event_start, event_end)
                } else {
                    (event_end, event_end)
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

fn project_sdks(source: &str) -> Result<Vec<SdkReference>> {
    let mut reader = Reader::from_str(source);
    let mut references = Vec::new();
    let mut project_depth = 0usize;
    loop {
        match reader.read_event()? {
            Event::Start(element) if element.name().as_ref() == b"Project" => {
                append_project_sdk_attribute(&mut references, &element, &reader)?;
                project_depth = 1;
            }
            Event::Empty(element) if element.name().as_ref() == b"Project" => {
                append_project_sdk_attribute(&mut references, &element, &reader)?;
                return Ok(references);
            }
            Event::Empty(element) if project_depth == 1 && element.name().as_ref() == b"Sdk" => {
                references.push(parse_sdk_element(&element, &reader)?);
            }
            Event::Start(element) if project_depth == 1 && element.name().as_ref() == b"Sdk" => {
                references.push(parse_sdk_element(&element, &reader)?);
                project_depth += 1;
            }
            Event::Start(_) if project_depth > 0 => project_depth += 1,
            Event::End(element) if element.name().as_ref() == b"Project" => {
                return Ok(references);
            }
            Event::End(_) if project_depth > 0 => {
                project_depth -= 1;
            }
            Event::Eof => return Ok(references),
            _ => {}
        }
    }
}

fn append_project_sdk_attribute(
    references: &mut Vec<SdkReference>,
    element: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
) -> Result<()> {
    if let Some(sdks) = attribute_value(element, reader, b"Sdk")? {
        for specification in tokenize_list(&sdks)? {
            let specification = unescape_once(specification);
            let (name, version) = specification
                .split_once('/')
                .map(|(name, version)| (name, Some(version.to_string())))
                .unwrap_or((specification.as_str(), None));
            references.push(SdkReference {
                name: name.to_string(),
                version,
            });
        }
    }
    Ok(())
}

fn parse_sdk_element(element: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<SdkReference> {
    let name = attribute_value(element, reader, b"Name")?
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("Sdk element is missing its Name attribute"))?;
    let version =
        attribute_value(element, reader, b"Version")?.filter(|version| !version.trim().is_empty());
    Ok(SdkReference { name, version })
}

fn insert_sdk_markers_into_project(
    output: &str,
    props_markers: &str,
    targets_markers: &str,
) -> Result<String> {
    let opening_end = output
        .find('>')
        .ok_or_else(|| anyhow!("Project root opening tag was not found"))?
        + 1;
    if let Some(closing_start) = output.rfind("</Project>") {
        return Ok(format!(
            "{}\n{}{}\n{}{}",
            &output[..opening_end],
            props_markers,
            &output[opening_end..closing_start],
            targets_markers,
            &output[closing_start..]
        ));
    }

    let opening = output[..opening_end].trim_end();
    let opening = opening
        .strip_suffix("/>")
        .ok_or_else(|| anyhow!("Project root closing tag was not found"))?;
    Ok(format!(
        "{opening}>\n{props_markers}\n{targets_markers}</Project>"
    ))
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
        "  <!--\n{BOUNDARY}\n  <Import Project=\"{file_name}\" Sdk=\"{sdk}\">\n  This import was added implicitly because the project declared SDK \"{sdk}\".\n\n{}\n{BOUNDARY}\n-->\n{}\n  <!--\n{BOUNDARY}\n  </Import>\n\n{}\n{BOUNDARY}\n-->{suffix}",
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
