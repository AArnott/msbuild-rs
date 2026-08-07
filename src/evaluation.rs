use anyhow::{Result, anyhow};
use log::{debug, info};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use crate::expression::ExpressionEvaluator;
use crate::loader::load_project;
use crate::object_model::{ProjectModel, PropertyMap};
use crate::tasks::TaskRegistry;

/// Immutable inputs captured once for one or more evaluations.
#[derive(Debug, Clone)]
pub struct EvaluationContext {
    environment: PropertyMap,
    global_properties: PropertyMap,
    sdk_root_override: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveToolset {
    pub sdk_root: PathBuf,
    pub tools_path: PathBuf,
    pub msbuild_version: String,
    pub msbuild_semantic_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HostResolutionKey {
    global_json_path: Option<PathBuf>,
    global_json_contents: Option<String>,
    dotnet_host_path: Option<String>,
    dotnet_root: Option<String>,
    path: Option<String>,
}

static HOST_RESOLUTIONS: OnceLock<Mutex<HashMap<HostResolutionKey, Option<ActiveToolset>>>> =
    OnceLock::new();

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
        context.sdk_root_override =
            explicit_sdk_root(&context.environment, &context.global_properties);
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
        let sdk_root_override = explicit_sdk_root(&environment_map, &global_map);
        Self {
            environment: environment_map,
            global_properties: global_map,
            sdk_root_override,
        }
    }

    pub(crate) fn environment(&self) -> &PropertyMap {
        &self.environment
    }

    pub(crate) fn global_properties(&self) -> &PropertyMap {
        &self.global_properties
    }

    pub(crate) fn sdk_root_override(&self) -> Option<&Path> {
        self.sdk_root_override.as_deref()
    }

    pub(crate) fn resolve_toolset(&self, project_path: &Path) -> Option<ActiveToolset> {
        resolve_dotnet_toolset(&self.environment, project_path)
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
                                .evaluated_metadata()
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

fn explicit_sdk_root(
    environment: &PropertyMap,
    global_properties: &PropertyMap,
) -> Option<PathBuf> {
    global_properties
        .get("MSBuildSDKsPath")
        .or_else(|| environment.get("MSBuildSDKsPath"))
        .map(PathBuf::from)
}

fn resolve_dotnet_toolset(environment: &PropertyMap, project_path: &Path) -> Option<ActiveToolset> {
    let project_directory = project_path.parent().unwrap_or_else(|| Path::new(""));
    let (global_json_path, global_json_contents) = nearest_global_json(project_directory)
        .map(|path| {
            let contents = fs::read_to_string(&path).ok();
            (Some(path), contents)
        })
        .unwrap_or((None, None));
    let key = HostResolutionKey {
        global_json_path,
        global_json_contents,
        dotnet_host_path: environment.get("DOTNET_HOST_PATH").cloned(),
        dotnet_root: environment.get("DOTNET_ROOT").cloned(),
        path: environment.get("PATH").cloned(),
    };
    let cache = HOST_RESOLUTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(resolution) = cache.lock().ok()?.get(&key).cloned() {
        return resolution;
    }

    let resolution = invoke_dotnet_host(environment, project_directory);
    cache.lock().ok()?.insert(key, resolution.clone());
    resolution
}

fn nearest_global_json(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .map(|ancestor| ancestor.join("global.json"))
        .find(|candidate| candidate.is_file())
        .and_then(|path| path.canonicalize().ok().or(Some(path)))
}

fn invoke_dotnet_host(
    environment: &PropertyMap,
    project_directory: &Path,
) -> Option<ActiveToolset> {
    let executable = environment
        .get("DOTNET_HOST_PATH")
        .map(String::as_str)
        .unwrap_or("dotnet");
    let mut command = Command::new(executable);
    command
        .arg("--info")
        .current_dir(project_directory)
        .envs(environment.iter())
        .env("DOTNET_CLI_UI_LANGUAGE", "en-US")
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_dotnet_info(&String::from_utf8_lossy(&output.stdout))
}

fn parse_dotnet_info(output: &str) -> Option<ActiveToolset> {
    let base_path = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Base Path:")
            .map(str::trim)
            .filter(|path| !path.is_empty())
    })?;
    let tools_path = PathBuf::from(base_path.trim_end_matches(['/', '\\']));
    let sdk_root = tools_path.join("Sdks");
    if !sdk_root.is_dir() {
        return None;
    }
    let msbuild_semantic_version = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("MSBuild version:").map(str::trim))
        .unwrap_or_default()
        .to_string();
    let msbuild_version = msbuild_semantic_version
        .split(['+', '-'])
        .next()
        .unwrap_or_default()
        .to_string();
    Some(ActiveToolset {
        sdk_root,
        tools_path,
        msbuild_version,
        msbuild_semantic_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::properties::display_path;
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
            r#"<Project>
  <Import Project="imports/current.props" />
  <PropertyGroup>
    <CapturedRootThisFile>$(MSBuildThisFile)</CapturedRootThisFile>
    <CapturedRootThisFullPath>$(MSBuildThisFileFullPath)</CapturedRootThisFullPath>
  </PropertyGroup>
</Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            [("MSBuildThisFile", "environment-must-not-leak")],
            Vec::<(String, String)>::new(),
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project(&project)?;
        let model = evaluator.get_model();
        let full_path = crate::properties::lexical_absolute(&project)?;

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
            model.get_property("CapturedRootThisFile").unwrap(),
            "sample.csproj"
        );
        assert_eq!(
            model.get_property("CapturedRootThisFullPath").unwrap(),
            &crate::properties::display_path(&full_path)
        );
        assert!(model.get_property("MSBuildThisFile").is_none());
        assert!(model.get_property("MSBuildThisFileFullPath").is_none());
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
        let error = evaluator.load_project(&project).unwrap_err().to_string();
        assert!(error.contains("MSB4004"));
        assert!(error.contains("reserved"));

        fs::write(&project, "<Project />")?;
        let mut evaluator =
            ProjectEvaluator::with_global_properties([("MSBuildToolsPath", "other")]);
        let error = evaluator.load_project(project).unwrap_err().to_string();
        assert_eq!(
            error,
            "MSB4177: Invalid property. The \"MSBuildToolsPath\" property name is reserved."
        );
        Ok(())
    }

    #[test]
    fn empty_project_is_valid() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(&directory, "empty.proj", "<Project />");
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("MSBuildProjectName")
                .map(String::as_str),
            Some("empty")
        );
        Ok(())
    }

    #[test]
    fn host_resolved_sdk_and_toolset_match_the_repo_global_json() -> Result<()> {
        let manifest_directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        let project = manifest_directory.join("fixtures/evaluation/basic/project.proj");
        let global_json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(manifest_directory.join("global.json"))?)?;
        let pinned_version = global_json["sdk"]["version"]
            .as_str()
            .ok_or_else(|| anyhow!("global.json has no SDK version"))?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let sdk_root = Path::new(model.get_property("MSBuildSDKsPath").unwrap());
        let selected_version = sdk_root
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        assert_eq!(selected_version, pinned_version);
        assert!(!selected_version.contains('-'));
        assert_eq!(
            model.get_property("MSBuildToolsPath"),
            model.get_property("MSBuildBinPath")
        );
        assert_eq!(
            model.get_property("MSBuildRuntimeType").map(String::as_str),
            Some("Core")
        );
        assert!(
            model
                .get_property("MSBuildVersion")
                .is_some_and(|version| !version.is_empty())
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
    fn observable_paths_preserve_lexical_spelling_while_imports_resolve_relatively() -> Result<()> {
        let directory = TempDir::new()?;
        fs::create_dir(directory.path().join("imports"))?;
        write_project(
            &directory,
            "imports/CHILD.props",
            r#"<Project><PropertyGroup>
  <ImportedFullPath>$(MSBuildThisFileFullPath)</ImportedFullPath>
  <ImportedCount>$(ImportedCount)x</ImportedCount>
</PropertyGroup></Project>"#,
        );
        let project = write_project(
            &directory,
            "sample.proj",
            r#"<Project>
  <Import Project="imports/../imports/CHILD.props" />
  <Import Project="imports/CHILD.props" />
  <PropertyGroup><RootFullPath>$(MSBuildThisFileFullPath)</RootFullPath></PropertyGroup>
</Project>"#,
        );
        let lexical_input = if cfg!(windows) {
            directory.path().join("SAMPLE.proj")
        } else {
            project
        };
        let expected_root = crate::properties::lexical_absolute(&lexical_input)?;
        let expected_import =
            crate::properties::lexical_absolute(&directory.path().join("imports/CHILD.props"))?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(&lexical_input)?;
        let model = evaluator.get_model();
        assert_eq!(
            model.get_property("MSBuildProjectFullPath").unwrap(),
            &crate::properties::display_path(&expected_root)
        );
        assert_eq!(
            model.get_property("RootFullPath").unwrap(),
            &crate::properties::display_path(&expected_root)
        );
        assert_eq!(
            model.get_property("ImportedFullPath").unwrap(),
            &crate::properties::display_path(&expected_import)
        );
        assert_eq!(
            model.get_property("ImportedCount").map(String::as_str),
            Some("x")
        );
        Ok(())
    }

    #[test]
    fn upstream_imports_only_included_once_uses_normalized_lexical_identity() -> Result<()> {
        // Port of dotnet/msbuild Evaluator_Tests.ImportsOnlyIncludedOnce.
        let directory = TempDir::new()?;
        fs::create_dir(directory.path().join("imports"))?;
        write_project(
            &directory,
            "imports/common.props",
            r#"<Project><PropertyGroup>
  <ImportCount>$(ImportCount)x</ImportCount>
  <ImportedCurrentFile Condition="'$(MSBuildThisFile)' == 'common.props'">yes</ImportedCurrentFile>
</PropertyGroup></Project>"#,
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <Import Project="imports/../imports/common.props" />
  <Import Project="imports/common.props" />
  <ImportGroup Condition="'$(MSBuildThisFile)' == 'project.proj'">
    <Import Project="imports/common.props" Condition="'$(MSBuildThisFile)' == 'project.proj'" />
  </ImportGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;

        assert_eq!(
            evaluator
                .get_model()
                .get_property("ImportCount")
                .map(String::as_str),
            Some("x")
        );
        assert_eq!(
            evaluator
                .get_model()
                .get_property("ImportedCurrentFile")
                .map(String::as_str),
            Some("yes")
        );
        Ok(())
    }

    #[test]
    fn circular_imports_are_diagnosed_and_skipped_by_default() -> Result<()> {
        // MSBuild only rejects these when ProjectLoadSettings.RejectCircularImports is set.
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "a.proj",
            r#"<Project><Import Project="b.props" /></Project>"#,
        );
        write_project(
            &directory,
            "b.props",
            r#"<Project><PropertyGroup><ImportedBeforeCycle>yes</ImportedBeforeCycle></PropertyGroup><Import Project="./a.proj" /></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(directory.path().join("a.proj"))?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("ImportedBeforeCycle")
                .map(String::as_str),
            Some("yes")
        );
        Ok(())
    }

    #[test]
    fn upstream_verify_loading_import_scenarios_handles_conditions_and_sorted_globs() -> Result<()>
    {
        // Representative file-based port of Evaluator_Tests.VerifyLoadingImportScenarios.
        let directory = TempDir::new()?;
        fs::create_dir(directory.path().join("imports"))?;
        write_project(
            &directory,
            "imports/a.props",
            "<Project><PropertyGroup><Order>$(Order)a</Order></PropertyGroup></Project>",
        );
        write_project(
            &directory,
            "imports/b.props",
            "<Project><PropertyGroup><Order>$(Order)b</Order></PropertyGroup></Project>",
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup><Enabled>true</Enabled></PropertyGroup>
  <ImportGroup Condition="'$(MSBuildThisFile)' == 'project.proj' And '$(Enabled)' == 'true'">
    <Import Project="imports/*.props" />
    <Import Project="missing.props" Condition="false" />
  </ImportGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("Order")
                .map(String::as_str),
            Some("ab")
        );
        Ok(())
    }

    #[test]
    fn upstream_all_evaluated_items_choose_selects_only_the_first_true_branch() -> Result<()> {
        // Representative port of Evaluator_Tests.AllEvaluatedItems Choose coverage.
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "selected.props",
            "<Project><PropertyGroup><ImportedFromChoose>yes</ImportedFromChoose></PropertyGroup></Project>",
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup><Selection>first</Selection></PropertyGroup>
  <Choose>
    <When Condition="'$(Selection)' == 'first'">
      <PropertyGroup><ChooseValue>first</ChooseValue></PropertyGroup>
      <ItemGroup><Chosen Include="first-item" /></ItemGroup>
      <Choose>
        <When Condition="false"><PropertyGroup><Nested>wrong</Nested></PropertyGroup></When>
        <Otherwise><PropertyGroup><Nested>otherwise</Nested></PropertyGroup></Otherwise>
      </Choose>
    </When>
    <When Condition="true">
      <PropertyGroup><ChooseValue>second</ChooseValue></PropertyGroup>
      <ItemGroup><Chosen Include="second-item" /></ItemGroup>
    </When>
    <Otherwise><PropertyGroup><ChooseValue>otherwise</ChooseValue></PropertyGroup></Otherwise>
  </Choose>
  <Import Project="selected.props" Condition="'$(ChooseValue)' == 'first'" />
</Project>"#,
        );
        let output_path = directory.path().join("out.xml");
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project_and_write_preprocessed(project, &output_path)?;
        let model = evaluator.get_model();

        assert_eq!(
            model.get_property("ChooseValue").map(String::as_str),
            Some("first")
        );
        assert_eq!(
            model.get_property("Nested").map(String::as_str),
            Some("otherwise")
        );
        assert_eq!(
            model.get_property("ImportedFromChoose").map(String::as_str),
            Some("yes")
        );
        assert_eq!(
            model
                .get_items("Chosen")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first-item"]
        );
        let output = fs::read_to_string(output_path)?;
        assert!(output.contains("<ImportedFromChoose>yes</ImportedFromChoose>"));
        Ok(())
    }

    #[test]
    fn self_closing_when_selects_its_empty_branch() -> Result<()> {
        let directory = TempDir::new()?;
        let selected = write_project(
            &directory,
            "selected.proj",
            r#"<Project><Choose>
  <When Condition="true" />
  <Otherwise><PropertyGroup><Branch>otherwise</Branch></PropertyGroup></Otherwise>
</Choose></Project>"#,
        );
        let unselected = write_project(
            &directory,
            "unselected.proj",
            r#"<Project><Choose>
  <When Condition="false" />
  <Otherwise><PropertyGroup><Branch>otherwise</Branch></PropertyGroup></Otherwise>
</Choose></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(selected)?;
        assert_eq!(evaluator.get_model().get_property("Branch"), None);

        evaluator.load_project(unselected)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("Branch")
                .map(String::as_str),
            Some("otherwise")
        );
        Ok(())
    }

    #[test]
    fn choose_structure_is_validated_even_in_inactive_branches() -> Result<()> {
        let directory = TempDir::new()?;
        let inactive_unknown = write_project(
            &directory,
            "inactive-unknown.proj",
            r#"<Project><Choose><When Condition="false"><Bogus /></When><Otherwise /></Choose></Project>"#,
        );
        let error = ProjectEvaluator::new()
            .load_project(inactive_unknown)
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4067"));
        assert!(error.contains("<Bogus>"));
        assert!(error.contains("<When>"));

        let invalid_order = write_project(
            &directory,
            "invalid-order.proj",
            r#"<Project><Choose><When Condition="false" /><Otherwise /><When Condition="true" /></Choose></Project>"#,
        );
        let error = ProjectEvaluator::new()
            .load_project(invalid_order)
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4084"));

        let missing_when = write_project(
            &directory,
            "missing-when.proj",
            r#"<Project><Choose><Otherwise /></Choose></Project>"#,
        );
        let error = ProjectEvaluator::new()
            .load_project(missing_when)
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4085"));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn import_identity_is_lexical_and_does_not_resolve_symlinks() -> Result<()> {
        use std::os::windows::fs::symlink_file;

        let directory = TempDir::new()?;
        let imported = write_project(
            &directory,
            "real.props",
            "<Project><PropertyGroup><ImportCount>$(ImportCount)x</ImportCount></PropertyGroup></Project>",
        );
        let link = directory.path().join("linked.props");
        if let Err(error) = symlink_file(&imported, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return Ok(());
            }
            return Err(error.into());
        }
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><Import Project="real.props" /><Import Project="linked.props" /></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("ImportCount")
                .map(String::as_str),
            Some("xx")
        );
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
        let custom_toolset_candidate = sdk_root
            .parent()
            .map(crate::properties::display_path)
            .unwrap_or_default();
        assert_ne!(
            evaluator
                .get_model()
                .get_property("MSBuildToolsPath")
                .map(String::as_str),
            Some(custom_toolset_candidate.as_str())
        );
        Ok(())
    }

    #[test]
    fn top_level_sdk_elements_import_all_props_and_targets_in_declaration_order() -> Result<()> {
        let directory = TempDir::new()?;
        let sdk_root = directory.path().join("Sdks");
        for sdk in ["Top.One", "Top.Two"] {
            fs::create_dir_all(sdk_root.join(sdk).join("Sdk"))?;
        }
        fs::write(
            sdk_root.join("Top.One/Sdk/Sdk.props"),
            "<Project><PropertyGroup><Order>one-props</Order></PropertyGroup></Project>",
        )?;
        fs::write(
            sdk_root.join("Top.Two/Sdk/Sdk.props"),
            "<Project><PropertyGroup><TwoSaw>$(Order)</TwoSaw><Order>two-props</Order></PropertyGroup></Project>",
        )?;
        fs::write(
            sdk_root.join("Top.One/Sdk/Sdk.targets"),
            "<Project><PropertyGroup><Final>$(Order)</Final><Order>one-targets</Order></PropertyGroup></Project>",
        )?;
        fs::write(
            sdk_root.join("Top.Two/Sdk/Sdk.targets"),
            "<Project><PropertyGroup><TwoTargetSaw>$(Order)</TwoTargetSaw><Order>two-targets</Order></PropertyGroup></Project>",
        )?;
        let project = write_project(
            &directory,
            "top-level.proj",
            r#"<Project>
  <Sdk Name="Top.One" />
  <PropertyGroup><BodySaw>$(Order)</BodySaw><Order>body</Order></PropertyGroup>
  <Sdk Name="Top.Two" Version="1.2.3" />
</Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            Vec::<(String, String)>::new(),
            [(
                "MSBuildSDKsPath".to_string(),
                crate::properties::display_path(&sdk_root),
            )],
        );
        let output_path = directory.path().join("out.xml");
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project_and_write_preprocessed(project, &output_path)?;
        let model = evaluator.get_model();

        assert_eq!(
            model.get_property("TwoSaw").map(String::as_str),
            Some("one-props")
        );
        assert_eq!(
            model.get_property("BodySaw").map(String::as_str),
            Some("two-props")
        );
        assert_eq!(
            model.get_property("Final").map(String::as_str),
            Some("body")
        );
        assert_eq!(
            model.get_property("TwoTargetSaw").map(String::as_str),
            Some("one-targets")
        );
        assert_eq!(
            model.get_property("Order").map(String::as_str),
            Some("two-targets")
        );

        let output = fs::read_to_string(output_path)?;
        let one_props = output.find("<Order>one-props</Order>").unwrap();
        let two_props = output.find("<TwoSaw>$(Order)</TwoSaw>").unwrap();
        let body = output.find("<BodySaw>$(Order)</BodySaw>").unwrap();
        let one_targets = output.find("<Final>$(Order)</Final>").unwrap();
        let two_targets = output
            .find("<TwoTargetSaw>$(Order)</TwoTargetSaw>")
            .unwrap();
        assert!(one_props < two_props);
        assert!(two_props < body);
        assert!(body < one_targets);
        assert!(one_targets < two_targets);
        assert!(output.contains("Sdk=\"Top.Two/1.2.3\""));
        Ok(())
    }

    #[test]
    fn upstream_escaping_projects_preserves_semicolon_items() -> Result<()> {
        // Exact project-data port of
        // EscapingInProjects_Tests.CanGetCorrectListOfItemsWithSemicolonsInThem.
        let directory = TempDir::new()?;
        let escaped = write_project(
            &directory,
            "escaped.proj",
            r#"<Project>
  <PropertyGroup><MyUserMacro>foo%3bbar</MyUserMacro></PropertyGroup>
  <ItemGroup>
    <DifferentList Include="a" />
    <DifferentList Include="b%3bc" />
    <DifferentList Include="$(MyUserMacro)" />
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(escaped)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_items("DifferentList")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b;c", "foo;bar"]
        );

        // Exact project-data port of
        // EscapingInProjects_Tests.CanGetCorrectListOfItemsWithSemicolonsInThem2.
        let unescaped = write_project(
            &directory,
            "unescaped.proj",
            r#"<Project>
  <PropertyGroup><MyUserMacro>foo;bar</MyUserMacro></PropertyGroup>
  <ItemGroup>
    <DifferentList Include="a" />
    <DifferentList Include="b%3bc" />
    <DifferentList Include="$(MyUserMacro)" />
  </ItemGroup>
</Project>"#,
        );
        evaluator.load_project(unescaped)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_items("DifferentList")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b;c", "foo", "bar"]
        );
        Ok(())
    }

    #[test]
    fn percent_patterns_round_trip_once_and_escaped_wildcards_stay_literal() -> Result<()> {
        // Classification port of EscapingInProjects_Tests.
        // EscapedWildcardsShouldNotBeExpanded and escaping-focused port of
        // ItemGlobs_Tests.PatternsWithPercentEncodingRoundTripAndMatchGetAllGlobs
        // plus PatternContainingSemicolonIsRecoverableFromEscapedMetadata.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup><RoundTrip>a%2512b</RoundTrip></PropertyGroup>
  <ItemGroup>
    <Pattern Include="$(RoundTrip)_%2A.cs;a%3Bb/%3F.cs" />
    <ActualGlob Include="*.cs" />
    <Source Include="x"><PatternMetadata>a;b</PatternMetadata></Source>
    <FromMetadata Include="@(Source->'%(PatternMetadata)')" />
    <FromTemplate Include="@(Source->'%(PatternMetadata);x')" />
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(model.get_property("RoundTrip").unwrap(), "a%12b");
        assert_eq!(
            model
                .get_items("Pattern")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a%12b_*.cs", "a;b/?.cs"]
        );
        assert!(
            model
                .get_items("Pattern")
                .unwrap()
                .iter()
                .all(|item| item.spec_kind == crate::escaping::ItemSpecKind::Literal)
        );
        assert_eq!(
            model.get_items("ActualGlob").unwrap()[0].spec_kind,
            crate::escaping::ItemSpecKind::Glob
        );
        assert_eq!(
            model.get_items("Pattern").unwrap()[1].escaped_name,
            "a%3Bb/%3F.cs"
        );
        assert_eq!(model.get_items("FromMetadata").unwrap().len(), 1);
        assert_eq!(model.get_items("FromMetadata").unwrap()[0].name, "a;b");
        assert_eq!(model.get_items("FromTemplate").unwrap().len(), 1);
        assert_eq!(model.get_items("FromTemplate").unwrap()[0].name, "a;b;x");
        Ok(())
    }

    #[test]
    fn upstream_item_and_metadata_case_changes_find_predecessors() -> Result<()> {
        // Behavioral port of Evaluator_Tests.ItemPredecessorToItemWithCaseChange.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <ItemGroup>
    <item_with_lowercase_name Include="h1"><m>1</m></item_with_lowercase_name>
    <i Include="@(ITEM_WITH_LOWERCASE_NAME)">
      <m>2;%(m)</m>
      <Qualified>%(I.M)</Qualified>
      <CaseInsensitive>%(i.qualified)</CaseInsensitive>
    </i>
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let item = &evaluator.get_model().get_items("I").unwrap()[0];
        assert_eq!(item.get_metadata("M").as_deref(), Some("2;1"));
        assert_eq!(item.get_metadata("qualified").as_deref(), Some("2;1"));
        assert_eq!(item.get_metadata("CASEINSENSITIVE").as_deref(), Some("2;1"));
        Ok(())
    }

    #[test]
    fn upstream_item_definition_predecessor_and_all_evaluated_metadata() -> Result<()> {
        // Ports Evaluator_Tests.ItemDefinitionPredecessorToItemDefinition,
        // ItemDefinitionPredecessorToItem, and AllEvaluatedItemDefinitionMetadata.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <ItemDefinitionGroup>
    <i>
      <m>1</m>
      <n>2</n>
    </i>
  </ItemDefinitionGroup>
  <ItemGroup><i Include="before" /></ItemGroup>
  <ItemDefinitionGroup>
    <I>
      <m>1</m>
      <m Condition="false">3</m>
      <m>%(m);2</m>
    </I>
  </ItemDefinitionGroup>
  <ItemGroup>
    <i Include="one"><m>item;%(m)</m></i>
    <i Include="two" />
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(model.get_item_definition_metadata("I", "M"), Some("1;2"));
        let evaluated = model.all_evaluated_item_definition_metadata();
        assert_eq!(evaluated.len(), 4);
        assert_eq!(
            evaluated
                .iter()
                .map(|metadata| (
                    metadata.item_type.as_str(),
                    metadata.name.as_str(),
                    metadata.value.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("i", "m", "1"),
                ("i", "n", "2"),
                ("I", "m", "1"),
                ("I", "m", "1;2"),
            ]
        );
        let items = model.get_items("i").unwrap();
        assert_eq!(items[0].get_metadata("m").as_deref(), Some("1"));
        assert_eq!(items[1].get_metadata("m").as_deref(), Some("item;1;2"));
        assert_eq!(items[2].get_metadata("M").as_deref(), Some("1;2"));
        Ok(())
    }

    #[test]
    fn upstream_metadata_and_well_known_references_are_case_insensitive() -> Result<()> {
        // Ports Expander_Tests.DirectItemMetadataReferenceShouldBeCaseInsensitive
        // and WellKnownMetadataReferenceShouldBeCaseInsensitive in item context.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "sample.proj",
            r#"<Project><ItemGroup>
  <Foo Include="dir/Foo.cs">
    <SENSITIVE>X</SENSITIVE>
    <QualifiedNotMatchCase>%(Foo.sensitive)</QualifiedNotMatchCase>
    <QualifiedMatchCase>%(Foo.SENSITIVE)</QualifiedMatchCase>
    <UnqualifiedNotMatchCase>%(sensitive)</UnqualifiedNotMatchCase>
    <UnqualifiedMatchCase>%(SENSITIVE)</UnqualifiedMatchCase>
    <WellKnownNotMatchCase>%(Foo.FILENAME)</WellKnownNotMatchCase>
    <WellKnownMatchCase>%(filename)</WellKnownMatchCase>
  </Foo>
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let item = &evaluator.get_model().get_items("fOO").unwrap()[0];
        for name in [
            "QualifiedNotMatchCase",
            "QualifiedMatchCase",
            "UnqualifiedNotMatchCase",
            "UnqualifiedMatchCase",
        ] {
            assert_eq!(item.get_metadata(name).as_deref(), Some("X"));
        }
        assert_eq!(
            item.get_metadata("WellKnownNotMatchCase").as_deref(),
            Some("Foo")
        );
        assert_eq!(
            item.get_metadata("WellKnownMatchCase").as_deref(),
            Some("Foo")
        );
        assert!(
            item.get_metadata("fullpath")
                .unwrap()
                .ends_with(&format!("dir{}Foo.cs", std::path::MAIN_SEPARATOR))
        );
        assert_eq!(item.get_metadata("EXTENSION").as_deref(), Some(".cs"));
        assert_eq!(
            item.get_metadata("relativeDIR").as_deref(),
            Some(format!("dir{}", std::path::MAIN_SEPARATOR).as_str())
        );
        assert_eq!(
            item.get_metadata("DefiningProjectName").as_deref(),
            Some("sample")
        );
        Ok(())
    }

    #[test]
    fn upstream_expand_item_vector_functions_item_spec_modifier() -> Result<()> {
        // Applicable pipeline port of
        // Expander_Tests.ExpandItemVectorFunctionsItemSpecModifier.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <i Include="i0"><Meta0>firstdirectory/seconddirectory/file0.ext</Meta0></i>
  <DirectoryResult Include="@(i->Metadata('Meta0')->Directory())" />
  <FilenameResult Include="@(i->Metadata('Meta0')->Filename())" />
  <ExtensionResult Include="@(i->Metadata('Meta0')->Extension())" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model.get_items("DirectoryResult").unwrap()[0].name,
            format!(
                "firstdirectory{}seconddirectory{}",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            )
        );
        assert_eq!(model.get_items("FilenameResult").unwrap()[0].name, "file0");
        assert_eq!(model.get_items("ExtensionResult").unwrap()[0].name, ".ext");
        Ok(())
    }

    #[test]
    fn xml_entities_cdata_and_whitespace_decode_once_and_dtd_is_rejected() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup>
    <Entity>  A&amp;B%3BC  </Entity>
    <CData><![CDATA[  x&y%2512  ]]></CData>
  </PropertyGroup>
  <ItemGroup>
    <X Include="a&amp;b%3Bc"><M><![CDATA[  m&n%3Bo  ]]></M></X>
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(model.get_property("Entity").unwrap(), "  A&B;C  ");
        assert_eq!(model.get_property("CData").unwrap(), "  x&y%12  ");
        let item = &model.get_items("X").unwrap()[0];
        assert_eq!(item.name, "a&b;c");
        assert_eq!(item.get_metadata("M").as_deref(), Some("  m&n;o  "));

        // Security-focused port of Evaluator_Tests.VerifyDTDProcessingIsDisabled:
        // no DTD or external entity is parsed or resolved.
        let dtd = write_project(
            &directory,
            "dtd.proj",
            r#"<?xml version="1.0"?>
<!DOCTYPE Project [<!ENTITY external SYSTEM "file:///must-not-be-read">]>
<Project><PropertyGroup><P>&external;</P></PropertyGroup></Project>"#,
        );
        let error = evaluator.load_project(dtd).unwrap_err().to_string();
        assert!(error.contains("DTD declarations and external entities are disabled"));
        Ok(())
    }

    #[test]
    fn review_function_operands_provenance_and_structured_items_match_msbuild() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "review.proj",
            r#"<Project>
  <PropertyGroup>
    <P>a%3Bb</P>
    <RawProperty>a;b</RawProperty>
    <EscapedProperty>a%3Bb</EscapedProperty>
    <ContainsResult>$(P.Contains(';'))</ContainsResult>
    <ContainsCondition Condition="$(P.Contains(';')) == True">passed</ContainsCondition>
    <FileNameResult>$([System.IO.Path]::GetFileName('dir%2Ffile.txt'))</FileNameResult>
  </PropertyGroup>
  <ItemDefinitionGroup>
    <Source><Default>source-default</Default></Source>
  </ItemDefinitionGroup>
  <ItemGroup>
    <RawPropertyResult Include="$(RawProperty)" />
    <EscapedPropertyResult Include="$(EscapedProperty)" />
    <FunctionResult Include="$(P.Substring(0,3))" />
    <Raw Include="one"><M>a;b</M><Custom>raw</Custom></Raw>
    <Authored Include="two"><M>a%3Bb</M><Custom>authored</Custom></Authored>
    <RawResult Include="@(Raw->Metadata('M'))" />
    <AuthoredResult Include="@(Authored->Metadata('M'))" />
    <Source Include="dir/one.cs"><Custom>first</Custom></Source>
    <Source Include="dir/one.cs"><Custom>second</Custom></Source>
    <DistinctResult Include="@(Source->Distinct())" />
    <TransformResult Include="@(Source->'%(Filename).out')" />
  </ItemGroup>
</Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model.get_property("ContainsResult").map(String::as_str),
            Some("True")
        );
        assert_eq!(
            model.get_property("ContainsCondition").map(String::as_str),
            Some("passed")
        );
        assert_eq!(
            model.get_property("FileNameResult").map(String::as_str),
            Some("file.txt")
        );
        assert_eq!(model.get_items("RawPropertyResult").unwrap().len(), 2);
        assert_eq!(model.get_items("EscapedPropertyResult").unwrap().len(), 1);
        assert_eq!(model.get_items("FunctionResult").unwrap()[0].name, "a;b");

        let raw = model.get_items("RawResult").unwrap();
        assert_eq!(
            raw.iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(
            raw.iter()
                .all(|item| item.get_metadata("Custom").as_deref() == Some("raw"))
        );
        let authored = model.get_items("AuthoredResult").unwrap();
        assert_eq!(authored.len(), 1);
        assert_eq!(authored[0].name, "a;b");
        assert_eq!(
            authored[0].get_metadata("Custom").as_deref(),
            Some("authored")
        );

        let distinct = model.get_items("DistinctResult").unwrap();
        assert_eq!(distinct.len(), 1);
        assert_eq!(distinct[0].get_metadata("Custom").as_deref(), Some("first"));
        assert_eq!(
            distinct[0].get_metadata("Default").as_deref(),
            Some("source-default")
        );
        let transformed = model.get_items("TransformResult").unwrap();
        assert_eq!(
            transformed
                .iter()
                .map(|item| item.get_metadata("Custom").unwrap().into_owned())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(
            transformed
                .iter()
                .all(|item| item.get_metadata("Default").as_deref() == Some("source-default"))
        );
        Ok(())
    }

    #[test]
    fn imported_items_use_root_directory_and_literal_bracket_imports() -> Result<()> {
        let directory = TempDir::new()?;
        let import_directory = directory.path().join("sub");
        fs::create_dir(&import_directory)?;
        let imported_path = import_directory.join("import[1].props");
        fs::write(
            &imported_path,
            r#"<Project><ItemGroup><Imported Include="relative/file.cs" /></ItemGroup></Project>"#,
        )?;
        let project = write_project(
            &directory,
            "root.proj",
            r#"<Project><Import Project="sub/import[1].props" /></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let item = &evaluator.get_model().get_items("Imported").unwrap()[0];
        let expected_full_path = display_path(&directory.path().join("relative").join("file.cs"));
        let expected_defining_path = display_path(&imported_path);
        assert_eq!(
            item.get_metadata("FullPath").as_deref(),
            Some(expected_full_path.as_str())
        );
        assert_eq!(
            item.get_metadata("DefiningProjectFullPath").as_deref(),
            Some(expected_defining_path.as_str())
        );
        assert_eq!(
            item.get_metadata("DefiningProjectName").as_deref(),
            Some("import[1]")
        );
        Ok(())
    }

    #[test]
    fn reserved_metadata_is_rejected_case_insensitively() -> Result<()> {
        let directory = TempDir::new()?;
        for (name, contents, metadata) in [
            (
                "item.proj",
                r#"<Project><ItemGroup><I Include="x"><fullPATH>bad</fullPATH></I></ItemGroup></Project>"#,
                "fullPATH",
            ),
            (
                "definition.proj",
                r#"<Project><ItemDefinitionGroup><I><identity>bad</identity></I></ItemDefinitionGroup></Project>"#,
                "identity",
            ),
            (
                "inactive.proj",
                r#"<Project><ItemGroup Condition="false"><I Include="x"><Filename>bad</Filename></I></ItemGroup></Project>"#,
                "Filename",
            ),
        ] {
            let project = write_project(&directory, name, contents);
            let error = ProjectEvaluator::new()
                .load_project(project)
                .unwrap_err()
                .to_string();
            assert!(error.contains("MSB4033"), "{error}");
            assert!(error.contains(metadata), "{error}");
            assert!(error.contains("reserved item metadata"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn indexed_item_and_metadata_lookup_scales_without_default_clones() -> Result<()> {
        let directory = TempDir::new()?;
        let mut project = String::from(
            "<Project><ItemDefinitionGroup><Scale><Default>base</Default></Scale></ItemDefinitionGroup><ItemGroup>",
        );
        for index in 0..1_000 {
            write!(
                project,
                "<Scale Include=\"file{index}.txt\"><M{index}>value{index}</M{index}></Scale>"
            )?;
        }
        project.push_str("</ItemGroup></Project>");
        let project = write_project(&directory, "scale.proj", &project);
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let items = evaluator.get_model().get_items("sCaLe").unwrap();
        assert_eq!(items.len(), 1_000);
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item.get_metadata("DEFAULT").as_deref(), Some("base"));
            assert_eq!(
                item.get_metadata(&format!("m{index}")).as_deref(),
                Some(format!("value{index}").as_str())
            );
        }
        Ok(())
    }
}
