mod evaluation;
mod expression;
mod logger;
mod object_model;
mod parser;
mod tasks;
mod tests;

use anyhow::Result;
use clap::Parser;
use log::info;
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

    let mut evaluator = ProjectEvaluator::new();
    evaluator.load_project(&project_path)?;
    evaluator.execute_target(&args.target)?;

    info!("Build completed successfully");
    Ok(())
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
