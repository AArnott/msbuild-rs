use anyhow::{anyhow, Result};
use log::{info, debug, warn};
use std::collections::HashSet;
use std::path::Path;

use crate::object_model::ProjectModel;
use crate::parser::ProjectParser;
use crate::tasks::TaskRegistry;
use crate::expression::ExpressionEvaluator;

pub struct ProjectEvaluator {
    model: ProjectModel,
    task_registry: TaskRegistry,
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
        self.model = parser.parse_file(path)?;

        debug!("Loaded {} properties", self.model.properties.len());
        debug!("Loaded {} item types", self.model.items.len());
        debug!("Loaded {} targets", self.model.targets.len());

        // Process imports (simplified - would need to handle relative paths properly in real implementation)
        for import in &self.model.imports.clone() {
            let evaluator = ExpressionEvaluator::new(&self.model);
            if let Some(condition) = &import.condition {
                if !evaluator.evaluate_condition(condition)? {
                    continue;
                }
            }

            let import_path = evaluator.evaluate(&import.project)?;
            info!("Processing import: {}", import_path);

            // In a real implementation, this would resolve relative paths and handle SDK imports
            if Path::new(&import_path).exists() {
                let mut import_parser = ProjectParser::new();
                let import_model = import_parser.parse_file(&import_path)?;

                // Merge the imported model into the current model
                self.merge_model(import_model)?;
            } else {
                warn!("Import file not found: {}", import_path);
            }
        }

        Ok(())
    }

    pub fn execute_target(&mut self, target_name: &str) -> Result<()> {
        info!("Executing target: {}", target_name);

        let mut executed_targets = HashSet::new();
        self.execute_target_recursive(target_name, &mut executed_targets)
    }

    fn execute_target_recursive(&self, target_name: &str, executed_targets: &mut HashSet<String>) -> Result<()> {
        if executed_targets.contains(target_name) {
            debug!("Target {} already executed, skipping", target_name);
            return Ok(());
        }

        let target = self.model.get_target(target_name)
            .ok_or_else(|| anyhow!("Target not found: {}", target_name))?
            .clone();

        // Check target condition
        if let Some(condition) = &target.condition {
            let evaluator = ExpressionEvaluator::new(&self.model);
            if !evaluator.evaluate_condition(condition)? {
                info!("Skipping target {} due to condition: {}", target_name, condition);
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
}
