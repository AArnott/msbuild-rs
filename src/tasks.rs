use anyhow::{Result, anyhow};
use log::{error, info};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::object_model::{ProjectModel, Task};

/// Context passed to task executors containing all necessary execution parameters
#[derive(Debug)]
pub struct TaskExecutionContext {
    /// Pre-evaluated attribute values from the task XML
    pub attributes: HashMap<String, String>,
    /// The directory containing the executing project file
    pub project_directory: PathBuf,
}

impl TaskExecutionContext {
    pub fn new(attributes: HashMap<String, String>, project_directory: PathBuf) -> Self {
        Self {
            attributes,
            project_directory,
        }
    }
}

pub trait TaskExecutor {
    fn execute(&self, context: &TaskExecutionContext) -> Result<()>;
}

pub struct MessageTask;

impl TaskExecutor for MessageTask {
    fn execute(&self, context: &TaskExecutionContext) -> Result<()> {
        if let Some(text) = context.attributes.get("Text") {
            info!("{}", text);
        }
        Ok(())
    }
}

pub struct ErrorTask;

impl TaskExecutor for ErrorTask {
    fn execute(&self, context: &TaskExecutionContext) -> Result<()> {
        if let Some(text) = context.attributes.get("Text") {
            error!("{}", text);
            return Err(anyhow!("Build failed: {}", text));
        }

        Err(anyhow!("Build failed"))
    }
}

pub struct CopyTask;

impl TaskExecutor for CopyTask {
    fn execute(&self, context: &TaskExecutionContext) -> Result<()> {
        let source_files = context
            .attributes
            .get("SourceFiles")
            .ok_or_else(|| anyhow!("Copy task missing SourceFiles attribute"))?;

        let destination_folder = context
            .attributes
            .get("DestinationFolder")
            .ok_or_else(|| anyhow!("Copy task missing DestinationFolder attribute"))?;

        // Resolve destination folder relative to project directory
        let dest_path = if Path::new(destination_folder).is_absolute() {
            PathBuf::from(destination_folder)
        } else {
            context.project_directory.join(destination_folder)
        };

        // Create destination directory if it doesn't exist
        fs::create_dir_all(&dest_path)?;

        // Copy each file
        for source_file in source_files.split(';') {
            if source_file.trim().is_empty() {
                continue;
            }

            // Resolve source file relative to project directory
            let source_path = if Path::new(source_file.trim()).is_absolute() {
                PathBuf::from(source_file.trim())
            } else {
                context.project_directory.join(source_file.trim())
            };

            if source_path.exists() {
                let file_name = source_path
                    .file_name()
                    .ok_or_else(|| anyhow!("Invalid source file path: {}", source_file))?;

                let final_dest_path = dest_path.join(file_name);

                fs::copy(&source_path, &final_dest_path)?;
                info!(
                    "Copied {} to {}",
                    source_path.display(),
                    final_dest_path.display()
                );
            } else {
                error!("Source file does not exist: {}", source_path.display());
            }
        }

        Ok(())
    }
}

pub struct TaskRegistry {
    tasks: HashMap<String, Box<dyn TaskExecutor>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tasks: HashMap::new(),
        };

        // Register built-in tasks
        registry.register("Message", Box::new(MessageTask));
        registry.register("Error", Box::new(ErrorTask));
        registry.register("Copy", Box::new(CopyTask));

        registry
    }

    pub fn register(&mut self, name: &str, executor: Box<dyn TaskExecutor>) {
        self.tasks.insert(name.to_string(), executor);
    }

    pub fn execute_task(&self, task: &Task, model: &ProjectModel) -> Result<()> {
        if let Some(executor) = self.tasks.get(&task.name) {
            // Check task condition first
            if let Some(condition) = &task.condition {
                use crate::expression::ExpressionEvaluator;
                let evaluator = ExpressionEvaluator::new(model);
                if !evaluator.evaluate_condition(condition)? {
                    return Ok(());
                }
            }

            // Evaluate all attribute values before passing to task
            use crate::expression::ExpressionEvaluator;
            let evaluator = ExpressionEvaluator::new(model);
            let mut evaluated_attributes = HashMap::new();

            for (key, value) in &task.attributes {
                let evaluated_value = evaluator.evaluate(value)?;
                evaluated_attributes.insert(key.clone(), evaluated_value);
            }

            let project_directory = model
                .get_project_directory()
                .unwrap_or_else(|| PathBuf::from("."));

            let context = TaskExecutionContext::new(evaluated_attributes, project_directory);
            executor.execute(&context)
        } else {
            error!("Unknown task: {}", task.name);
            Ok(()) // Don't fail on unknown tasks for now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_model::ProjectModel;
    use std::collections::HashMap;
    #[test]
    fn test_message_task() -> Result<()> {
        let mut attributes = HashMap::new();
        attributes.insert("Text".to_string(), "Building Debug".to_string());

        let context = TaskExecutionContext::new(attributes, PathBuf::from("."));
        let message_task = MessageTask;
        message_task.execute(&context)?;

        Ok(())
    }

    #[test]
    fn test_task_registry() -> Result<()> {
        let registry = TaskRegistry::new();
        let model = ProjectModel::new();

        let mut attributes = HashMap::new();
        attributes.insert("Text".to_string(), "Test message".to_string());

        let task = Task {
            name: "Message".to_string(),
            attributes,
            condition: None,
        };

        registry.execute_task(&task, &model)?;

        Ok(())
    }
}
