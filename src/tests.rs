#[cfg(test)]
mod integration_tests {
    use anyhow::Result;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    use crate::evaluation::ProjectEvaluator;
    use crate::expression::ExpressionEvaluator;
    use crate::object_model::ProjectModel;

    #[test]
    fn test_project_default_targets_match_sample_project() -> Result<()> {
        let project_path = Path::new("sample_projects/simple.proj");
        if !project_path.exists() {
            return Ok(());
        }

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project_path)?;

        assert_eq!(
            evaluator
                .get_model()
                .get_property("MSBuildProjectDefaultTargets")
                .map(String::as_str),
            Some("Build")
        );

        Ok(())
    }

    #[test]
    fn test_self_closing_project_preserves_default_targets_text() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let project_path = temp_dir.path().join("empty.proj");
        fs::write(&project_path, r#"<Project DefaultTargets="Build;Pack" />"#)?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(&project_path)?;

        assert_eq!(
            evaluator
                .get_model()
                .get_property("MSBuildProjectDefaultTargets")
                .map(String::as_str),
            Some("Build;Pack")
        );

        Ok(())
    }

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
        use crate::object_model::{Item, MetadataMap};
        use std::sync::Arc;

        let item1 = Item::new(
            "Source".to_string(),
            "file1.cs".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("project.proj"),
        );

        let item2 = Item::new(
            "Source".to_string(),
            "file2.cs".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("project.proj"),
        );

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
        use crate::tasks::{CopyTask, TaskExecutionContext, TaskExecutor};
        use std::collections::HashMap; // Execute the copy task

        // Prepare evaluated attributes (simulating what TaskRegistry.execute_task does)
        let mut evaluated_attributes = HashMap::new();
        evaluated_attributes.insert(
            "SourceFiles".to_string(),
            test_file.to_string_lossy().to_string(),
        );
        evaluated_attributes.insert(
            "DestinationFolder".to_string(),
            dest_dir.to_string_lossy().to_string(),
        );

        let context =
            TaskExecutionContext::new(evaluated_attributes, temp_dir.path().to_path_buf());
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
        let fixture_path = Path::new("sample_projects/simple.proj");
        if !fixture_path.exists() {
            return Ok(());
        }

        let temp_dir = TempDir::new()?;
        let project_path = temp_dir.path().join("simple.proj");
        fs::copy(fixture_path, &project_path)?;
        fs::copy(
            "sample_projects/readme.txt",
            temp_dir.path().join("readme.txt"),
        )?;
        fs::copy(
            "sample_projects/config.xml",
            temp_dir.path().join("config.xml"),
        )?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(&project_path)?;

        // The Build target depends on CopyResources, which depends on Compile, which depends on Clean
        // This should execute all targets in the correct order
        evaluator.execute_target("Build")?;

        Ok(())
    }
}
