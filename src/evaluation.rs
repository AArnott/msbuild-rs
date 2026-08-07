use anyhow::{Result, anyhow};
use log::{debug, info, warn};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::expression::ExpressionEvaluator;
use crate::object_model::ProjectModel;
use crate::parser::ProjectParser;
use crate::preprocess::ProjectPreprocessor;
use crate::tasks::TaskRegistry;

pub struct ProjectEvaluator {
    model: ProjectModel,
    task_registry: TaskRegistry,
}

/// A deterministic, target-free projection of an evaluated project.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct EvaluationQueryResult {
    pub properties: BTreeMap<String, String>,
    pub items: BTreeMap<String, Vec<EvaluationQueryItem>>,
}

/// An item selected by an evaluation query.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct EvaluationQueryItem {
    pub identity: String,
    pub metadata: BTreeMap<String, String>,
}

impl ProjectEvaluator {
    pub fn new() -> Self {
        Self {
            model: ProjectModel::new(),
            task_registry: TaskRegistry::new(),
        }
    }

    pub fn load_project<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        info!("Loading project: {}", path.as_ref().display());

        let mut parser = ProjectParser::new();
        self.model = parser.parse_file(&path)?;

        // Set the project file path for task execution context
        self.model
            .set_project_file_path(path.as_ref().to_path_buf());

        debug!("Loaded {} properties", self.model.properties.len());
        debug!("Loaded {} item types", self.model.items.len());
        debug!("Loaded {} targets", self.model.targets.len());

        // Process imports (simplified - would need to handle relative paths properly in real implementation)
        for import in &self.model.imports.clone() {
            let project_directory = self.model.get_project_directory();
            let evaluator = ExpressionEvaluator::with_base_directory(
                &self.model,
                project_directory
                    .as_deref()
                    .unwrap_or_else(|| Path::new("")),
            );
            if let Some(condition) = &import.condition
                && !evaluator.evaluate_condition(condition)?
            {
                continue;
            }

            let import_path = evaluator.evaluate(&import.project)?;
            info!("Processing import: {import_path}");

            // In a real implementation, this would resolve relative paths and handle SDK imports
            let import_path = Path::new(&import_path);
            let import_path = if import_path.is_absolute() {
                import_path.to_path_buf()
            } else {
                project_directory
                    .as_deref()
                    .unwrap_or_else(|| Path::new(""))
                    .join(import_path)
            };
            if import_path.exists() {
                let mut import_parser = ProjectParser::new();
                let import_model = import_parser.parse_file(import_path)?;

                // Merge the imported model into the current model
                self.merge_model(import_model)?;
            } else {
                warn!("Import file not found: {}", import_path.display());
            }
        }

        Ok(())
    }

    pub fn execute_target(&mut self, target_name: &str) -> Result<()> {
        info!("Executing target: {target_name}");

        let mut executed_targets = HashSet::new();
        self.execute_target_recursive(target_name, &mut executed_targets)
    }

    pub fn write_preprocessed_project<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        ProjectPreprocessor::new(&self.model).write(path)
    }

    /// Select evaluated properties and items without executing targets.
    pub fn query_evaluation(
        &self,
        property_names: &[String],
        item_types: &[String],
    ) -> Result<EvaluationQueryResult> {
        let base_directory = self.model.get_project_directory().unwrap_or_default();
        let expression_evaluator =
            ExpressionEvaluator::with_base_directory(&self.model, &base_directory);
        let mut properties = BTreeMap::new();
        let mut items = BTreeMap::new();

        for name in property_names {
            let value = self
                .model
                .get_property(name)
                .map(|value| expression_evaluator.evaluate(value))
                .transpose()?
                .unwrap_or_default();
            properties.insert(name.clone(), value);
        }

        for item_type in item_types {
            let queried_items = self
                .model
                .get_items(item_type)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| EvaluationQueryItem {
                            identity: item.name.clone(),
                            metadata: item
                                .metadata
                                .iter()
                                .map(|(name, value)| (name.clone(), value.clone()))
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            items.insert(item_type.clone(), queried_items);
        }

        Ok(EvaluationQueryResult { properties, items })
    }

    fn execute_target_recursive(
        &self,
        target_name: &str,
        executed_targets: &mut HashSet<String>,
    ) -> Result<()> {
        if executed_targets.contains(target_name) {
            debug!("Target {target_name} already executed, skipping");
            return Ok(());
        }

        let target = self
            .model
            .get_target(target_name)
            .ok_or_else(|| anyhow!("Target not found: {target_name}"))?
            .clone();

        // Check target condition
        if let Some(condition) = &target.condition {
            let evaluator = ExpressionEvaluator::new(&self.model);
            if !evaluator.evaluate_condition(condition)? {
                info!("Skipping target {target_name} due to condition: {condition}");
                return Ok(());
            }
        }

        // Execute dependencies first
        for dependency in &target.depends_on {
            self.execute_target_recursive(dependency, executed_targets)?;
        }

        info!("Executing target: {}", target.name);
        executed_targets.insert(target_name.to_string());

        // Execute tasks in the target
        for task in &target.tasks {
            debug!("Executing task: {}", task.name);
            self.task_registry.execute_task(task, &self.model)?;
        }

        Ok(())
    }

    fn merge_model(&mut self, other: ProjectModel) -> Result<()> {
        // Merge properties
        for (name, value) in other.properties {
            self.model.set_property(name, value);
        }

        // Merge items
        for (_item_type, items) in other.items {
            for item in items {
                self.model.add_item(item);
            }
        }

        // Merge targets
        for (_name, target) in other.targets {
            self.model.add_target(target);
        }

        // Merge imports
        for import in other.imports {
            self.model.add_import(import);
        }

        // Merge using tasks
        for (task_name, assembly) in other.using_tasks {
            self.model.add_using_task(task_name, assembly);
        }

        Ok(())
    }

    /// Get a reference to the loaded project model
    /// Useful for inspecting properties, items, and targets after loading
    #[allow(dead_code)] // Public API method for library users
    pub fn get_model(&self) -> &ProjectModel {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_project_evaluation() -> Result<()> {
        let xml_content = r#"<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build">
  <PropertyGroup>
    <Configuration>Debug</Configuration>
    <OutputPath>bin/$(Configuration)/</OutputPath>
  </PropertyGroup>

  <ItemGroup>
    <Compile Include="Program.cs" />
    <Compile Include="Utils.cs" />
  </ItemGroup>

  <Target Name="Build" DependsOnTargets="Compile">
    <Message Text="Build completed for $(Configuration)" />
  </Target>

  <Target Name="Compile">
    <Message Text="Compiling @(Compile) to $(OutputPath)" />
  </Target>
</Project>"#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(xml_content.as_bytes())?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(temp_file.path())?;
        evaluator.execute_target("Build")?;

        Ok(())
    }

    #[test]
    fn query_evaluation_does_not_execute_targets_and_expands_selected_values() -> Result<()> {
        let directory = tempfile::TempDir::new()?;
        let import_path = directory.path().join("values.props");
        std::fs::write(
            &import_path,
            r#"<Project><PropertyGroup><Imported>from-import</Imported></PropertyGroup></Project>"#,
        )?;
        let project_path = directory.path().join("project.proj");
        std::fs::write(
            &project_path,
            r#"<Project>
  <PropertyGroup><Base>base</Base><Derived>$(Base)-value</Derived></PropertyGroup>
  <Import Project="values.props" />
  <ItemGroup><Compile Include="Program.cs"><Kind>source</Kind></Compile></ItemGroup>
  <Target Name="Build">
    <ItemGroup><Compile Include="Generated.cs" /></ItemGroup>
    <Error Text="Targets must not run during a query" />
  </Target>
</Project>"#,
        )?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(&project_path)?;
        let result = evaluator.query_evaluation(
            &["Derived".to_string(), "Imported".to_string()],
            &["Compile".to_string()],
        )?;

        assert_eq!(result.properties["Derived"], "base-value");
        assert_eq!(result.properties["Imported"], "from-import");
        assert_eq!(result.items["Compile"][0].identity, "Program.cs");
        assert_eq!(result.items["Compile"][0].metadata["Kind"], "source");
        assert_eq!(result.items["Compile"].len(), 1);
        Ok(())
    }
}
