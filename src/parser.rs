use anyhow::{Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::expression::ExpressionEvaluator;
use crate::object_model::{Import, Item, ProjectModel, Target, Task};

pub struct ProjectParser {
    model: ProjectModel,
}

impl ProjectParser {
    pub fn new() -> Self {
        Self {
            model: ProjectModel::new(),
        }
    }

    pub fn parse_file<P: AsRef<Path>>(&mut self, path: P) -> Result<ProjectModel> {
        let file = File::open(&path)?;
        let buf_reader = BufReader::new(file);
        let mut reader = Reader::from_reader(buf_reader);
        reader.config_mut().trim_text(true);

        let mut buf = Vec::new();
        let mut current_target: Option<Target> = None;
        let mut current_task: Option<Task> = None;
        let mut in_property_group = false;
        let mut in_item_group = false;
        let mut current_property_name: Option<String> = None;
        let mut current_item_type: Option<String> = None;
        let mut current_item_include: Option<String> = None;
        let mut current_item_metadata: HashMap<String, String> = HashMap::new();

        // First pass: collect all properties and static elements
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name_bytes = e.name();
                    let name = std::str::from_utf8(name_bytes.as_ref())?;
                    let attributes = self.parse_attributes(e)?;

                    match name {
                        "Project" => {
                            // Root element, continue parsing
                        }
                        "PropertyGroup" => {
                            if self.should_process_conditional(&attributes)? {
                                in_property_group = true;
                            }
                        }
                        "ItemGroup" => {
                            if self.should_process_conditional(&attributes)? {
                                in_item_group = true;
                            }
                        }
                        "Target" => {
                            // Always load targets regardless of their conditions
                            // Conditions will be evaluated during execution phase
                            let target_name = attributes
                                .get("Name")
                                .ok_or_else(|| anyhow!("Target missing Name attribute"))?
                                .clone();

                            let depends_on = attributes
                                .get("DependsOnTargets")
                                .map(|deps| deps.split(';').map(|s| s.trim().to_string()).collect())
                                .unwrap_or_default();

                            current_target = Some(Target {
                                name: target_name,
                                depends_on,
                                condition: attributes.get("Condition").cloned(),
                                tasks: Vec::new(),
                            });
                        }
                        "Import" => {
                            if self.should_process_conditional(&attributes)? {
                                if let Some(project) = attributes.get("Project") {
                                    self.model.add_import(Import {
                                        project: project.clone(),
                                        condition: attributes.get("Condition").cloned(),
                                    });
                                }
                            }
                        }
                        "UsingTask" => {
                            if let (Some(task_name), Some(assembly)) =
                                (attributes.get("TaskName"), attributes.get("AssemblyName"))
                            {
                                self.model
                                    .add_using_task(task_name.clone(), assembly.clone());
                            }
                        }
                        task_name if current_target.is_some() => {
                            // This is a task within a target
                            current_task = Some(Task {
                                name: task_name.to_string(),
                                attributes: attributes.clone(),
                                condition: attributes.get("Condition").cloned(),
                            });
                        }
                        property_name if in_property_group => {
                            // Only set property name if there's no condition or condition is true
                            if self.should_process_conditional(&attributes)? {
                                current_property_name = Some(property_name.to_string());
                            }
                        }
                        item_type if in_item_group => {
                            current_item_type = Some(item_type.to_string());
                            current_item_metadata.clear();
                            current_item_include = attributes.get("Include").cloned();
                        }
                        _ => {
                            // Unknown element, skip
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    // Handle self-closing tags like <Compile Include="file.cs" />
                    let name_bytes = e.name();
                    let name = std::str::from_utf8(name_bytes.as_ref())?;
                    let attributes = self.parse_attributes(e)?;

                    if in_item_group {
                        // This is an item definition
                        if let Some(include) = attributes.get("Include") {
                            self.process_item(name.to_string(), include.clone(), HashMap::new())?;
                        }
                    } else if current_target.is_some() {
                        // This is a task within a target
                        let task = Task {
                            name: name.to_string(),
                            attributes: attributes.clone(),
                            condition: attributes.get("Condition").cloned(),
                        };

                        if let Some(ref mut target) = current_target {
                            target.tasks.push(task);
                        }
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name_bytes = e.name();
                    let name = std::str::from_utf8(name_bytes.as_ref())?;

                    match name {
                        "PropertyGroup" => {
                            in_property_group = false;
                        }
                        "ItemGroup" => {
                            in_item_group = false;
                        }
                        "Target" => {
                            if let Some(target) = current_target.take() {
                                self.model.add_target(target);
                            }
                        }
                        _task_name if current_task.is_some() => {
                            if let Some(task) = current_task.take() {
                                if let Some(ref mut target) = current_target {
                                    target.tasks.push(task);
                                }
                            }
                        }
                        property_name
                            if in_property_group
                                && current_property_name.as_ref()
                                    == Some(&property_name.to_string()) =>
                        {
                            current_property_name = None;
                        }
                        item_type
                            if in_item_group
                                && current_item_type.as_ref() == Some(&item_type.to_string()) =>
                        {
                            // Process item after we have all properties
                            if let (Some(item_type), Some(include)) =
                                (&current_item_type, &current_item_include)
                            {
                                self.process_item(
                                    item_type.clone(),
                                    include.clone(),
                                    current_item_metadata.clone(),
                                )?;
                            }
                            current_item_type = None;
                            current_item_include = None;
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(e)) => {
                    let text = e.decode()?.trim().to_string();
                    if !text.is_empty() {
                        if let Some(ref prop_name) = current_property_name {
                            // Store the raw property value, don't evaluate yet
                            self.model.set_property(prop_name.clone(), text);
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("Error parsing XML: {}", e)),
                _ => {}
            }
            buf.clear();
        }

        Ok(std::mem::take(&mut self.model))
    }

    fn process_item(
        &mut self,
        item_type: String,
        include: String,
        metadata: HashMap<String, String>,
    ) -> Result<()> {
        let evaluator = ExpressionEvaluator::new(&self.model);
        let evaluated_include = evaluator.evaluate(&include)?;

        // Handle multiple items separated by semicolons
        for item_name in evaluated_include.split(';') {
            if !item_name.trim().is_empty() {
                let item = Item {
                    item_type: item_type.clone(),
                    name: item_name.trim().to_string(),
                    metadata: metadata.clone(),
                };
                self.model.add_item(item);
            }
        }
        Ok(())
    }

    fn parse_attributes(&self, element: &BytesStart) -> Result<HashMap<String, String>> {
        let mut attributes = HashMap::new();

        for attr in element.attributes() {
            let attr = attr?;
            let key = std::str::from_utf8(attr.key.as_ref())?.to_string();
            let value = std::str::from_utf8(&attr.value)?.to_string();
            attributes.insert(key, value);
        }

        Ok(attributes)
    }

    fn should_process_conditional(&self, attributes: &HashMap<String, String>) -> Result<bool> {
        if let Some(condition) = attributes.get("Condition") {
            let evaluator = ExpressionEvaluator::new(&self.model);
            evaluator.evaluate_condition(condition)
        } else {
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_simple_project() -> Result<()> {
        let xml_content = r#"<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build">
  <PropertyGroup>
    <Configuration>Debug</Configuration>
    <Platform>x64</Platform>
  </PropertyGroup>

  <ItemGroup>
    <Compile Include="Program.cs" />
    <Compile Include="Utils.cs" />
  </ItemGroup>

  <Target Name="Build">
    <Message Text="Building $(Configuration) $(Platform)" />
  </Target>
</Project>"#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(xml_content.as_bytes())?;

        let mut parser = ProjectParser::new();
        let model = parser.parse_file(temp_file.path())?;

        assert_eq!(
            model.get_property("Configuration"),
            Some(&"Debug".to_string())
        );
        assert_eq!(model.get_property("Platform"), Some(&"x64".to_string()));

        assert!(model.get_items("Compile").is_some());
        let compile_items = model.get_items("Compile").unwrap();
        assert_eq!(compile_items.len(), 2);
        assert_eq!(compile_items[0].name, "Program.cs");
        assert_eq!(compile_items[1].name, "Utils.cs");

        assert!(model.get_target("Build").is_some());

        Ok(())
    }
}
