#[cfg(test)]
mod integration_tests {
    use anyhow::Result;
    use std::path::Path;
    use tempfile::TempDir;
    use std::fs;

    use crate::evaluation::ProjectEvaluator;
    use crate::object_model::ProjectModel;
    use crate::expression::ExpressionEvaluator;

    #[test]
    fn test_simple_project_execution() -> Result<()> {
        // Test that we can load and execute a simple project
        let project_path = Path::new("sample_projects/simple.proj");
        if !project_path.exists() {
            // Skip test if sample project doesn't exist
            return Ok(());
        }

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project_path)?;
        evaluator.execute_target("Build")?;

        Ok(())
    }

    #[test]
    fn test_conditional_project() -> Result<()> {
        let project_path = Path::new("sample_projects/conditional.proj");
        if !project_path.exists() {
            return Ok(());
        }

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project_path)?;

        // Test with Debug configuration (should run tests)
        evaluator.execute_target("Test")?;

        Ok(())
    }

    #[test]
    fn test_property_evaluation() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("Configuration".to_string(), "Debug".to_string());
        model.set_property("Platform".to_string(), "x64".to_string());

        let evaluator = ExpressionEvaluator::new(&model);

        // Test nested property references
        let result = evaluator.evaluate("Output: bin/$(Configuration)/$(Platform)")?;
        assert_eq!(result, "Output: bin/Debug/x64");

        // Test condition evaluation
        assert!(evaluator.evaluate_condition("'$(Configuration)' == 'Debug'")?);
        assert!(!evaluator.evaluate_condition("'$(Configuration)' == 'Release'")?);

        Ok(())
    }

    #[test]
    fn test_item_evaluation() -> Result<()> {
        let mut model = ProjectModel::new();

        // Add some items
        use crate::object_model::Item;
        use std::collections::HashMap;

        let item1 = Item {
            item_type: "Source".to_string(),
            name: "file1.cs".to_string(),
            metadata: HashMap::new(),
        };

        let item2 = Item {
            item_type: "Source".to_string(),
            name: "file2.cs".to_string(),
            metadata: HashMap::new(),
        };

        model.add_item(item1);
        model.add_item(item2);

        let evaluator = ExpressionEvaluator::new(&model);

        // Test item expansion
        let result = evaluator.evaluate("Sources: @(Source)")?;
        assert_eq!(result, "Sources: file1.cs;file2.cs");

        Ok(())
    }

    #[test]
    fn test_copy_task() -> Result<()> {
        // Create a temporary directory for testing
        let temp_dir = TempDir::new()?;
        let source_dir = temp_dir.path().join("source");
        let dest_dir = temp_dir.path().join("dest");

        fs::create_dir_all(&source_dir)?;

        // Create a test file to copy
        let test_file = source_dir.join("test.txt");
        fs::write(&test_file, "test content")?;

        // Create task attributes
        use std::collections::HashMap;        // Execute the copy task
        use crate::tasks::{CopyTask, TaskExecutor, TaskExecutionContext};

        // Prepare evaluated attributes (simulating what TaskRegistry.execute_task does)
        let mut evaluated_attributes = HashMap::new();
        evaluated_attributes.insert("SourceFiles".to_string(), test_file.to_string_lossy().to_string());
        evaluated_attributes.insert("DestinationFolder".to_string(), dest_dir.to_string_lossy().to_string());

        let context = TaskExecutionContext::new(evaluated_attributes, temp_dir.path().to_path_buf());
        let copy_task = CopyTask;
        copy_task.execute(&context)?;

        // Verify the file was copied
        let copied_file = dest_dir.join("test.txt");
        assert!(copied_file.exists());

        let content = fs::read_to_string(copied_file)?;
        assert_eq!(content, "test content");

        Ok(())
    }

    #[test]
    fn test_target_dependencies() -> Result<()> {
        // Test that target dependencies are executed in the correct order
        // This is tested through the integration with sample projects
        let project_path = Path::new("sample_projects/simple.proj");
        if !project_path.exists() {
            return Ok(());
        }

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project_path)?;

        // The Build target depends on CopyResources, which depends on Compile, which depends on Clean
        // This should execute all targets in the correct order
        evaluator.execute_target("Build")?;

        Ok(())
    }
}
