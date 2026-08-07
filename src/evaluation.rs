use anyhow::{Result, anyhow};
use log::{debug, info};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::expression::ExpressionEvaluator;
use crate::loader::load_project;
use crate::object_model::{ProjectModel, PropertyMap};
use crate::tasks::TaskRegistry;

/// Immutable inputs captured once for one or more evaluations.
#[derive(Debug, Clone)]
pub struct EvaluationContext {
    environment: PropertyMap,
    global_properties: PropertyMap,
    sdk_root: Option<PathBuf>,
}

impl EvaluationContext {
    pub fn from_process_environment() -> Self {
        let environment = env::vars_os()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<Vec<_>>();
        Self::with_environment_and_global_properties(environment, Vec::<(String, String)>::new())
    }

    pub fn with_global_properties<I, K, V>(global_properties: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut context = Self::from_process_environment();
        for (name, value) in global_properties {
            context.global_properties.insert(name.into(), value.into());
        }
        context.sdk_root = discover_sdk_root(&context.environment, &context.global_properties);
        context
    }

    pub fn with_environment_and_global_properties<E, G, EK, EV, GK, GV>(
        environment: E,
        global_properties: G,
    ) -> Self
    where
        E: IntoIterator<Item = (EK, EV)>,
        G: IntoIterator<Item = (GK, GV)>,
        EK: Into<String>,
        EV: Into<String>,
        GK: Into<String>,
        GV: Into<String>,
    {
        let mut environment_map = PropertyMap::new();
        for (name, value) in environment {
            environment_map.insert(name.into(), value.into());
        }
        let mut global_map = PropertyMap::new();
        for (name, value) in global_properties {
            global_map.insert(name.into(), value.into());
        }
        let sdk_root = discover_sdk_root(&environment_map, &global_map);
        Self {
            environment: environment_map,
            global_properties: global_map,
            sdk_root,
        }
    }

    pub(crate) fn environment(&self) -> &PropertyMap {
        &self.environment
    }

    pub(crate) fn global_properties(&self) -> &PropertyMap {
        &self.global_properties
    }

    pub(crate) fn sdk_root(&self) -> Option<&Path> {
        self.sdk_root.as_deref()
    }
}

impl Default for EvaluationContext {
    fn default() -> Self {
        Self::from_process_environment()
    }
}

/// A finalized project made only of immutable, owned data.
#[derive(Debug)]
pub struct EvaluatedProject {
    model: ProjectModel,
}

impl EvaluatedProject {
    #[allow(dead_code)] // Public sharing API for library extraction.
    pub fn model(&self) -> &ProjectModel {
        &self.model
    }
}

pub struct ProjectEvaluator {
    context: EvaluationContext,
    project: Option<Arc<EvaluatedProject>>,
    project_path: Option<PathBuf>,
    preprocessed: Option<String>,
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
        Self::with_context(EvaluationContext::default())
    }

    pub fn with_global_properties<I, K, V>(global_properties: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self::with_context(EvaluationContext::with_global_properties(global_properties))
    }

    pub fn with_context(context: EvaluationContext) -> Self {
        Self {
            context,
            project: None,
            project_path: None,
            preprocessed: None,
            task_registry: TaskRegistry::new(),
        }
    }

    pub fn load_project<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.load_internal(path.as_ref(), false)
    }

    /// Evaluate and preprocess in one parse pass, then write the aggregate.
    pub fn load_project_and_write_preprocessed<P: AsRef<Path>, O: AsRef<Path>>(
        &mut self,
        project_path: P,
        output_path: O,
    ) -> Result<()> {
        self.load_internal(project_path.as_ref(), true)?;
        fs::write(
            output_path,
            self.preprocessed
                .as_ref()
                .ok_or_else(|| anyhow!("Preprocessed output was not produced"))?,
        )?;
        Ok(())
    }

    fn load_internal(&mut self, path: &Path, render_preprocessed: bool) -> Result<()> {
        info!("Loading project: {}", path.display());
        let output = load_project(&self.context, path, render_preprocessed)?;
        let project_path = output
            .model
            .project_file_path
            .clone()
            .ok_or_else(|| anyhow!("Loaded project has no project path"))?;

        debug!("Loaded {} properties", output.model.properties.len());
        debug!("Loaded {} item types", output.model.items.len());
        debug!("Loaded {} targets", output.model.targets.len());
        self.project = Some(Arc::new(EvaluatedProject {
            model: output.model,
        }));
        self.project_path = Some(project_path);
        self.preprocessed = output.preprocessed;
        Ok(())
    }

    pub fn execute_target(&self, target_name: &str) -> Result<()> {
        info!("Executing target: {target_name}");
        let mut executed_targets = HashSet::new();
        self.execute_target_recursive(target_name, &mut executed_targets)
    }

    #[allow(dead_code)] // Retained for callers that request preprocessing after loading.
    pub fn write_preprocessed_project<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        if let Some(output) = &self.preprocessed {
            fs::write(path, output)?;
            return Ok(());
        }
        let project_path = self
            .project_path
            .as_ref()
            .ok_or_else(|| anyhow!("Cannot preprocess a project that has not been loaded"))?;
        let output = load_project(&self.context, project_path, true)?
            .preprocessed
            .ok_or_else(|| anyhow!("Preprocessed output was not produced"))?;
        fs::write(path, output)?;
        Ok(())
    }

    /// Select evaluated properties and items without executing targets.
    pub fn query_evaluation(
        &self,
        property_names: &[String],
        item_types: &[String],
    ) -> Result<EvaluationQueryResult> {
        let model = self.model()?;
        let mut properties = BTreeMap::new();
        let mut items = BTreeMap::new();

        for name in property_names {
            properties.insert(
                name.clone(),
                model.get_property(name).cloned().unwrap_or_default(),
            );
        }

        for item_type in item_types {
            let queried_items = model
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

    /// Share the immutable evaluated result without cloning its data.
    #[allow(dead_code)] // Public sharing API for library extraction.
    pub fn evaluated_project(&self) -> Result<Arc<EvaluatedProject>> {
        self.project
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("No project has been loaded"))
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

        let model = self.model()?;
        let target = model
            .get_target(target_name)
            .ok_or_else(|| anyhow!("Target not found: {target_name}"))?
            .clone();

        if let Some(condition) = &target.condition
            && !ExpressionEvaluator::with_current_file(model, &target.source_file)
                .evaluate_condition(condition)?
        {
            info!("Skipping target {target_name} due to condition: {condition}");
            return Ok(());
        }

        for dependency in &target.depends_on {
            self.execute_target_recursive(dependency, executed_targets)?;
        }

        info!("Executing target: {}", target.name);
        executed_targets.insert(target_name.to_string());
        for task in &target.tasks {
            debug!("Executing task: {}", task.name);
            self.task_registry
                .execute_task(task, model, &target.source_file)?;
        }
        Ok(())
    }

    fn model(&self) -> Result<&ProjectModel> {
        self.project
            .as_ref()
            .map(|project| &project.model)
            .ok_or_else(|| anyhow!("No project has been loaded"))
    }

    #[allow(dead_code)]
    pub fn get_model(&self) -> &ProjectModel {
        &self
            .project
            .as_ref()
            .expect("a project must be loaded before accessing its model")
            .model
    }
}

impl Default for ProjectEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

fn discover_sdk_root(
    environment: &PropertyMap,
    global_properties: &PropertyMap,
) -> Option<PathBuf> {
    if let Some(path) = global_properties
        .get("MSBuildSDKsPath")
        .or_else(|| environment.get("MSBuildSDKsPath"))
    {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Some(path);
        }
    }

    let dotnet_root = environment
        .get("DOTNET_ROOT")
        .map(PathBuf::from)
        .or_else(|| default_dotnet_root(environment))?;
    let mut versions = fs::read_dir(dotnet_root.join("sdk"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("Sdks").is_dir())
        .collect::<Vec<_>>();
    versions.sort_by_key(|entry| version_key(&entry.file_name().to_string_lossy()));
    versions.last().map(|entry| entry.path().join("Sdks"))
}

fn default_dotnet_root(environment: &PropertyMap) -> Option<PathBuf> {
    if cfg!(windows) {
        environment
            .get("ProgramFiles")
            .map(|path| PathBuf::from(path).join("dotnet"))
    } else {
        [
            PathBuf::from("/usr/share/dotnet"),
            PathBuf::from("/usr/local/share/dotnet"),
        ]
        .into_iter()
        .find(|path| path.is_dir())
    }
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use tempfile::TempDir;

    fn write_project(directory: &TempDir, name: &str, contents: &str) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn upstream_all_evaluated_properties_uses_each_preceding_value() -> Result<()> {
        // Port of dotnet/msbuild Evaluator_Tests.AllEvaluatedProperties.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup>
    <p>1</p>
    <p>2</p>
    <p Condition="false">3</p>
    <p>$(p);2</p>
  </PropertyGroup>
  <PropertyGroup Condition="false"><p>3</p></PropertyGroup>
  <PropertyGroup><r>4</r></PropertyGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::with_context(
            EvaluationContext::with_environment_and_global_properties(
                Vec::<(String, String)>::new(),
                Vec::<(String, String)>::new(),
            ),
        );
        evaluator.load_project(project)?;

        assert_eq!(evaluator.get_model().get_property("P").unwrap(), "2;2");
        assert_eq!(evaluator.get_model().get_property("r").unwrap(), "4");
        Ok(())
    }

    #[test]
    fn upstream_self_reference_before_set_and_mutual_references_are_single_pass() -> Result<()> {
        // Ports Evaluator_Tests.SetPropertyToItself and UsePropertyBeforeSet.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><PropertyGroup>
  <baz>$(baz);I am some text</baz>
  <bar>STUFF $(baz) STUFF</bar>
  <Before>$(Later)</Before>
  <Later>later</Later>
  <MutualA>$(MutualB)-a</MutualA>
  <MutualB>$(MutualA)-b</MutualB>
</PropertyGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();

        assert_eq!(model.get_property("baz").unwrap(), ";I am some text");
        assert_eq!(
            model.get_property("bar").unwrap(),
            "STUFF ;I am some text STUFF"
        );
        assert_eq!(model.get_property("Before").unwrap(), "");
        assert_eq!(model.get_property("MutualA").unwrap(), "-a");
        assert_eq!(model.get_property("MutualB").unwrap(), "-a-b");
        Ok(())
    }

    #[test]
    fn raw_initial_values_are_literal_and_global_properties_win() -> Result<()> {
        // Representative port of VerifyOnlySpecifiedPropertiesOverridden and
        // MSBuildExtensionsPathWithEnvironmentOverride using injected inputs.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><PropertyGroup>
  <B>body-b</B>
  <GlobalChoice>project</GlobalChoice>
  <EnvironmentChoice>project-overrides-environment</EnvironmentChoice>
  <FromGlobal>$(RawGlobal)</FromGlobal>
  <FromEnvironment>$(RawEnvironment)</FromEnvironment>
  <FromRawCycle>$(RawCycle)</FromRawCycle>
</PropertyGroup></Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            [
                ("EnvironmentChoice", "environment"),
                ("RawEnvironment", "$(B)-environment"),
                ("RawCycle", "$(RawCycle)"),
                ("MSBuildExtensionsPath", "injected-extensions"),
            ],
            [("GlobalChoice", "global"), ("RawGlobal", "$(B)-global")],
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project(project)?;
        let model = evaluator.get_model();

        assert_eq!(model.get_property("GlobalChoice").unwrap(), "global");
        assert_eq!(
            model.get_property("EnvironmentChoice").unwrap(),
            "project-overrides-environment"
        );
        assert_eq!(model.get_property("FromGlobal").unwrap(), "$(B)-global");
        assert_eq!(
            model.get_property("FromEnvironment").unwrap(),
            "$(B)-environment"
        );
        assert_eq!(model.get_property("FromRawCycle").unwrap(), "$(RawCycle)");
        assert_eq!(
            model.get_property("MSBuildExtensionsPath").unwrap(),
            "injected-extensions"
        );
        Ok(())
    }

    #[test]
    fn upstream_reserved_project_properties_and_current_file_properties() -> Result<()> {
        // Port of Evaluator_Tests.ReservedProjectProperties and
        // MSBuildThisFileProperties.
        let directory = TempDir::new()?;
        let import_directory = directory.path().join("imports");
        fs::create_dir(&import_directory)?;
        write_project(
            &directory,
            "imports/current.props",
            r#"<Project><PropertyGroup>
  <CapturedThisFile>$(MSBuildThisFile)</CapturedThisFile>
  <CapturedThisDirectory>$(MSBuildThisFileDirectory)</CapturedThisDirectory>
</PropertyGroup></Project>"#,
        );
        let project = write_project(
            &directory,
            "sample.csproj",
            r#"<Project><Import Project="imports/current.props" /></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(&project)?;
        let model = evaluator.get_model();
        let full_path = project.canonicalize()?;

        assert_eq!(
            model.get_property("MSBuildProjectFullPath").unwrap(),
            &crate::properties::display_path(&full_path)
        );
        assert_eq!(
            model.get_property("MSBuildProjectFile").unwrap(),
            "sample.csproj"
        );
        assert_eq!(
            model.get_property("MSBuildProjectExtension").unwrap(),
            ".csproj"
        );
        assert_eq!(model.get_property("MSBuildProjectName").unwrap(), "sample");
        assert_eq!(
            model.get_property("CapturedThisFile").unwrap(),
            "current.props"
        );
        assert!(
            model
                .get_property("CapturedThisDirectory")
                .unwrap()
                .ends_with(std::path::MAIN_SEPARATOR)
        );
        assert_eq!(
            model.get_property("MSBuildThisFile").unwrap(),
            "sample.csproj"
        );
        Ok(())
    }

    #[test]
    fn reserved_properties_cannot_be_redefined_or_supplied_as_globals() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            "<Project><PropertyGroup><MSBuildProjectName>other</MSBuildProjectName></PropertyGroup></Project>",
        );
        let mut evaluator = ProjectEvaluator::new();
        assert!(
            evaluator
                .load_project(&project)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );

        fs::write(&project, "<Project />")?;
        let mut evaluator =
            ProjectEvaluator::with_global_properties([("MSBuildProjectName", "other")]);
        assert!(
            evaluator
                .load_project(project)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        Ok(())
    }

    #[test]
    fn imports_observe_parent_state_and_evaluate_at_source_position() -> Result<()> {
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "values.props",
            r#"<Project><PropertyGroup>
  <ParentSeen>$(ParentBeforeImport)</ParentSeen>
  <OrderedValue>from-import</OrderedValue>
</PropertyGroup>
<Target Name="ImportedTarget">
  <Error Text="wrong current file" Condition="'$(MSBuildThisFile)' != 'values.props'" />
</Target>
</Project>"#,
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup>
    <ParentBeforeImport>visible</ParentBeforeImport>
    <OrderedValue>before-import</OrderedValue>
  </PropertyGroup>
  <Import Project="values.props" Condition="'$(ParentBeforeImport)' == 'visible'" />
  <PropertyGroup>
    <ValueSeenAfterImport>$(OrderedValue)</ValueSeenAfterImport>
    <OrderedValue>after-import</OrderedValue>
  </PropertyGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();

        assert_eq!(model.get_property("ParentSeen").unwrap(), "visible");
        assert_eq!(
            model.get_property("ValueSeenAfterImport").unwrap(),
            "from-import"
        );
        assert_eq!(model.get_property("OrderedValue").unwrap(), "after-import");
        evaluator.execute_target("ImportedTarget")?;
        Ok(())
    }

    #[test]
    fn finalized_project_is_send_sync_and_arc_shared() -> Result<()> {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvaluatedProject>();

        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            "<Project><PropertyGroup><P>value</P></PropertyGroup></Project>",
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let first = evaluator.evaluated_project()?;
        let second = evaluator.evaluated_project()?;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            std::thread::spawn(move || second.model().get_property("p").cloned())
                .join()
                .unwrap(),
            Some("value".to_string())
        );
        Ok(())
    }

    #[test]
    fn indexed_property_lookup_scales_across_many_assignments() -> Result<()> {
        let directory = TempDir::new()?;
        let mut xml = String::from("<Project><PropertyGroup><Seed>x</Seed>");
        for index in 0..5_000 {
            write!(
                &mut xml,
                "<Property{index}>$(sEeD)-{index}</Property{index}>"
            )?;
        }
        xml.push_str("</PropertyGroup></Project>");
        let project = write_project(&directory, "scale.proj", &xml);
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;

        assert_eq!(
            evaluator.get_model().get_property("PROPERTY4999").unwrap(),
            "x-4999"
        );
        Ok(())
    }

    #[test]
    fn preprocessing_uses_the_same_ordered_evaluator() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <!-- Keep this comment. -->
  <PropertyGroup><ImportName>values.props</ImportName></PropertyGroup>
  <ImportGroup Condition="'$(ImportName)' != ''">
    <Import Project="$(ImportName)" />
  </ImportGroup>
  <PropertyGroup><After>$(Imported)</After></PropertyGroup>
</Project>"#,
        );
        write_project(
            &directory,
            "values.props",
            "<Project><PropertyGroup><Imported>A &amp; B</Imported></PropertyGroup></Project>",
        );
        let output_path = directory.path().join("out.xml");
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project_and_write_preprocessed(&project, &output_path)?;
        let output = fs::read_to_string(output_path)?;

        assert!(output.contains("<!-- Keep this comment. -->"));
        assert!(output.contains("Project=\"$(ImportName)\""));
        assert!(output.contains("<!--<ImportGroup Condition="));
        assert!(output.contains("<!--</ImportGroup>-->"));
        assert!(output.contains("<Imported>A &amp; B</Imported>"));
        assert_eq!(output.matches("<Project").count(), 1);
        assert_eq!(output.matches("</Project>").count(), 1);
        assert_eq!(
            evaluator.get_model().get_property("After").unwrap(),
            "A & B"
        );
        Ok(())
    }

    #[test]
    fn implicit_sdk_imports_are_evaluated_before_and_after_the_project() -> Result<()> {
        let directory = TempDir::new()?;
        let sdk_root = directory.path().join("Sdks");
        let sdk_directory = sdk_root.join("Test.Sdk").join("Sdk");
        fs::create_dir_all(&sdk_directory)?;
        fs::write(
            sdk_directory.join("Sdk.props"),
            "<Project><PropertyGroup><Order>props</Order></PropertyGroup></Project>",
        )?;
        fs::write(
            sdk_directory.join("Sdk.targets"),
            "<Project><PropertyGroup><AfterProject>$(Order)</AfterProject></PropertyGroup></Project>",
        )?;
        let project = write_project(
            &directory,
            "sdk.proj",
            r#"<Project Sdk="Test.Sdk"><PropertyGroup>
  <SawProps>$(Order)</SawProps><Order>project</Order>
</PropertyGroup></Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            Vec::<(String, String)>::new(),
            [(
                "MSBuildSDKsPath".to_string(),
                crate::properties::display_path(&sdk_root),
            )],
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        let output_path = directory.path().join("out.xml");
        evaluator.load_project_and_write_preprocessed(&project, &output_path)?;

        assert_eq!(
            evaluator.get_model().get_property("SawProps").unwrap(),
            "props"
        );
        assert_eq!(
            evaluator.get_model().get_property("AfterProject").unwrap(),
            "project"
        );
        let output = fs::read_to_string(output_path)?;
        let props = output.find("<Order>props</Order>").unwrap();
        let project_body = output.find("<SawProps>$(Order)</SawProps>").unwrap();
        let targets = output
            .find("<AfterProject>$(Order)</AfterProject>")
            .unwrap();
        assert!(props < project_body && project_body < targets);
        assert!(output.contains("This import was added implicitly"));
        Ok(())
    }
}
