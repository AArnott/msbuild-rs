use anyhow::{anyhow, Result};
use log::{info, error};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::object_model::{ProjectModel, Task};

pub trait TaskExecutor {
    fn execute(&self, attributes: &HashMap<String, String>) -> Result<()>;
}

pub struct MessageTask;

impl TaskExecutor for MessageTask {
    fn execute(&self, attributes: &HashMap<String, String>) -> Result<()> {
        if let Some(text) = attributes.get("Text") {
            info!("{}", text);
        }
        Ok(())
    }
}

pub struct ErrorTask;

impl TaskExecutor for ErrorTask {
    fn execute(&self, attributes: &HashMap<String, String>) -> Result<()> {
        if let Some(text) = attributes.get("Text") {
            error!("{}", text);
            return Err(anyhow!("Build failed: {}", text));
        }

        Err(anyhow!("Build failed"))
    }
}

pub struct CopyTask;

impl TaskExecutor for CopyTask {
    fn execute(&self, attributes: &HashMap<String, String>) -> Result<()> {
        let source_files = attributes.get("SourceFiles")
            .ok_or_else(|| anyhow!("Copy task missing SourceFiles attribute"))?;

        let destination_folder = attributes.get("DestinationFolder")
            .ok_or_else(|| anyhow!("Copy task missing DestinationFolder attribute"))?;

        // Create destination directory if it doesn't exist
        fs::create_dir_all(destination_folder)?;

        // Copy each file
        for source_file in source_files.split(';') {
            if source_file.trim().is_empty() {
                continue;
            }

            let source_path = Path::new(source_file.trim());
            if source_path.exists() {
                let file_name = source_path.file_name()
                    .ok_or_else(|| anyhow!("Invalid source file path: {}", source_file))?;

                let dest_path = Path::new(destination_folder).join(file_name);

                fs::copy(source_path, &dest_path)?;
                info!("Copied {} to {}", source_file, dest_path.display());
            } else {
                error!("Source file does not exist: {}", source_file);
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

            executor.execute(&evaluated_attributes)
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
    use std::collections::HashMap;    #[test]
    fn test_message_task() -> Result<()> {
        let mut attributes = HashMap::new();
        attributes.insert("Text".to_string(), "Building Debug".to_string());

        let message_task = MessageTask;
        message_task.execute(&attributes)?;

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
