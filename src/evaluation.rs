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
    environment_entries: Vec<(String, String)>,
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
    dotnet_root_x64: Option<String>,
    dotnet_root_x86: Option<String>,
    dotnet_root_arm64: Option<String>,
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
        let mut environment_entries = Vec::new();
        for (name, value) in environment {
            let name = name.into();
            let value = value.into();
            environment_map.insert(name.clone(), value.clone());
            environment_entries.push((name, value));
        }
        let mut global_map = PropertyMap::new();
        for (name, value) in global_properties {
            global_map.insert(name.into(), value.into());
        }
        let sdk_root_override = explicit_sdk_root(&environment_map, &global_map);
        Self {
            environment: environment_map,
            environment_entries,
            global_properties: global_map,
            sdk_root_override,
        }
    }

    pub(crate) fn environment(&self) -> &PropertyMap {
        &self.environment
    }

    pub(crate) fn environment_entries(&self) -> &[(String, String)] {
        &self.environment_entries
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
        let initial_targets = self.model()?.initial_targets().to_vec();
        for initial_target in initial_targets {
            self.execute_target_recursive(&initial_target, &mut executed_targets)?;
        }
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
        global_json_contents: global_json_contents.clone(),
        dotnet_host_path: environment.get("DOTNET_HOST_PATH").cloned(),
        dotnet_root: environment.get("DOTNET_ROOT").cloned(),
        dotnet_root_x64: environment.get("DOTNET_ROOT_X64").cloned(),
        dotnet_root_x86: environment.get("DOTNET_ROOT_X86").cloned(),
        dotnet_root_arm64: environment.get("DOTNET_ROOT_ARM64").cloned(),
        path: environment.get("PATH").cloned(),
    };
    let cache = HOST_RESOLUTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(resolution) = cache.lock().ok()?.get(&key).cloned() {
        return resolution;
    }

    // An exact installed pin needs no host roll-forward decision, so avoid the
    // comparatively expensive `dotnet --info` subprocess on this common path.
    let resolution = global_json_contents
        .as_deref()
        .and_then(|contents| resolve_exact_pinned_toolset(environment, contents))
        .or_else(|| invoke_dotnet_host(environment, project_directory));
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

fn resolve_exact_pinned_toolset(
    environment: &PropertyMap,
    global_json_contents: &str,
) -> Option<ActiveToolset> {
    let global_json: serde_json::Value = serde_json::from_str(global_json_contents).ok()?;
    let sdk = global_json.get("sdk")?;
    let version = sdk.get("version")?.as_str()?;
    if !sdk
        .get("rollForward")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("disable"))
        || version.is_empty()
        || version == "."
        || version == ".."
        || version.contains(['/', '\\'])
    {
        return None;
    }

    let dotnet_root = find_dotnet_root(environment, version)?;
    active_toolset_from_sdk_directory(dotnet_root.join("sdk").join(version))
}

fn find_dotnet_root(environment: &PropertyMap, sdk_version: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for name in [
        "DOTNET_ROOT",
        "DOTNET_ROOT_X64",
        "DOTNET_ROOT_X86",
        "DOTNET_ROOT_ARM64",
    ] {
        if let Some(root) = environment.get(name) {
            candidates.push(PathBuf::from(root));
        }
    }

    let executable = environment
        .get("DOTNET_HOST_PATH")
        .map(String::as_str)
        .unwrap_or("dotnet");
    let executable_path = PathBuf::from(executable);
    if executable_path.is_absolute() || executable_path.components().count() > 1 {
        if let Some(parent) = executable_path
            .canonicalize()
            .unwrap_or(executable_path)
            .parent()
        {
            candidates.push(parent.to_path_buf());
        }
    } else if let Some(path) = environment.get("PATH") {
        for directory in env::split_paths(path) {
            let candidate = directory.join(executable);
            let candidate = if candidate.is_file() {
                Some(candidate)
            } else if cfg!(windows) && candidate.extension().is_none() {
                let with_extension = candidate.with_extension("exe");
                with_extension.is_file().then_some(with_extension)
            } else {
                None
            };
            if let Some(candidate) = candidate
                && let Some(parent) = candidate.canonicalize().unwrap_or(candidate).parent()
            {
                candidates.push(parent.to_path_buf());
                break;
            }
        }
    }

    candidates
        .into_iter()
        .find(|root| root.join("sdk").join(sdk_version).is_dir())
}

fn active_toolset_from_sdk_directory(tools_path: PathBuf) -> Option<ActiveToolset> {
    let sdk_root = tools_path.join("Sdks");
    if !sdk_root.is_dir() {
        return None;
    }

    let bundled_information =
        fs::read_to_string(tools_path.join("Microsoft.NETCoreSdk.BundledMSBuildInformation.props"))
            .ok()?;
    let msbuild_version =
        xml_element_text(&bundled_information, "BundledMSBuildVersion")?.to_string();
    let commit = fs::read_to_string(tools_path.join(".version"))
        .ok()
        .and_then(|contents| {
            let value = contents.lines().next()?.trim();
            (!value.is_empty() && value.chars().all(|character| character.is_ascii_hexdigit()))
                .then(|| value.chars().take(9).collect::<String>())
        });
    let msbuild_semantic_version = commit.map_or_else(
        || msbuild_version.clone(),
        |commit| format!("{msbuild_version}+{commit}"),
    );
    Some(ActiveToolset {
        sdk_root,
        tools_path,
        msbuild_version,
        msbuild_semantic_version,
    })
}

fn xml_element_text<'a>(xml: &'a str, element: &str) -> Option<&'a str> {
    let opening = format!("<{element}>");
    let closing = format!("</{element}>");
    let start = xml.find(&opening)? + opening.len();
    let end = xml[start..].find(&closing)? + start;
    let value = xml[start..end].trim();
    (!value.is_empty()).then_some(value)
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
    fn exact_pinned_toolset_avoids_dotnet_info_resolution() -> Result<()> {
        let directory = TempDir::new()?;
        let tools_path = directory.path().join("sdk").join("10.0.302");
        fs::create_dir_all(tools_path.join("Sdks"))?;
        fs::write(
            tools_path.join("Microsoft.NETCoreSdk.BundledMSBuildInformation.props"),
            "<Project><PropertyGroup><BundledMSBuildVersion>18.6.11</BundledMSBuildVersion></PropertyGroup></Project>",
        )?;
        fs::write(
            tools_path.join(".version"),
            "35b593bebfcba58f8e78298cef14c2761f5d86c6\n10.0.302\n",
        )?;
        let mut environment = PropertyMap::new();
        environment.insert(
            "DOTNET_ROOT".to_string(),
            directory.path().to_string_lossy().into_owned(),
        );

        let toolset = resolve_exact_pinned_toolset(
            &environment,
            r#"{"sdk":{"version":"10.0.302","rollForward":"disable"}}"#,
        )
        .expect("the exact installed SDK should resolve without invoking dotnet");
        assert_eq!(toolset.tools_path, tools_path);
        assert_eq!(toolset.sdk_root, tools_path.join("Sdks"));
        assert_eq!(toolset.msbuild_version, "18.6.11");
        assert_eq!(toolset.msbuild_semantic_version, "18.6.11+35b593beb");
        assert!(
            resolve_exact_pinned_toolset(
                &environment,
                r#"{"sdk":{"version":"10.0.302","rollForward":"latestFeature"}}"#
            )
            .is_none()
        );
        Ok(())
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
    fn upstream_treat_as_local_property_import_affects_only_subsequent_assignments() -> Result<()> {
        // Ports Evaluator_Tests.VerifyTreatAsLocalPropertyInImportDoesntAffectParentProjectAboveIt
        // and VerifyTreatAsLocalPropertyInImportAffectsParentProjectBelowIt.
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "local.props",
            r#"<Project TreatAsLocalProperty="Foo" />"#,
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup>
    <Foo>before-import</Foo>
    <SeenAbove>$(Foo)</SeenAbove>
  </PropertyGroup>
  <Import Project="local.props" />
  <PropertyGroup>
    <Foo>$(Foo)-below-import</Foo>
    <SeenBelow>$(Foo)</SeenBelow>
  </PropertyGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::with_global_properties([("fOO", "global")]);
        evaluator.load_project(project)?;
        let model = evaluator.get_model();

        assert_eq!(
            model.get_property("SeenAbove").map(String::as_str),
            Some("global")
        );
        assert_eq!(
            model.get_property("SeenBelow").map(String::as_str),
            Some("global-below-import")
        );
        Ok(())
    }

    #[test]
    fn upstream_treat_as_local_property_expands_lists_and_unions_imports() -> Result<()> {
        // Ports VerifyTreatAsLocalPropertySpecificationWorksIfSpecificationIsItselfAProperty,
        // VerifyTreatAsLocalPropertyUnionBetweenImports, and VerifyDuplicateTreatAsLocalProperty.
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "local.props",
            r#"<Project TreatAsLocalProperty="BAR; baz ;fOo">
  <PropertyGroup>
    <Foo>$(Foo)-import</Foo>
    <Bar>$(Bar)-import</Bar>
    <Baz>$(Baz)-import</Baz>
  </PropertyGroup>
</Project>"#,
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project TreatAsLocalProperty="$(LocalNames); foo ;;;">
  <PropertyGroup>
    <Foo>$(Foo)-root</Foo>
    <Bar>ignored-before-import</Bar>
    <Untouched>ignored</Untouched>
  </PropertyGroup>
  <Import Project="local.props" />
</Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            Vec::<(String, String)>::new(),
            [
                ("LocalNames", "Foo"),
                ("Foo", "global-foo"),
                ("Bar", "global-bar"),
                ("Baz", "global-baz"),
                ("Untouched", "global-untouched"),
            ],
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project(project)?;
        let model = evaluator.get_model();

        assert_eq!(
            model.get_property("Foo").map(String::as_str),
            Some("global-foo-root-import")
        );
        assert_eq!(
            model.get_property("Bar").map(String::as_str),
            Some("global-bar-import")
        );
        assert_eq!(
            model.get_property("Baz").map(String::as_str),
            Some("global-baz-import")
        );
        assert_eq!(
            model.get_property("Untouched").map(String::as_str),
            Some("global-untouched")
        );
        Ok(())
    }

    #[test]
    fn upstream_invalid_treat_as_local_property_is_rejected() -> Result<()> {
        // Port of Evaluator_Tests.VerifyInvalidTreatAsLocalProperty.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project TreatAsLocalProperty="||Bar;Foo" />"#,
        );
        let error = ProjectEvaluator::with_global_properties([("Foo", "global")])
            .load_project(project)
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB5016"), "{error}");
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
    fn feature_wave_and_environment_intrinsics_use_the_evaluation_snapshot() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "features.proj",
            r#"<Project><PropertyGroup>
  <Before>$([MSBuild]::AreFeaturesEnabled('18.5'))</Before>
  <At>$([MSBuild]::AreFeaturesEnabled('18.6'))</At>
  <Expanded>$([System.Environment]::ExpandEnvironmentVariables('%MSBUILD_RS_SNAPSHOT%'))</Expanded>
  <CaseProbe>$([System.Environment]::ExpandEnvironmentVariables('%msbuild_rs_snapshot%'))</CaseProbe>
</PropertyGroup></Project>"#,
        );
        let context = EvaluationContext::with_environment_and_global_properties(
            [
                ("MSBUILDDISABLEFEATURESFROMVERSION", "18.6"),
                ("MSBUILD_RS_SNAPSHOT", "snapshot-value"),
            ],
            Vec::<(String, String)>::new(),
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project(&project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model
                .get_property("MSBuildDisableFeaturesFromVersion")
                .map(String::as_str),
            Some("18.6")
        );
        assert_eq!(
            model.get_property("Before").map(String::as_str),
            Some("True")
        );
        assert_eq!(model.get_property("At").map(String::as_str), Some("False"));
        assert_eq!(
            model.get_property("Expanded").map(String::as_str),
            Some("snapshot-value")
        );
        assert_eq!(
            model.get_property("CaseProbe").map(String::as_str),
            Some(if cfg!(windows) {
                "snapshot-value"
            } else {
                "%msbuild_rs_snapshot%"
            })
        );

        let context = EvaluationContext::with_environment_and_global_properties(
            [("MSBUILDDISABLEFEATURESFROMVERSION", "garbage")],
            Vec::<(String, String)>::new(),
        );
        let mut evaluator = ProjectEvaluator::with_context(context);
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("MSBuildDisableFeaturesFromVersion")
                .map(String::as_str),
            Some("999.999")
        );
        Ok(())
    }

    #[test]
    fn installed_sdk_style_fixture_completes_workload_aware_evaluation() -> Result<()> {
        let project = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/evaluation/sdk-style-progress/project.csproj");
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model.get_property("TargetFramework").map(String::as_str),
            Some("net10.0")
        );
        assert_eq!(
            model
                .get_property("TargetFrameworkIdentifier")
                .map(String::as_str),
            Some(".NETCoreApp")
        );
        assert!(
            model
                .get_items("KnownFrameworkReference")
                .is_some_and(|items| items
                    .iter()
                    .any(|item| item.name == "Microsoft.NETCore.App"))
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
    fn semicolon_separated_import_paths_ignore_empty_entries_and_preserve_order() -> Result<()> {
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "first.props",
            "<Project><PropertyGroup><Order>$(Order)first</Order></PropertyGroup></Project>",
        );
        write_project(
            &directory,
            "second.props",
            "<Project><PropertyGroup><Order>$(Order),second</Order></PropertyGroup></Project>",
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><Import Project=";first.props;;second.props;" /></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_property("Order")
                .map(String::as_str),
            Some("first,second")
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
        assert!(model.get_items("ActualGlob").is_none());
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
        let full_directory = directory
            .path()
            .join("firstdirectory")
            .join("seconddirectory");
        let root = full_directory.ancestors().last().unwrap();
        let expected_directory = format!(
            "{}{}",
            display_path(full_directory.strip_prefix(root)?),
            std::path::MAIN_SEPARATOR
        );
        assert_eq!(
            model.get_items("DirectoryResult").unwrap()[0].name,
            expected_directory
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
    fn upstream_include_and_exclude_observe_intermediary_state() -> Result<()> {
        // Exact project-data ports of ItemEvaluation_Tests.
        // IncludeShouldPreserveIntermediaryReferences and ExcludeSeesIntermediaryState.
        let directory = TempDir::new()?;
        let include_project = write_project(
            &directory,
            "include.proj",
            r#"<Project><ItemGroup>
  <i2 Include="a;b;c"><m1>m1_contents</m1><m2>m2_contents</m2></i2>
  <i Include="@(i2)" />
  <i2 Include="d;e;f;@(i2)"><m1>m1_updated</m1><m2>m2_updated</m2></i2>
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(include_project)?;
        let model = evaluator.get_model();
        let copied = model.get_items("i").unwrap();
        assert_eq!(
            copied
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(copied.iter().all(|item| item.get_metadata("m1").as_deref()
            == Some("m1_contents")
            && item.get_metadata("m2").as_deref() == Some("m2_contents")));
        let source = model.get_items("i2").unwrap();
        assert_eq!(
            source
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "d", "e", "f", "a", "b", "c"]
        );
        assert!(
            source[..3]
                .iter()
                .all(|item| item.get_metadata("m1").as_deref() == Some("m1_contents"))
        );
        assert!(
            source[3..]
                .iter()
                .all(|item| item.get_metadata("m1").as_deref() == Some("m1_updated"))
        );

        let exclude_project = write_project(
            &directory,
            "exclude.proj",
            r#"<Project><ItemGroup>
  <a Include="1" />
  <i Include="1;2" Exclude="@(a)" />
  <a Include="2" />
  <a Condition="'@(a)' == '1;2'" Include="3" />
</ItemGroup></Project>"#,
        );
        evaluator.load_project(exclude_project)?;
        assert_eq!(
            evaluator
                .get_model()
                .get_items("i")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
        Ok(())
    }

    #[test]
    fn upstream_remove_and_update_preserve_intermediary_references() -> Result<()> {
        // Exact project-data ports of ItemEvaluation_Tests.
        // RemoveShouldPreserveIntermediaryReferences and
        // UpdateShouldPreserveIntermediaryReferences.
        let directory = TempDir::new()?;
        for (name, remove) in [("literal.proj", "a;b;c"), ("glob.proj", "*")] {
            let project = write_project(
                &directory,
                name,
                &format!(
                    r#"<Project><ItemGroup>
  <i2 Include="a;b;c"><m1>m1_contents</m1><m2>m2_contents</m2></i2>
  <i Include="@(i2)" />
  <i2 Remove="{remove}" />
</ItemGroup></Project>"#
                ),
            );
            let mut evaluator = ProjectEvaluator::new();
            evaluator.load_project(project)?;
            let model = evaluator.get_model();
            assert!(model.get_items("i2").unwrap().is_empty());
            let copied = model.get_items("i").unwrap();
            assert_eq!(copied.len(), 3);
            assert!(copied.iter().all(|item| item.get_metadata("m1").as_deref()
                == Some("m1_contents")
                && item.get_metadata("m2").as_deref() == Some("m2_contents")));
        }

        let update_project = write_project(
            &directory,
            "update.proj",
            r#"<Project><ItemGroup>
  <i2 Include="a;b;c"><m1>m1_contents</m1><m2>%(Identity)</m2></i2>
  <i Include="@(i2)">
    <m3>@(i2 -> '%(m2)')</m3>
    <m4 Condition="'@(i2 -> &apos;%(m2)&apos;)' == 'a;b;c'">m4_contents</m4>
  </i>
  <i2 Update="a;b;c">
    <m1>m1_updated</m1><m2>m2_updated</m2>
    <m3>m3_updated</m3><m4>m4_updated</m4>
  </i2>
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(update_project)?;
        let model = evaluator.get_model();
        let copied = model.get_items("i").unwrap();
        for (index, identity) in ["a", "b", "c"].iter().enumerate() {
            assert_eq!(
                copied[index].get_metadata("m1").as_deref(),
                Some("m1_contents")
            );
            assert_eq!(copied[index].get_metadata("m2").as_deref(), Some(*identity));
            assert_eq!(copied[index].get_metadata("m3").as_deref(), Some("a;b;c"));
            assert_eq!(
                copied[index].get_metadata("m4").as_deref(),
                Some("m4_contents")
            );
        }
        assert!(model.get_items("i2").unwrap().iter().all(|item| {
            ["m1", "m2", "m3", "m4"].iter().all(|name| {
                item.get_metadata(name).as_deref() == Some(format!("{name}_updated").as_str())
            })
        }));
        Ok(())
    }

    #[test]
    fn upstream_remove_and_update_respect_item_transforms() -> Result<()> {
        // Exact project-data ports of ItemEvaluation_Tests.RemoveRespectsItemTransform
        // and UpdateRespectsItemTransform.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <i Include="a;b;c" />
  <i Remove="@(i->WithMetadataValue('Identity', 'b'))" />
  <i Remove="@(i->'%(Extension)')" />
  <i Update="@(i->WithMetadataValue('Identity', 'c'))"><m1>m1_updated</m1></i>
  <i Update="@(i->'%(Extension)')"><m2>m2_updated</m2></i>
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let items = evaluator.get_model().get_items("i").unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert!(items[0].get_metadata("m1").is_none());
        assert_eq!(items[1].get_metadata("m1").as_deref(), Some("m1_updated"));
        assert!(items.iter().all(|item| item.get_metadata("m2").is_none()));
        Ok(())
    }

    #[test]
    fn upstream_multiple_inter_item_dependencies_on_same_operation() -> Result<()> {
        // Exact project-data port of ItemEvaluation_Tests.
        // MultipleInterItemDependenciesOnSameItemOperation.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <i1 Include="i1_1;i1_2;i1_3;i1_4;i1_5" />
  <i1 Update="*"><m>i1</m></i1>
  <i1 Remove="*i1_5" />
  <i_cond Condition="@(i1->Count()) == 4" Include="i1 has 4 items" />
  <i2 Include="@(i1);i2_4" />
  <i2 Remove="i?_4" />
  <i2 Update="i?_1"><m>i2</m></i2>
  <i3 Include="@(i1);i3_3" />
  <i3 Remove="*i?_3" />
  <i1 Remove="*i1_2" />
  <i1 Include="i1_6" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model
                .get_items("i1")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["i1_1", "i1_3", "i1_4", "i1_6"]
        );
        assert_eq!(
            model
                .get_items("i2")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["i1_1", "i1_2", "i1_3"]
        );
        assert_eq!(
            model
                .get_items("i3")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["i1_1", "i1_2", "i1_4"]
        );
        assert_eq!(
            model.get_items("i2").unwrap()[0]
                .get_metadata("m")
                .as_deref(),
            Some("i2")
        );
        assert_eq!(model.get_items("i_cond").unwrap()[0].name, "i1 has 4 items");
        Ok(())
    }

    #[test]
    fn upstream_item_expression_grammar_and_function_chaining() -> Result<()> {
        // Ports Expander_Tests.ItemIncludeContainsMultipleItemReferences,
        // ItemFunctionChainingWithWhitespaceBeforeArrow,
        // ExpandItemVectorFunctionsChained1, BuiltIn1, and BuiltIn3.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <ItemGroup>
    <CFiles Include="foo.c;bar.c" />
    <ObjFiles Include="@(CFiles->'%(filename).obj')" />
    <CleanFiles Include="@(ObjFiles);@(Missing)" />
    <I Include="A"><M>F</M><Path>first/second/file.ext</Path></I>
    <I Include="B"><M>T</M><Path>first/second/file.ext</Path></I>
    <I Include="C"><M>T</M><Path>first/second/file.ext</Path></I>
    <BuiltIn1 Include="foo;bar" />
    <BuiltIn3 Include="foo;bar;foo;bar;foo" />
    <WhitespaceChain Include="@(I -> WithMetadataValue('M', 'T') -> WithMetadataValue('M', 'T'))" />
    <ChainedTransform Include="@(I->'%(Path)'->'%(Directory)'->Distinct())" />
    <FullPathBuiltIn Include="@(BuiltIn1->FullPath())" />
    <FullPathDistinct Include="@(BuiltIn3->FullPath()->Distinct())" />
    <LiteralTemplate Include="@(I->'out/%(Identity).txt')" />
  </ItemGroup>
  <PropertyGroup>
    <Joined>@(I, '|')</Joined>
    <TransformJoined>@(I->'%(Identity).obj', ' + ')</TransformJoined>
  </PropertyGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model
                .get_items("CleanFiles")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["foo.obj", "bar.obj"]
        );
        assert_eq!(
            model
                .get_items("WhitespaceChain")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["B", "C"]
        );
        assert_eq!(model.get_items("ChainedTransform").unwrap().len(), 1);
        assert!(
            model.get_items("ChainedTransform").unwrap()[0]
                .name
                .ends_with(&format!(
                    "first{}second{}",
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR
                ))
        );
        assert_eq!(model.get_items("FullPathBuiltIn").unwrap().len(), 2);
        assert!(
            model
                .get_items("FullPathBuiltIn")
                .unwrap()
                .iter()
                .all(|item| Path::new(&item.name).is_absolute())
        );
        assert_eq!(model.get_items("FullPathDistinct").unwrap().len(), 2);
        assert!(
            model
                .get_items("FullPathDistinct")
                .unwrap()
                .iter()
                .all(|item| Path::new(&item.name).is_absolute())
        );
        assert_eq!(
            model
                .get_items("LiteralTemplate")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["out/A.txt", "out/B.txt", "out/C.txt"]
        );
        assert_eq!(
            model.get_property("Joined").map(String::as_str),
            Some("A|B|C")
        );
        assert_eq!(
            model.get_property("TransformJoined").map(String::as_str),
            Some("A.obj + B.obj + C.obj")
        );
        Ok(())
    }

    #[test]
    fn upstream_common_item_functions_and_empty_results() -> Result<()> {
        // Ports Expander_Tests Count/empty Count, AnyHaveMetadataValue,
        // HasMetadata, WithoutMetadataValue, Metadata, ClearMetadata, Exists,
        // DirectoryName, Combine, Reverse, and item-spec modifier coverage.
        let directory = TempDir::new()?;
        fs::create_dir(directory.path().join("alpha"))?;
        fs::write(directory.path().join("alpha").join("one.cs"), "")?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <ItemDefinitionGroup><Cleared><DestinationDefault>D</DestinationDefault></Cleared></ItemDefinitionGroup>
  <ItemGroup>
    <I Include="One"><A>true</A><Path>alpha/one.cs</Path></I>
    <I Include="Two"><A>false</A><Path>alpha/missing.cs</Path></I>
    <I Include="Three"><A></A></I>
    <I Include="Four"><B></B></I>
    <Case Include="A;a;B;A" />
    <CountResult Include="@(I->Count());@(Missing->Count());@(I->Metadata('Missing')->Count())" />
    <AnyResult Include="@(I->AnyHaveMetadataValue('A','TRUE'));@(Missing->AnyHaveMetadataValue('A','x'))" />
    <HasResult Include="@(I->HasMetadata('a'))" />
    <WithResult Include="@(I->WithMetadataValue('A','true'))" />
    <WithoutResult Include="@(I->WithoutMetadataValue('A','true'))" />
    <MetadataResult Include="@(I->Metadata('Path'))" />
    <DistinctResult Include="@(Case->Distinct())" />
    <DistinctCaseResult Include="@(Case->DistinctWithCase())" />
    <ReverseResult Include="@(I->Reverse())" />
    <Cleared Include="@(I->ClearMetadata())" />
    <Potential Include="alpha/one.cs;alpha/missing.cs" />
    <ExistsResult Include="@(Potential->Exists())" />
    <DirectoryNameResult Include="@(Potential->DirectoryName()->Distinct())" />
    <CombineResult Include="@(I->Combine('.squiggle'))" />
    <FullPathResult Include="@(Potential->FullPath())" />
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model
                .get_items("CountResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["4", "0", "0"]
        );
        assert_eq!(
            model
                .get_items("AnyResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["true", "false"]
        );
        assert_eq!(
            model
                .get_items("HasResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["One", "Two"]
        );
        assert_eq!(model.get_items("WithResult").unwrap()[0].name, "One");
        assert_eq!(
            model
                .get_items("WithoutResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Two", "Three", "Four"]
        );
        assert_eq!(
            model
                .get_items("MetadataResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha/one.cs", "alpha/missing.cs"]
        );
        assert_eq!(
            model
                .get_items("DistinctResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(
            model
                .get_items("DistinctCaseResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "a", "B"]
        );
        assert_eq!(
            model
                .get_items("ReverseResult")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Four", "Three", "Two", "One"]
        );
        assert!(model.get_items("Cleared").unwrap().iter().all(|item| {
            item.get_metadata("A").is_none()
                && item.get_metadata("DestinationDefault").as_deref() == Some("D")
        }));
        assert_eq!(model.get_items("ExistsResult").unwrap().len(), 1);
        assert_eq!(
            model.get_items("ExistsResult").unwrap()[0].name,
            "alpha/one.cs"
        );
        assert_eq!(model.get_items("DirectoryNameResult").unwrap().len(), 1);
        assert_eq!(
            model.get_items("DirectoryNameResult").unwrap()[0].name,
            display_path(&directory.path().join("alpha"))
        );
        assert!(
            model
                .get_items("CombineResult")
                .unwrap()
                .iter()
                .all(|item| item.get_metadata("A").is_none())
        );
        assert!(
            model
                .get_items("FullPathResult")
                .unwrap()
                .iter()
                .all(|item| Path::new(&item.name).is_absolute())
        );
        Ok(())
    }

    #[test]
    fn upstream_get_paths_of_all_directories_above_returns_unique_canonical_paths() -> Result<()> {
        // Ports Expander_Tests.ExpandItemVectorFunctions_GetPathsOfAllDirectoriesAbove
        // and ExpandItemVectorFunctions_GetPathsOfAllDirectoriesAbove_ReturnCanonicalPaths.
        let directory = TempDir::new()?;
        let alpha = directory.path().join("alpha");
        let beta = alpha.join("beta");
        let gamma = directory.path().join("gamma");
        fs::create_dir_all(&beta)?;
        fs::create_dir_all(&gamma)?;
        let project = alpha.join("project.proj");
        fs::write(
            &project,
            r#"<Project>
  <ItemGroup>
    <Compile Include="One.cs;beta/Two.cs;beta/Three.cs;../gamma/Four.cs" />
    <MyDirectories Include="@(Compile->GetPathsOfAllDirectoriesAbove())" />
  </ItemGroup>
</Project>"#,
        )?;

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let directories = evaluator
            .get_model()
            .get_items("MyDirectories")
            .unwrap()
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            directories
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            directories.len()
        );
        for expected in [
            directory.path(),
            alpha.as_path(),
            beta.as_path(),
            gamma.as_path(),
        ] {
            let expected = display_path(expected);
            assert!(
                directories.iter().any(|actual| actual == &expected),
                "{expected:?} not found in {directories:?}"
            );
        }
        assert!(
            !directories
                .iter()
                .any(|actual| actual.ends_with("gamma/Four.cs")
                    || actual.ends_with("gamma\\Four.cs"))
        );
        Ok(())
    }

    #[test]
    fn upstream_and_sdk_per_item_string_functions_use_deterministic_allowlist() -> Result<()> {
        // Ports Expander_Tests.ExpandItemVectorFunctionsItemSpecModifier2's Substring
        // case and the Trim/TrimStart/Replace forms used by the .NET 10 SDK.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <I Include="  Ab.c%3Bd  ;.NETCoreApp,Version=v8.0;abcdef" />
  <Trimmed Include="@(I->Trim())" />
  <TrimStartChars Include="@(I->TrimStart('.NETCoreApp,Version=v'))" />
  <Replaced Include="@(I->Replace('.', '_'))" />
  <Substring Include="@(I->Substring(2, 3))" />
  <Contains Include="@(I->Contains('b'))" />
  <Equals Include="@(I->Equals('abcdef'))" />
  <Length Include="@(I->get_Length())" />
  <Spaced Include="%20%20abc%20%20" />
  <TrimEmptySet Include="@(Spaced->Trim(''))" />
  <TrimCharsSource Include="xxabcxx" />
  <TrimChars Include="@(TrimCharsSource->Trim('x'))" />
  <TrimEndChars Include="@(TrimCharsSource->TrimEnd('x'))" />
</ItemGroup></Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let identities = |item_type: &str| {
            model
                .get_items(item_type)
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            identities("Trimmed"),
            ["Ab.c;d", ".NETCoreApp,Version=v8.0", "abcdef"]
        );
        assert_eq!(identities("TrimStartChars"), ["b.c;d", "8.0", "abcdef"]);
        assert_eq!(
            identities("Replaced"),
            ["Ab_c;d", "_NETCoreApp,Version=v8_0", "abcdef"]
        );
        assert_eq!(identities("Substring"), [".c;", "ETC", "cde"]);
        assert_eq!(identities("Contains"), ["True", "False", "True"]);
        assert_eq!(identities("Equals"), ["False", "False", "True"]);
        assert_eq!(identities("Length"), ["6", "24", "6"]);
        assert_eq!(identities("TrimEmptySet"), ["abc"]);
        assert_eq!(identities("TrimChars"), ["abc"]);
        assert_eq!(identities("TrimEndChars"), ["xxabc"]);
        Ok(())
    }

    #[test]
    fn ascii_casing_per_item_string_functions_match_dotnet_and_reject_non_ascii() -> Result<()> {
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <I Include="Ab-cD;xyZ;a%3bb" />
  <Upper Include="@(I->ToUpper())" />
  <Lower Include="@(I->ToLower())" />
  <UpperInvariant Include="@(I->ToUpperInvariant())" />
  <LowerInvariant Include="@(I->ToLowerInvariant())" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let identities = |item_type: &str| {
            model
                .get_items(item_type)
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(identities("Upper"), ["AB-CD", "XYZ", "A;B"]);
        assert_eq!(identities("Lower"), ["ab-cd", "xyz", "a;b"]);
        assert_eq!(identities("UpperInvariant"), ["AB-CD", "XYZ", "A;B"]);
        assert_eq!(identities("LowerInvariant"), ["ab-cd", "xyz", "a;b"]);

        for (member, input) in [("ToUpper", "i"), ("ToLower", "I")] {
            let project = write_project(
                &directory,
                "project.proj",
                &format!(
                    r#"<Project><ItemGroup>
  <I Include="{input}" />
  <Rejected Include="@(I->{member}())" />
</ItemGroup></Project>"#
                ),
            );
            let error = format!(
                "{:#}",
                ProjectEvaluator::new().load_project(project).unwrap_err()
            );
            assert!(error.contains("current-culture ASCII"), "{member}: {error}");
            assert!(
                error.contains("requires a CoreCLR fallback"),
                "{member}: {error}"
            );
        }

        for member in ["ToUpper", "ToLower", "ToUpperInvariant", "ToLowerInvariant"] {
            let project = write_project(
                &directory,
                "project.proj",
                &format!(
                    r#"<Project><ItemGroup>
  <I Include="Straße" />
  <Rejected Include="@(I->{member}())" />
</ItemGroup></Project>"#
                ),
            );
            let error = ProjectEvaluator::new()
                .load_project(project)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("non-ASCII casing") && error.contains("CoreCLR fallback"),
                "{member}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn upstream_initial_targets_aggregate_for_preprocessing_and_execution() -> Result<()> {
        let directory = TempDir::new()?;
        write_project(
            &directory,
            "nested.props",
            r#"<Project InitialTargets="Nested;Shared;$(Inside)">
  <Target Name="Nested" />
</Project>"#,
        );
        write_project(
            &directory,
            "first.props",
            r#"<Project InitialTargets="First;Shared;$(Prefix);$(Inside)">
  <PropertyGroup>
    <Inside>Inside</Inside>
    <FromFirst>yes</FromFirst>
  </PropertyGroup>
  <Import Project="nested.props" />
  <Target Name="First" />
</Project>"#,
        );
        write_project(
            &directory,
            "skipped.props",
            r#"<Project InitialTargets="Skipped"><Target Name="Skipped" /></Project>"#,
        );
        write_project(
            &directory,
            "second.props",
            r#"<Project InitialTargets="Second;Shared;$(Prefix);$(FromFirst)">
  <Target Name="Second" />
</Project>"#,
        );
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project DefaultTargets="Main" InitialTargets=" Root;Shared;$(AtRoot) " ToolsVersion="Current" TreatAsLocalProperty="Local">
  <PropertyGroup>
    <Prefix>Parent</Prefix>
    <EnableSecond>true</EnableSecond>
  </PropertyGroup>
  <Import Project="first.props" />
  <PropertyGroup><Prefix>Between</Prefix></PropertyGroup>
  <Import Project="skipped.props" Condition="false" />
  <Import Project="second.props" Condition="'$(EnableSecond)' == 'true'" />
  <Import Project="first.props" />
  <Target Name="Root"><Error Text="initial target ran" /></Target>
  <Target Name="Main" />
</Project>"#,
        );
        let output_path = directory.path().join("preprocessed.xml");
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project_and_write_preprocessed(&project, &output_path)?;

        // Ports Preprocessor_Tests.InitialTargetsOuterAndInner and direct
        // depth-first/conditional/duplicate/property-expansion probes.
        assert_eq!(
            evaluator.get_model().initial_targets(),
            [
                "Root", "Shared", "First", "Shared", "Parent", "Nested", "Shared", "Inside",
                "Second", "Shared", "Between", "yes"
            ]
        );

        let output = fs::read_to_string(output_path)?;
        assert!(output.contains(
            r#"<Project DefaultTargets="Main" InitialTargets="Root;Shared;$(AtRoot);First;Shared;$(Prefix);$(Inside);Nested;Shared;$(Inside);Second;Shared;$(Prefix);$(FromFirst)" ToolsVersion="Current" TreatAsLocalProperty="Local">"#
        ));
        assert!(!output.contains("InitialTargets=\"Skipped"));
        assert_eq!(output.matches("InitialTargets=").count(), 1);

        let error = evaluator.execute_target("Main").unwrap_err().to_string();
        assert!(error.contains("initial target ran"), "{error}");
        Ok(())
    }

    #[test]
    fn timestamp_metadata_covers_literal_missing_directory_glob_and_transform_items() -> Result<()>
    {
        let directory = TempDir::new()?;
        fs::write(directory.path().join("literal.txt"), "literal")?;
        fs::write(directory.path().join("glob.txt"), "glob")?;
        fs::create_dir(directory.path().join("folder"))?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup><Root>$(MSBuildThisFileDirectory)</Root></PropertyGroup>
  <ItemGroup>
    <Literal Include="$(Root)literal.txt;$(Root)missing.txt;$(Root)folder" />
    <Globbed Include="$(Root)*.txt" />
    <ModifiedProjection Include="@(Literal->ModifiedTime())" />
  </ItemGroup>
</Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let literal = model.get_items("Literal").unwrap();
        for name in ["ModifiedTime", "CreatedTime", "AccessedTime"] {
            let value = literal[0].get_metadata(name).unwrap();
            assert_eq!(value.len(), 27, "{name}={value}");
            assert_eq!(literal[1].get_metadata(name).as_deref(), Some(""));
            assert_eq!(literal[2].get_metadata(name).as_deref(), Some(""));
        }

        let globbed = model.get_items("Globbed").unwrap();
        assert_eq!(globbed.len(), 2);
        assert!(globbed.iter().all(|item| {
            item.get_metadata("ModifiedTime")
                .is_some_and(|value| value.len() == 27)
        }));
        assert_eq!(
            model.get_items("ModifiedProjection").unwrap()[0].name,
            literal[0].get_metadata("ModifiedTime").unwrap()
        );
        Ok(())
    }

    #[test]
    fn reviewed_item_pipeline_semantics_match_direct_msbuild() -> Result<()> {
        // Direct dotnet-msbuild repros for transform correlation, scalar chaining,
        // intrinsic escaping, custom separators, provenance, and ordinal distinctness.
        let directory = TempDir::new()?;
        fs::create_dir_all(directory.path().join("tree").join("sub"))?;
        fs::write(directory.path().join("tree").join("root.txt"), "")?;
        fs::write(
            directory.path().join("tree").join("sub").join("nested.txt"),
            "",
        )?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <PropertyGroup><Args>'M','x'</Args></PropertyGroup>
  <ItemDefinitionGroup>
    <I><SourceDefault>source-default</SourceDefault><Overlap>source</Overlap></I>
    <Copy><DestinationDefault>destination-default</DestinationDefault><Overlap>destination</Overlap></Copy>
    <Same><DestinationDefault>destination-default</DestinationDefault><Overlap>destination</Overlap></Same>
    <Changed><DestinationDefault>destination-default</DestinationDefault><Overlap>destination</Overlap></Changed>
    <Cleared><DestinationDefault>destination-default</DestinationDefault></Cleared>
  </ItemDefinitionGroup>
  <ItemGroup>
    <ArgText Include="M" />
    <I Include="a;b"><M>x</M><Custom>source</Custom></I>
    <EmptyTransformCount Include="@(I->'%(Missing)'->Count())" />
    <EmptyTransformMaterialized Include="@(I->'%(Missing)')" />
    <CountCombine Include="@(I->Count()->Combine('x'))" />
    <AnyCount Include="@(I->AnyHaveMetadataValue('M','x')->Count())" />
    <PropertyArguments Include="@(I->WithMetadataValue($(Args)))" />
    <NestedVectorArgument Include="@(I->HasMetadata(@(ArgText))->Count())" />
    <EscapedCombine Include="@(I->Combine('%3B'))" />
    <TransformMetadataIdentity Include="@(I->'%(Identity).changed'->Metadata('Identity'))" />
    <TransformIdentityFunction Include="@(I->'%(Identity).changed'->Identity())" />
    <ClearIdentityCount Include="@(I->ClearMetadata()->Metadata('Identity')->Count())" />
    <CombineIdentityCount Include="@(I->Combine('x')->Metadata('Identity')->Count())" />
    <SemiSeparator Include="@(I, ';')" />
    <PipeSeparator Include="@(I, '|')" />
    <AnySource Include="@(I->AnyHaveMetadataValue('M','x'))" />
    <AnySeparated Include="@(I->AnyHaveMetadataValue('M','x'), '|')" />
    <CountSeparated Include="@(I->Count(), '|')" />
    <Escaped Include="%41;A;K;K;σ;ς" />
    <DistinctEscaped Include="@(Escaped->Distinct())" />
    <DistinctCaseEscaped Include="@(Escaped->DistinctWithCase())" />
    <Copy Include="@(I)" />
    <Same Include="@(I->'%(Identity)')" />
    <Changed Include="@(I->'%(Identity).changed')" />
    <Cleared Include="@(I->ClearMetadata())" />
    <Glob Include="tree/**/*.txt"><Custom>glob</Custom></Glob>
    <GlobCopy Include="@(Glob)" />
    <GlobSame Include="@(Glob->'%(Identity)')" />
    <GlobChanged Include="@(Glob->'%(Identity).changed')" />
  </ItemGroup>
</Project>"#,
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let identities = |item_type: &str| {
            model
                .get_items(item_type)
                .into_iter()
                .flatten()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(identities("EmptyTransformCount"), ["2"]);
        assert!(
            model
                .get_items("EmptyTransformMaterialized")
                .is_none_or(Vec::is_empty)
        );
        assert_eq!(
            identities("CountCombine"),
            [format!("2{}x", std::path::MAIN_SEPARATOR)]
        );
        assert_eq!(identities("AnyCount"), ["1"]);
        assert_eq!(identities("PropertyArguments"), ["a", "b"]);
        assert_eq!(identities("NestedVectorArgument"), ["0"]);
        assert_eq!(
            identities("EscapedCombine"),
            [
                format!("a{}%3B", std::path::MAIN_SEPARATOR),
                format!("b{}%3B", std::path::MAIN_SEPARATOR),
            ]
        );
        assert_eq!(identities("TransformMetadataIdentity"), ["a", "b"]);
        assert_eq!(
            identities("TransformIdentityFunction"),
            ["a.changed", "b.changed"]
        );
        assert_eq!(identities("ClearIdentityCount"), ["0"]);
        assert_eq!(identities("CombineIdentityCount"), ["0"]);
        assert_eq!(identities("SemiSeparator"), ["a;b"]);
        assert_eq!(identities("PipeSeparator"), ["a|b"]);
        assert_eq!(identities("CountSeparated"), ["2"]);
        assert_eq!(identities("DistinctEscaped"), ["A", "A", "K", "K", "σ"]);
        assert_eq!(
            identities("DistinctCaseEscaped"),
            ["A", "A", "K", "K", "σ", "ς"]
        );

        let any_source = &model.get_items("AnySource").unwrap()[0];
        assert_eq!(any_source.get_metadata("M").as_deref(), Some("x"));
        for item_type in [
            "AnySeparated",
            "CountSeparated",
            "SemiSeparator",
            "PipeSeparator",
        ] {
            assert!(
                model.get_items(item_type).unwrap()[0]
                    .get_metadata("M")
                    .is_none()
            );
        }

        for item_type in ["Copy", "Same", "Changed"] {
            assert!(model.get_items(item_type).unwrap().iter().all(|item| {
                item.get_metadata("M").as_deref() == Some("x")
                    && item.get_metadata("SourceDefault").as_deref() == Some("source-default")
                    && item.get_metadata("DestinationDefault").as_deref()
                        == Some("destination-default")
                    && item.get_metadata("Overlap").as_deref() == Some("source")
            }));
        }
        assert!(model.get_items("Cleared").unwrap().iter().all(|item| {
            item.get_metadata("M").is_none()
                && item.get_metadata("SourceDefault").is_none()
                && item.get_metadata("DestinationDefault").as_deref() == Some("destination-default")
        }));

        for item_type in ["GlobCopy", "GlobSame"] {
            assert_eq!(
                model.get_items(item_type).unwrap()[1]
                    .get_metadata("RecursiveDir")
                    .as_deref(),
                Some(format!("sub{}", std::path::MAIN_SEPARATOR).as_str())
            );
        }
        assert!(model.get_items("GlobChanged").unwrap().iter().all(|item| {
            item.get_metadata("RecursiveDir").as_deref() == Some("")
                && item.get_metadata("Custom").as_deref() == Some("glob")
        }));
        Ok(())
    }

    #[test]
    fn item_operation_conditions_reject_metadata_but_child_conditions_retain_context() -> Result<()>
    {
        let directory = TempDir::new()?;
        for (name, operation) in [
            (
                "include",
                r#"<I Include="x" Condition="'%(Identity)' == 'x'" />"#,
            ),
            (
                "exclude",
                r#"<I Include="x" Exclude="y" Condition="'%(Identity)' == 'x'" />"#,
            ),
            (
                "remove",
                r#"<I Include="x" /><I Remove="x" Condition="'%(Identity)' == 'x'" />"#,
            ),
            (
                "update",
                r#"<I Include="x" /><I Update="x" Condition="'%(Identity)' == 'x'"><M>v</M></I>"#,
            ),
        ] {
            let project = write_project(
                &directory,
                &format!("{name}.proj"),
                &format!("<Project><ItemGroup>{operation}</ItemGroup></Project>"),
            );
            let error = ProjectEvaluator::new().load_project(project).unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("MSB4190"), "{name}: {error}");
            assert!(
                error.contains(
                    r#"built-in metadata "Identity" at position 1 is not allowed in this condition"#
                ),
                "{name}: {error}"
            );
        }

        let custom = write_project(
            &directory,
            "custom.proj",
            r#"<Project><ItemGroup><I Include="x" Condition="'%(M)' == 'x'" /></ItemGroup></Project>"#,
        );
        let error = ProjectEvaluator::new().load_project(custom).unwrap_err();
        assert!(format!("{error:#}").contains(
            r#"MSB4191: The reference to custom metadata "M" at position 1 is not allowed"#
        ));

        let child = write_project(
            &directory,
            "child.proj",
            r#"<Project><ItemGroup><I Include="a;b"><M Condition="%(Identity) == a">yes</M><N Condition="%(M) == yes">custom</N></I></ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(child)?;
        let items = evaluator.get_model().get_items("I").unwrap();
        assert_eq!(items[0].get_metadata("M").as_deref(), Some("yes"));
        assert_eq!(items[0].get_metadata("N").as_deref(), Some("custom"));
        assert!(items[1].get_metadata("M").is_none());
        assert!(items[1].get_metadata("N").is_none());
        Ok(())
    }

    #[test]
    fn source_less_item_pipeline_values_require_compatible_stages() -> Result<()> {
        let directory = TempDir::new()?;
        for (name, stage) in [
            ("identity", "Identity()"),
            ("full-path", "FullPath()"),
            ("metadata-filter", "HasMetadata('M')"),
            ("exists", "Exists()"),
            ("metadata-transform", "'%(Identity)'"),
        ] {
            let project = write_project(
                &directory,
                &format!("{name}.proj"),
                &format!(
                    r#"<Project><ItemGroup><I Include="a;b"><M>x</M></I><R Include="@(I->Count()->{stage})" /></ItemGroup></Project>"#
                ),
            );
            let error = ProjectEvaluator::new().load_project(project).unwrap_err();
            let error = format!("{error:#}");
            assert!(
                error.contains("requires source-item context")
                    && error.contains("source-less value"),
                "{name}: {error}"
            );
        }

        let compatible = write_project(
            &directory,
            "compatible.proj",
            r#"<Project><ItemGroup>
  <I Include="a;b" />
  <Combined Include="@(I->Count()->Combine('x'))" />
  <Distinct Include="@(I->Count()->Distinct())" />
  <Constant Include="@(I->Count()->'constant')" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(compatible)?;
        let model = evaluator.get_model();
        assert_eq!(
            model.get_items("Combined").unwrap()[0].name,
            format!("2{}x", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(model.get_items("Distinct").unwrap()[0].name, "2");
        assert_eq!(model.get_items("Constant").unwrap()[0].name, "constant");
        Ok(())
    }

    #[test]
    fn upstream_different_excludes_and_recursive_glob_metadata() -> Result<()> {
        // Exact project-data port of ItemEvaluation_Tests.
        // DifferentExcludesOnSameWildcardProduceDifferentResults, extended with
        // recursive metadata, ?, escaped wildcards, and literal brackets.
        let directory = TempDir::new()?;
        for name in ["a.cs", "b.cs", "c.cs"] {
            fs::write(directory.path().join(name), "")?;
        }
        fs::create_dir_all(directory.path().join("tree").join("sub").join("deep"))?;
        fs::write(directory.path().join("tree").join("root.txt"), "")?;
        fs::write(directory.path().join("tree").join("sub").join("a.txt"), "")?;
        fs::write(
            directory
                .path()
                .join("tree")
                .join("sub")
                .join("deep")
                .join("b.txt"),
            "",
        )?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <i Include="**/*.cs" />
  <i Include="**/*.cs" Exclude="*a.cs" />
  <i Include="**/*.cs" Exclude="a.cs;c.cs" />
  <Recursive Include="tree/**/*.txt" Exclude="tree/sub/a.txt" />
  <Question Include="tree/sub/?.txt" />
  <EscapedWildcard Include="tree/%2A.txt" />
  <Bracket Include="tree/literal[1].txt" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(
            model
                .get_items("i")
                .unwrap()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["a.cs", "b.cs", "c.cs", "b.cs", "c.cs", "b.cs"]
        );
        let recursive = model.get_items("Recursive").unwrap();
        assert_eq!(
            recursive
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            [
                format!("tree{}root.txt", std::path::MAIN_SEPARATOR),
                format!(
                    "tree{}sub{}deep{}b.txt",
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR
                )
            ]
        );
        assert_eq!(
            recursive[0].get_metadata("RecursiveDir").as_deref(),
            Some("")
        );
        assert_eq!(
            recursive[1].get_metadata("RecursiveDir").as_deref(),
            Some(
                format!(
                    "sub{}deep{}",
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR
                )
                .as_str()
            )
        );
        assert_eq!(
            model.get_items("Question").unwrap()[0].name,
            format!(
                "tree{}sub{}a.txt",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            )
        );
        assert_eq!(
            model.get_items("EscapedWildcard").unwrap()[0].name,
            "tree/*.txt"
        );
        assert_eq!(
            model.get_items("Bracket").unwrap()[0].name,
            "tree/literal[1].txt"
        );
        Ok(())
    }

    #[test]
    fn reviewed_item_operation_snapshots_and_msbuild_wildcards_match_direct_repros() -> Result<()> {
        // Direct dotnet-msbuild repros, plus exact behavioral coverage from
        // FileMatcher_Tests.GetFilesComplexGlobbingMatching,
        // RegressItemRecursionWorksAsExpected, IllegalPaths, and SplitFileSpec.
        let directory = TempDir::new()?;
        fs::create_dir_all(directory.path().join("tree").join("sub").join("deep"))?;
        fs::create_dir_all(
            directory
                .path()
                .join("tree")
                .join("node_modules")
                .join("pkg"),
        )?;
        for path in [
            "Dockerfile",
            "Dockerfile.txt",
            "tree/root.txt",
            "tree/sub/a.txt",
            "tree/sub/deep/b.txt",
            "tree/node_modules/pkg/dependency.txt",
        ] {
            fs::write(directory.path().join(path), "")?;
        }
        let absolute_root = display_path(&directory.path().join("tree").join("root.txt"));
        let project = write_project(
            &directory,
            "project.proj",
            &format!(
                r#"<Project><ItemGroup>
  <Snapshot Include="a;b"><Seen>@(Snapshot)</Seen><OnlyA Condition="'%(Identity)' == 'a'">yes</OnlyA></Snapshot>
  <ConditionUpdate Include="a;b"><State>old-%(Identity)</State></ConditionUpdate>
  <SameUpdate Include="a;b"><State>old-%(Identity)</State></SameUpdate>
  <Duplicate Include="x;x;y" />
  <ConditionUpdate Update="a;b" Condition="'@(ConditionUpdate->'%(State)')' == 'old-a;old-b'"><State>new-%(Identity)</State></ConditionUpdate>
  <SameUpdate Update="a;b"><Seen>@(SameUpdate->'%(State)')</Seen></SameUpdate>
  <Duplicate Update="x"><Updated>yes</Updated></Duplicate>
  <DuplicateSnapshot Include="@(Duplicate)" />
  <Duplicate Remove="x" />
  <Terminal Include="tree/**" />
  <TerminalStar Include="tree/*/" />
  <TerminalRecursive Include="tree/**/" />
  <StarDot Include="*.*" />
  <PrefixedStarDot Include="D*.*" />
  <Ordinary Include="tree/s*/*.txt" />
  <LogicalDot Include="./tree/*.txt" />
  <LogicalDotDot Include="tree/sub/../*.txt" />
  <LogicalExcludePhysical Include="tree/sub/../*.txt" Exclude="tree/*.txt" />
  <LogicalExcludeSame Include="tree/sub/../*.txt" Exclude="tree/sub/../*.txt" />
  <LogicalRemovePhysical Include="tree/sub/../*.txt" />
  <LogicalRemovePhysical Remove="tree/*.txt" />
  <Mixed Include="%2A-*.txt" />
  <Illegal Include="tree/**.txt" />
  <LiteralMutation Include="%2A-*.txt;tree/**.txt" />
  <LiteralMutation Update="tree/**.txt"><Updated>yes</Updated></LiteralMutation>
  <LiteralMutation Remove="%2A-*.txt" />
  <Bracket Include="literal[1].txt" />
  <Escaped Include="tree/%2A.txt" />
  <Cone Include="../outside.md;tree/root.txt" />
  <Cone Remove="**/*.md" />
  <Absolute Include="{absolute_root}" />
  <Absolute Remove="tree/root.txt" />
  <ExcludeA Include="tree/**/*.txt" Exclude="tree/sub/**" />
  <ExcludeB Include="tree/**/*.txt" Exclude="tree/node_modules/**" />
  <FalseInvalid Condition="false" Include="$([Unsupported.Type]::Method())/**/*.txt" />
</ItemGroup></Project>"#
            ),
        );

        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        let items = |item_type: &str| model.get_items(item_type).map(Vec::as_slice).unwrap_or(&[]);

        assert_eq!(
            items("Snapshot")
                .iter()
                .map(|item| item.get_metadata("Seen").unwrap())
                .collect::<Vec<_>>(),
            ["", ""]
        );
        assert_eq!(
            items("Snapshot")
                .iter()
                .map(|item| item.get_metadata("OnlyA").unwrap_or_default())
                .collect::<Vec<_>>(),
            ["yes", ""]
        );
        assert!(items("ConditionUpdate").iter().all(|item| {
            item.get_metadata("State").as_deref() == Some(format!("new-{}", item.name).as_str())
        }));
        assert!(
            items("SameUpdate")
                .iter()
                .all(|item| { item.get_metadata("Seen").as_deref() == Some("old-a;old-b") })
        );
        assert_eq!(
            items("DuplicateSnapshot")
                .iter()
                .map(|item| (
                    item.name.clone(),
                    item.get_metadata("Updated")
                        .unwrap_or_default()
                        .into_owned()
                ))
                .collect::<Vec<_>>(),
            [
                ("x".to_string(), "yes".to_string()),
                ("x".to_string(), "yes".to_string()),
                ("y".to_string(), String::new()),
            ]
        );
        assert_eq!(
            items("Duplicate")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["y"]
        );
        assert_eq!(items("Terminal").len(), 4);
        assert!(items("TerminalStar").is_empty());
        assert!(items("TerminalRecursive").is_empty());
        assert!(
            items("StarDot")
                .iter()
                .any(|item| item.name == "Dockerfile")
        );
        assert_eq!(
            items("PrefixedStarDot")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Dockerfile.txt"]
        );
        assert_eq!(
            items("Ordinary")[0].get_metadata("RecursiveDir").as_deref(),
            Some(format!("sub{}", std::path::MAIN_SEPARATOR).as_str())
        );
        assert_eq!(
            items("LogicalDot")[0].name,
            format!(
                ".{}tree{}root.txt",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            )
        );
        let logical_dot_dot = format!(
            "tree{}sub{}..{}root.txt",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        );
        assert_eq!(items("LogicalDotDot")[0].name, logical_dot_dot);
        assert_eq!(items("LogicalExcludePhysical")[0].name, logical_dot_dot);
        assert!(items("LogicalExcludeSame").is_empty());
        assert!(items("LogicalRemovePhysical").is_empty());
        assert_eq!(items("Mixed")[0].name, "*-*.txt");
        assert_eq!(items("Illegal")[0].name, "tree/**.txt");
        assert_eq!(items("LiteralMutation")[0].name, "tree/**.txt");
        assert_eq!(
            items("LiteralMutation")[0]
                .get_metadata("Updated")
                .as_deref(),
            Some("yes")
        );
        assert_eq!(items("Bracket")[0].name, "literal[1].txt");
        assert_eq!(items("Escaped")[0].name, "tree/*.txt");
        assert_eq!(
            items("Cone")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["../outside.md", "tree/root.txt"]
        );
        assert!(items("Absolute").is_empty());
        assert_eq!(
            items("ExcludeA")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            [
                format!(
                    "tree{}node_modules{}pkg{}dependency.txt",
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR,
                    std::path::MAIN_SEPARATOR
                ),
                format!("tree{}root.txt", std::path::MAIN_SEPARATOR),
            ]
        );
        assert_eq!(items("ExcludeB").len(), 3);
        assert!(items("FalseInvalid").is_empty());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_and_root_relative_item_specs_match_lexically() -> Result<()> {
        // Direct dotnet-msbuild comparison of both selector orientations.
        let directory = TempDir::new()?;
        let drive = directory
            .path()
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy();
        let root_relative = r"\__msbuild_rs_root_probe__\x.txt";
        let root_absolute = format!("{drive}{root_relative}");
        let current_drive = std::env::current_dir()?
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .into_owned();
        let drive_relative = format!("{current_drive}__msbuild_rs_drive_probe__\\x.txt");
        let drive_absolute = display_path(&std::path::absolute(&drive_relative)?);
        fs::write(directory.path().join("root-relative.txt"), "")?;
        let directory_spelling = display_path(directory.path());
        let root_relative_directory = directory_spelling
            .strip_prefix(drive.as_ref())
            .expect("temporary directory should be on its reported drive");
        let root_relative_glob = format!(r"{root_relative_directory}\*.txt");
        let root_relative_identity = format!(r"{root_relative_directory}\root-relative.txt");
        let absolute_glob_file = display_path(&directory.path().join("root-relative.txt"));
        let project = write_project(
            &directory,
            "paths.proj",
            &format!(
                r#"<Project><ItemGroup>
  <Root Include="{root_relative}" /><Root Remove="{root_absolute}" />
  <ReverseRoot Include="{root_absolute}" /><ReverseRoot Remove="{root_relative}" />
  <Drive Include="{drive_relative}" /><Drive Remove="{drive_absolute}" />
  <ReverseDrive Include="{drive_absolute}" /><ReverseDrive Remove="{drive_relative}" />
  <RootGlob Include="{root_relative_glob}" />
  <RootGlobExcludeAbsolute Include="{root_relative_glob}" Exclude="{absolute_glob_file}" />
  <RootGlobRemoveAbsolute Include="{root_relative_glob}" />
  <RootGlobRemoveAbsolute Remove="{absolute_glob_file}" />
</ItemGroup></Project>"#
            ),
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        for item_type in ["Root", "ReverseRoot", "Drive", "ReverseDrive"] {
            assert!(
                evaluator
                    .get_model()
                    .get_items(item_type)
                    .is_none_or(Vec::is_empty)
            );
        }
        assert_eq!(
            evaluator.get_model().get_items("RootGlob").unwrap()[0].name,
            root_relative_identity
        );
        assert_eq!(
            evaluator
                .get_model()
                .get_items("RootGlobExcludeAbsolute")
                .unwrap()[0]
                .name,
            root_relative_identity
        );
        assert!(
            evaluator
                .get_model()
                .get_items("RootGlobRemoveAbsolute")
                .is_none_or(Vec::is_empty)
        );
        Ok(())
    }

    #[test]
    fn upstream_item_transform_containing_semicolon() -> Result<()> {
        // Exact evaluation-time port of
        // EscapingInProjects_Tests.ItemTransformContainingSemicolon.
        let directory = TempDir::new()?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project>
  <ItemGroup>
    <TextFile Include="X.txt;Y.txt;Z.txt" />
    <Result Include="@(TextFile->'%(FileName);%(FileName)%253b%(FileName)%(Extension)','    ')" />
  </ItemGroup>
</Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(
            evaluator.get_model().get_items("Result").unwrap()[0].name,
            "X;X%3bX.txt    Y;Y%3bY.txt    Z;Z%3bZ.txt"
        );
        Ok(())
    }

    #[test]
    fn upstream_long_include_chain_handles_ten_thousand_items_iteratively() -> Result<()> {
        // Exact scale of ItemEvaluation_Tests.LongIncludeChain.
        let directory = TempDir::new()?;
        let mut content = String::from("<Project><ItemGroup>");
        for index in 0..10_000 {
            write!(content, "<i Include=\"ItemValue{index}\" />")?;
        }
        content.push_str("</ItemGroup></Project>");
        let project = write_project(&directory, "long.proj", &content);
        let started = std::time::Instant::now();
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        assert_eq!(evaluator.get_model().get_items("i").unwrap().len(), 10_000);
        eprintln!("10,000 separate includes: {:?}", started.elapsed());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "10,000 includes took {:?}",
            started.elapsed()
        );
        Ok(())
    }

    #[test]
    fn benchmark_scale_remove_and_update_use_indexed_exact_matching() -> Result<()> {
        let directory = TempDir::new()?;
        let mut all = String::new();
        let mut even = String::new();
        let mut odd = String::new();
        for index in 0..10_000 {
            if index != 0 {
                all.push(';');
            }
            write!(all, "i{index}")?;
            let selected = if index % 2 == 0 { &mut even } else { &mut odd };
            if !selected.is_empty() {
                selected.push(';');
            }
            write!(selected, "i{index}")?;
        }
        let content = format!(
            r#"<Project><ItemGroup>
  <Scale Include="{all}" />
  <Scale Update="{even}"><Updated>true</Updated></Scale>
  <Scale Remove="{odd}" />
</ItemGroup></Project>"#
        );
        let project = write_project(&directory, "scale.proj", &content);
        let started = std::time::Instant::now();
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let elapsed = started.elapsed();
        let items = evaluator.get_model().get_items("Scale").unwrap();
        assert_eq!(items.len(), 5_000);
        assert!(
            items
                .iter()
                .all(|item| item.get_metadata("Updated").as_deref() == Some("true"))
        );
        eprintln!("10,000-item bulk update/remove: {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "10,000-item bulk update/remove took {elapsed:?}",
        );
        Ok(())
    }

    #[test]
    fn benchmark_ten_thousand_separate_exact_mutations_use_stable_identity_index() -> Result<()> {
        let directory = TempDir::new()?;
        let mut content = String::from("<Project><ItemGroup><Scale Include=\"");
        for index in 0..10_000 {
            if index != 0 {
                content.push(';');
            }
            write!(content, "i{index}")?;
        }
        content.push_str("\" />");
        for index in 0..10_000 {
            write!(
                content,
                "<Scale Update=\"i{index}\"><Updated>{index}</Updated></Scale>"
            )?;
        }
        content.push_str("<ScaleSnapshot Include=\"@(Scale)\" />");
        for index in 0..10_000 {
            write!(content, "<Scale Remove=\"i{index}\" />")?;
        }
        content.push_str("</ItemGroup></Project>");
        let project = write_project(&directory, "scale.proj", &content);
        let started = std::time::Instant::now();
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let elapsed = started.elapsed();
        let model = evaluator.get_model();
        assert!(model.get_items("Scale").unwrap().is_empty());
        let snapshot = model.get_items("ScaleSnapshot").unwrap();
        assert_eq!(snapshot.len(), 10_000);
        for (index, item) in snapshot.iter().enumerate() {
            assert_eq!(
                item.get_metadata("Updated").as_deref(),
                Some(index.to_string().as_str())
            );
        }
        eprintln!("10,000 separate updates + removes: {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "10,000 separate updates and removes took {elapsed:?}",
        );
        Ok(())
    }

    #[test]
    fn benchmark_ten_thousand_remove_include_churn_compacts_identity_buckets() -> Result<()> {
        let directory = TempDir::new()?;
        let mut content = String::from("<Project><ItemGroup><Scale Include=\"x\" />");
        for _ in 0..10_000 {
            content.push_str("<Scale Remove=\"x\" /><Scale Include=\"x\" />");
        }
        content.push_str("</ItemGroup></Project>");
        let project = write_project(&directory, "churn.proj", &content);
        let started = std::time::Instant::now();
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let elapsed = started.elapsed();
        let model = evaluator.get_model();
        assert_eq!(model.get_items("Scale").unwrap().len(), 1);
        let (peak_len, peak_capacity) = model.identity_index_peak_bucket();
        eprintln!(
            "10,000 remove/include churn: {elapsed:?}; identity bucket peak len/capacity: {peak_len}/{peak_capacity}"
        );
        assert!(
            peak_len <= 2,
            "inactive identity slots accumulated: {peak_len}"
        );
        assert!(
            peak_capacity <= 8,
            "identity bucket allocation grew unexpectedly: {peak_capacity}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "10,000 remove/include cycles took {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn benchmark_repeated_update_deduplicates_item_definition_layers() -> Result<()> {
        let directory = TempDir::new()?;
        let mut content = String::from(
            "<Project><ItemDefinitionGroup><Scale><Base>base</Base></Scale></ItemDefinitionGroup><ItemGroup><Scale Include=\"x\" /></ItemGroup><ItemDefinitionGroup><Scale><Later>later</Later></Scale></ItemDefinitionGroup><ItemGroup>",
        );
        for _ in 0..10_000 {
            content.push_str("<Scale Update=\"x\" />");
        }
        content.push_str("</ItemGroup></Project>");
        let project = write_project(&directory, "updates.proj", &content);
        let started = std::time::Instant::now();
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let evaluation_elapsed = started.elapsed();
        let item = &evaluator.get_model().get_items("Scale").unwrap()[0];
        assert_eq!(item.get_metadata("Base").as_deref(), Some("base"));
        assert_eq!(item.get_metadata("Later").as_deref(), Some("later"));
        assert_eq!(item.inherited_default_layer_count(), 1);

        let lookup_started = std::time::Instant::now();
        for _ in 0..10_000 {
            assert!(item.get_metadata("Missing").is_none());
        }
        let lookup_elapsed = lookup_started.elapsed();
        eprintln!(
            "10,000 repeated updates: {evaluation_elapsed:?}; 10,000 missing metadata lookups: {lookup_elapsed:?}; inherited layers: {}",
            item.inherited_default_layer_count()
        );
        assert!(
            evaluation_elapsed < std::time::Duration::from_secs(60),
            "10,000 repeated updates took {evaluation_elapsed:?}"
        );
        assert!(
            lookup_elapsed < std::time::Duration::from_secs(5),
            "10,000 missing metadata lookups took {lookup_elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn upstream_lazy_wildcard_case_is_tracked_as_an_eager_deviation() -> Result<()> {
        // ItemEvaluation_Tests.LazyWildcardExpansionDoesNotEvaluateWildCardsIfNotReferenced
        // is intentionally tracked rather than claimed as a parity port: this
        // evaluator eagerly expands every project-evaluation item wildcard.
        let directory = TempDir::new()?;
        fs::create_dir_all(directory.path().join("foo"))?;
        fs::write(directory.path().join("foo").join("a.cs"), "")?;
        fs::write(directory.path().join("foo").join("b.cs"), "")?;
        let project = write_project(
            &directory,
            "project.proj",
            r#"<Project><ItemGroup>
  <i Include="**/foo/**/*.cs" />
  <ItemReference Include="@(i)" />
  <RecursiveDir Include="@(i->'%(RecursiveDir)')" />
</ItemGroup></Project>"#,
        );
        let mut evaluator = ProjectEvaluator::new();
        evaluator.load_project(project)?;
        let model = evaluator.get_model();
        assert_eq!(model.get_items("i").unwrap().len(), 2);
        assert_eq!(model.get_items("ItemReference").unwrap().len(), 2);
        assert_eq!(model.get_items("RecursiveDir").unwrap().len(), 2);
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
