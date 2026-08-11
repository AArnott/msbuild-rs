mod escaping;
mod evaluation;
mod expression;
mod file_times;
mod item_glob;
mod loader;
mod logger;
mod native_functions;
mod object_model;
mod properties;
mod registry;
mod tasks;
mod tests;
mod workload;

use anyhow::Result;
use clap::Parser;
use log::info;
use std::io;
use std::path::PathBuf;

use crate::evaluation::ProjectEvaluator;
use crate::logger::setup_logging;

#[derive(Parser)]
#[command(name = "msbuild-rs")]
#[command(about = "A MSBuild project reader and executor written in Rust")]
struct Args {
    /// Path to the MSBuild project file
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// Target to execute (default: "Build")
    #[arg(short, long, default_value = "Build")]
    target: String,

    /// Write the evaluated project without executing targets
    #[arg(long, value_name = "PATH")]
    preprocess: Option<PathBuf>,

    /// Print selected evaluated property values as JSON without executing targets
    #[arg(long, value_name = "NAME", action = clap::ArgAction::Append)]
    get_property: Vec<String>,

    /// Print selected evaluated item identities and metadata as JSON without executing targets
    #[arg(long, value_name = "ITEM_TYPE", action = clap::ArgAction::Append)]
    get_item: Vec<String>,

    /// Set a global property using the MSBuild Name=Value form
    #[arg(
        long = "property",
        visible_alias = "global-property",
        value_name = "NAME=VALUE",
        value_parser = parse_global_property,
        action = clap::ArgAction::Append
    )]
    properties: Vec<(String, String)>,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Run demonstration with sample projects
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    setup_logging(args.verbose)?;

    if args.demo {
        info!("Running demonstration mode");
        return run_sample_projects(&args);
    }

    let project_path = args
        .project
        .ok_or_else(|| anyhow::anyhow!("Project path is required when not in demo mode"))?;

    info!("Starting MSBuild project execution");
    info!("Project: {}", project_path.display());
    info!("Target: {}", args.target);

    let mut evaluator = ProjectEvaluator::with_global_properties(args.properties);
    if let Some(output_path) = args.preprocess
        && args.get_property.is_empty()
        && args.get_item.is_empty()
    {
        evaluator.load_project_and_write_preprocessed(&project_path, output_path)?;
        return Ok(());
    }
    evaluator.load_project(&project_path)?;
    if !args.get_property.is_empty() || !args.get_item.is_empty() {
        let result = evaluator.query_evaluation(&args.get_property, &args.get_item)?;
        serde_json::to_writer_pretty(io::stdout(), &result)?;
        println!();
        return Ok(());
    }

    evaluator.execute_target(&args.target)?;

    info!("Build completed successfully");
    Ok(())
}

fn parse_global_property(value: &str) -> std::result::Result<(String, String), String> {
    let (name, value) = value
        .split_once('=')
        .ok_or_else(|| "global properties must use the Name=Value form".to_string())?;
    if name.is_empty() {
        return Err("global property names cannot be empty".to_string());
    }
    Ok((name.to_string(), value.to_string()))
}

fn run_sample_projects(_args: &Args) -> Result<()> {
    info!("=== MSBuild-RS Demonstration ===");

    // Test 1: Simple project
    info!("\n--- Testing Simple Project ---");
    let simple_project = PathBuf::from("sample_projects/simple.proj");
    if simple_project.exists() {
        let mut evaluator = ProjectEvaluator::new();
        match evaluator.load_project(&simple_project) {
            Ok(()) => match evaluator.execute_target("Build") {
                Ok(()) => info!("✓ Simple project executed successfully"),
                Err(e) => info!("✗ Failed to execute Build target: {e}"),
            },
            Err(e) => info!("✗ Could not load simple.proj: {e}"),
        }
    } else {
        info!("Simple project not found at {}", simple_project.display());
    }

    // Test 2: Conditional project
    info!("\n--- Testing Conditional Project ---");
    let conditional_project = PathBuf::from("sample_projects/conditional.proj");
    if conditional_project.exists() {
        let mut evaluator = ProjectEvaluator::new();
        match evaluator.load_project(&conditional_project) {
            Ok(()) => {
                match evaluator.execute_target("Test") {
                    Ok(()) => info!("✓ Conditional Test target executed successfully"),
                    Err(e) => info!("✗ Failed to execute Test target: {e}"),
                }
                match evaluator.execute_target("Build") {
                    Ok(()) => info!("✓ Conditional Build target executed successfully"),
                    Err(e) => info!("✗ Failed to execute Build target: {e}"),
                }
            }
            Err(e) => info!("✗ Could not load conditional.proj: {e}"),
        }
    } else {
        info!(
            "Conditional project not found at {}",
            conditional_project.display()
        );
    }

    // Test 3: Project with imports
    info!("\n--- Testing Project with Imports ---");
    let import_project = PathBuf::from("sample_projects/with_imports.proj");
    if import_project.exists() {
        let mut evaluator = ProjectEvaluator::new();
        match evaluator.load_project(&import_project) {
            Ok(()) => match evaluator.execute_target("Build") {
                Ok(()) => info!("✓ Import project executed successfully"),
                Err(e) => info!("✗ Failed to execute Build target: {e}"),
            },
            Err(e) => info!("✗ Could not load with_imports.proj: {e}"),
        }
    } else {
        info!("Import project not found at {}", import_project.display());
    }

    info!("\n=== Demonstration Complete ===");
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn global_property_uses_name_value_form_and_preserves_equals() {
        assert_eq!(
            parse_global_property("DefineConstants=A=B").unwrap(),
            ("DefineConstants".to_string(), "A=B".to_string())
        );
        assert!(parse_global_property("MissingValueSeparator").is_err());
        assert!(parse_global_property("=empty-name").is_err());
    }
}
