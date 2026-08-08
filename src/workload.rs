use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::evaluation::ActiveToolset;
use crate::object_model::PropertyMap;

const AUTO_IMPORT_PROPS_LOCATOR: &str = "Microsoft.NET.SDK.WorkloadAutoImportPropsLocator";
const MANIFEST_TARGETS_LOCATOR: &str = "Microsoft.NET.SDK.WorkloadManifestTargetsLocator";

#[derive(Debug)]
pub(crate) struct WorkloadResolverConfig {
    dotnet_root: PathBuf,
    tools_path: PathBuf,
    feature_band: String,
    project_path: PathBuf,
    environment: PropertyMap,
}

#[derive(Debug, Default)]
pub(crate) struct InstalledWorkloadSdkResolver {
    resolutions: HashMap<String, Vec<PathBuf>>,
}

#[derive(Debug, Clone)]
struct ManifestRecord {
    id: String,
    directory: PathBuf,
}

#[derive(Debug)]
struct InstallState {
    workload_version: Option<String>,
    manifests: Option<serde_json::Map<String, serde_json::Value>>,
    use_workload_sets: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GlobalWorkloadConfig {
    workload_version: Option<String>,
    global_json_specified_workload_sets: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkloadSetSelectionSource {
    GlobalJson,
    InstallState,
    Automatic,
}

struct WorkloadSetSelection {
    feature_band: String,
    manifests: serde_json::Map<String, serde_json::Value>,
    source: WorkloadSetSelectionSource,
}

#[derive(Debug)]
struct SdkPack {
    id: String,
    version: String,
    aliases: Option<Vec<(String, String)>>,
}

impl WorkloadResolverConfig {
    pub(crate) fn new(
        environment: &PropertyMap,
        toolset: &ActiveToolset,
        project_path: &Path,
    ) -> Option<Self> {
        let sdk_version = toolset
            .tools_path
            .file_name()
            .and_then(|name| name.to_str())?
            .to_string();
        let dotnet_root = toolset.tools_path.parent()?.parent()?.to_path_buf();
        Some(Self {
            dotnet_root,
            tools_path: toolset.tools_path.clone(),
            feature_band: sdk_feature_band(&sdk_version)?,
            project_path: project_path.to_path_buf(),
            environment: environment.clone(),
        })
    }
}

impl InstalledWorkloadSdkResolver {
    pub(crate) fn discover(config: &WorkloadResolverConfig) -> Result<Self> {
        if config
            .environment
            .get("MSBuildEnableWorkloadResolver")
            .is_some_and(|value| value.eq_ignore_ascii_case("false"))
            || config
                .tools_path
                .join("DisableWorkloadResolver.sentinel")
                .is_file()
        {
            return Ok(Self::default());
        }

        let manifest_roots = manifest_roots(config);
        let known_manifest_ids =
            read_nonempty_lines(&config.tools_path.join("KnownWorkloadManifests.txt"))
                .or_else(|| {
                    read_nonempty_lines(&config.tools_path.join("IncludedWorkloadManifests.txt"))
                })
                .unwrap_or_default();
        let mut manifests = discover_loose_manifests(config, &manifest_roots, &known_manifest_ids);

        let global_config = nearest_global_json(&config.project_path)
            .map(|path| read_global_workload_config(&path))
            .transpose()?
            .unwrap_or_default();
        let install_state = read_install_state(config)?;
        let mut workload_set = if let Some(workload_version) =
            global_config.workload_version.as_deref()
        {
            Some(
                find_workload_set(
                    &manifest_roots,
                    &config.feature_band,
                    workload_version,
                    WorkloadSetSelectionSource::GlobalJson,
                )?
                .ok_or_else(|| {
                    anyhow!(
                        "Workload set '{workload_version}' was not found under the configured workload manifest roots"
                    )
                })?,
            )
        } else if let Some(workload_version) = install_state.workload_version.as_deref() {
            Some(
                find_workload_set(
                    &manifest_roots,
                    &config.feature_band,
                    workload_version,
                    WorkloadSetSelectionSource::InstallState,
                )?
                .ok_or_else(|| {
                    anyhow!(
                        "Workload set '{workload_version}' from install state was not found under the configured workload manifest roots"
                    )
                })?,
            )
        } else {
            None
        };
        if workload_set.is_none()
            && global_config
                .global_json_specified_workload_sets
                .unwrap_or(install_state.use_workload_sets)
            && let Some((_, selected)) = latest_workload_set(&manifest_roots, &config.feature_band)?
        {
            workload_set = Some(selected);
        }
        if let Some(workload_set) = &workload_set {
            overlay_manifest_specifiers(
                &mut manifests,
                &manifest_roots,
                &workload_set.feature_band,
                &workload_set.manifests,
            )?;
        }
        let install_state_manifests_may_overlay = workload_set
            .as_ref()
            .is_none_or(|selection| selection.source == WorkloadSetSelectionSource::InstallState);
        if install_state_manifests_may_overlay
            && let Some(install_manifests) = &install_state.manifests
        {
            overlay_manifest_specifiers(
                &mut manifests,
                &manifest_roots,
                &config.feature_band,
                install_manifests,
            )?;
        }

        let ordered_manifests = order_manifests(manifests, &known_manifest_ids);
        let pack_roots = pack_roots(config);
        let runtime_identifiers = read_nonempty_lines(
            &config
                .tools_path
                .join("NETCoreSdkRuntimeIdentifierChain.txt"),
        )
        .unwrap_or_default();
        let mut auto_import_paths = Vec::new();
        let mut pack_sdk_paths = HashMap::<String, Vec<PathBuf>>::new();
        for manifest in &ordered_manifests {
            let manifest_path = manifest.directory.join("WorkloadManifest.json");
            let document: serde_json::Value = parse_relaxed_json(
                &fs::read_to_string(&manifest_path)
                    .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
            )
            .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
            let mut packs = read_sdk_packs(&document, &manifest_path)?;
            packs.sort_by(|left, right| ascii_case_compare(&left.id, &right.id));
            for pack in packs {
                let Some(resolved_name) = resolve_pack_alias(&pack, &runtime_identifiers) else {
                    continue;
                };
                let Some(pack_directory) =
                    find_installed_pack(&pack_roots, resolved_name, &pack.version)
                else {
                    continue;
                };
                let sdk_directory = pack_directory.join("Sdk");
                if !sdk_directory.is_dir() {
                    continue;
                }
                pack_sdk_paths
                    .entry(ascii_key(&pack.id))
                    .or_default()
                    .push(sdk_directory.clone());
                if sdk_directory.join("AutoImport.props").is_file() {
                    auto_import_paths.push(sdk_directory);
                }
            }
        }
        deduplicate_paths(&mut auto_import_paths);

        let mut manifest_target_paths = ordered_manifests
            .iter()
            .filter(|manifest| {
                manifest
                    .directory
                    .join("WorkloadManifest.targets")
                    .is_file()
            })
            .map(|manifest| manifest.directory.clone())
            .collect::<Vec<_>>();
        deduplicate_paths(&mut manifest_target_paths);

        let mut resolutions = pack_sdk_paths;
        resolutions.insert(ascii_key(AUTO_IMPORT_PROPS_LOCATOR), auto_import_paths);
        resolutions.insert(ascii_key(MANIFEST_TARGETS_LOCATOR), manifest_target_paths);
        Ok(Self { resolutions })
    }

    pub(crate) fn resolve(&self, sdk_name: &str) -> Option<&[PathBuf]> {
        self.resolutions
            .get(&ascii_key(sdk_name))
            .map(Vec::as_slice)
    }
}

fn manifest_roots(config: &WorkloadResolverConfig) -> Vec<PathBuf> {
    let mut roots = config
        .environment
        .get("DOTNETSDK_WORKLOAD_MANIFEST_ROOTS")
        .map(|value| env::split_paths(value).collect::<Vec<_>>())
        .unwrap_or_default();
    if !config
        .environment
        .contains_key("DOTNETSDK_WORKLOAD_MANIFEST_IGNORE_DEFAULT_ROOTS")
    {
        if is_user_local(config)
            && let Some(profile) = dotnet_user_profile(&config.environment)
        {
            roots.push(profile.join("sdk-manifests"));
        }
        roots.push(config.dotnet_root.join("sdk-manifests"));
    }
    roots
}

fn pack_roots(config: &WorkloadResolverConfig) -> Vec<PathBuf> {
    let mut roots = config
        .environment
        .get("DOTNETSDK_WORKLOAD_PACK_ROOTS")
        .map(|value| env::split_paths(value).collect::<Vec<_>>())
        .unwrap_or_default();
    if is_user_local(config)
        && let Some(profile) = dotnet_user_profile(&config.environment)
    {
        roots.push(profile);
    }
    roots.push(config.dotnet_root.clone());
    roots
}

fn is_user_local(config: &WorkloadResolverConfig) -> bool {
    config
        .dotnet_root
        .join("metadata")
        .join("workloads")
        .join(&config.feature_band)
        .join("userlocal")
        .is_file()
}

fn dotnet_user_profile(environment: &PropertyMap) -> Option<PathBuf> {
    if let Some(home) = environment.get("DOTNET_CLI_HOME") {
        return Some(PathBuf::from(home).join(".dotnet"));
    }
    environment
        .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(|home| PathBuf::from(home).join(".dotnet"))
}

fn discover_loose_manifests(
    config: &WorkloadResolverConfig,
    roots: &[PathBuf],
    known_manifest_ids: &[String],
) -> HashMap<String, ManifestRecord> {
    let mut result = HashMap::new();
    for root in roots {
        let feature_directory = root.join(&config.feature_band);
        let Ok(entries) = fs::read_dir(feature_directory) else {
            continue;
        };
        let mut directories = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .collect::<Vec<_>>();
        directories.sort_by_key(|entry| ascii_key(&entry.file_name().to_string_lossy()));
        for entry in directories {
            let id = entry.file_name().to_string_lossy().into_owned();
            if is_ignored_loose_manifest(&id) {
                continue;
            }
            if let Some(directory) = resolve_manifest_directory(&entry.path()) {
                result
                    .entry(ascii_key(&id))
                    .or_insert(ManifestRecord { id, directory });
            }
        }
    }

    let Some(fallback_root) = roots.last() else {
        return result;
    };
    for id in known_manifest_ids {
        if result.contains_key(&ascii_key(id)) {
            continue;
        }
        if let Some(directory) = find_fallback_manifest(fallback_root, &config.feature_band, id) {
            result.insert(
                ascii_key(id),
                ManifestRecord {
                    id: id.clone(),
                    directory,
                },
            );
        }
    }
    result
}

fn is_ignored_loose_manifest(id: &str) -> bool {
    id.eq_ignore_ascii_case("workloadsets")
        || [
            "microsoft.net.workload.android",
            "microsoft.net.workload.blazorwebassembly",
            "microsoft.net.workload.ios",
            "microsoft.net.workload.maccatalyst",
            "microsoft.net.workload.macos",
            "microsoft.net.workload.tvos",
            "microsoft.net.workload.mono.toolchain",
        ]
        .iter()
        .any(|candidate| id.eq_ignore_ascii_case(candidate))
}

fn resolve_manifest_directory(path: &Path) -> Option<PathBuf> {
    let mut version_directories = fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|directory| directory.join("WorkloadManifest.json").is_file())
        .collect::<Vec<_>>();
    version_directories.sort_by(|left, right| {
        compare_versions(
            &left.file_name().unwrap_or_default().to_string_lossy(),
            &right.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    version_directories.pop().or_else(|| {
        path.join("WorkloadManifest.json")
            .is_file()
            .then(|| path.to_path_buf())
    })
}

fn find_fallback_manifest(root: &Path, current_band: &str, id: &str) -> Option<PathBuf> {
    let mut candidates = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let band = entry.file_name().to_string_lossy().into_owned();
            (feature_band_compare(&band, current_band).is_some_and(|order| !order.is_gt()))
                .then(|| {
                    resolve_manifest_directory(&entry.path().join(id))
                        .map(|directory| (band, directory))
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| compare_versions(&left.0, &right.0));
    candidates.pop().map(|(_, directory)| directory)
}

fn overlay_manifest_specifiers(
    manifests: &mut HashMap<String, ManifestRecord>,
    roots: &[PathBuf],
    default_band: &str,
    specifiers: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    for (id, value) in specifiers {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("Workload manifest specifier for '{id}' must be a string"))?;
        let (version, band) = value
            .split_once('/')
            .map_or((value, None), |(version, band)| (version, Some(band)));
        let band = band.unwrap_or(default_band);
        let directory = roots
            .iter()
            .find_map(|root| {
                let candidate = root.join(band).join(id).join(version);
                candidate
                    .join("WorkloadManifest.json")
                    .is_file()
                    .then_some(candidate)
            })
            .ok_or_else(|| {
                anyhow!(
                    "Workload manifest '{id}/{value}' was not found under the configured manifest roots"
                )
            })?;
        manifests.insert(
            ascii_key(id),
            ManifestRecord {
                id: id.clone(),
                directory,
            },
        );
    }
    Ok(())
}

fn find_workload_set(
    roots: &[PathBuf],
    current_feature_band: &str,
    version: &str,
    source: WorkloadSetSelectionSource,
) -> Result<Option<WorkloadSetSelection>> {
    let mut candidate_bands = vec![current_feature_band.to_string()];
    if let Some(feature_band) = sdk_feature_band(version)
        && !candidate_bands.contains(&feature_band)
    {
        candidate_bands.push(feature_band);
    }
    if !candidate_bands.iter().any(|band| band == "8.0.100") {
        candidate_bands.push("8.0.100".to_string());
    }
    for root in roots {
        for band in &candidate_bands {
            let directory = root.join(band).join("workloadsets").join(version);
            if directory.is_dir() {
                return Ok(Some(WorkloadSetSelection {
                    feature_band: band.clone(),
                    manifests: read_workload_set_directory(&directory)?,
                    source,
                }));
            }
        }
    }
    Ok(None)
}

fn latest_workload_set(
    roots: &[PathBuf],
    feature_band: &str,
) -> Result<Option<(String, WorkloadSetSelection)>> {
    let mut latest: Option<(String, PathBuf)> = None;
    for root in roots {
        let sets = root.join(feature_band).join("workloadsets");
        let Ok(entries) = fs::read_dir(sets) else {
            continue;
        };
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let version = entry.file_name().to_string_lossy().into_owned();
            if latest
                .as_ref()
                .is_none_or(|(current, _)| compare_versions(&version, current).is_gt())
            {
                latest = Some((version, entry.path()));
            }
        }
    }
    let Some((version, directory)) = latest else {
        return Ok(None);
    };
    Ok(Some((
        version,
        WorkloadSetSelection {
            feature_band: feature_band.to_string(),
            manifests: read_workload_set_directory(&directory)?,
            source: WorkloadSetSelectionSource::Automatic,
        },
    )))
}

fn read_workload_set_directory(
    directory: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut files = fs::read_dir(directory)
        .with_context(|| format!("Failed to read workload set {}", directory.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".workloadset.json"))
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut result = serde_json::Map::new();
    for file in files {
        let document: serde_json::Value = parse_relaxed_json(
            &fs::read_to_string(&file)
                .with_context(|| format!("Failed to read {}", file.display()))?,
        )
        .with_context(|| format!("Failed to parse {}", file.display()))?;
        let object = document
            .as_object()
            .ok_or_else(|| anyhow!("Workload set {} must contain a JSON object", file.display()))?;
        for (id, value) in object {
            if result.insert(id.clone(), value.clone()).is_some() {
                bail!(
                    "Workload set {} defines manifest '{id}' more than once",
                    directory.display()
                );
            }
        }
    }
    if result.is_empty() {
        bail!(
            "Workload set {} contains no *.workloadset.json files",
            directory.display()
        );
    }
    Ok(result)
}

fn read_global_workload_config(path: &Path) -> Result<GlobalWorkloadConfig> {
    let document: serde_json::Value = parse_relaxed_json(
        &fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", path.display()))?;
    let root = document
        .as_object()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let Some(sdk) = object_field_ignore_ascii_case(root, "sdk", "global.json root")? else {
        return Ok(GlobalWorkloadConfig::default());
    };
    let sdk = sdk
        .as_object()
        .ok_or_else(|| anyhow!("The global.json 'sdk' value must be an object"))?;
    let workload_version =
        object_field_ignore_ascii_case(sdk, "workloadVersion", "global.json sdk")?
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("global.json sdk.workloadVersion must be a string"))
            })
            .transpose()?;
    let update_mode =
        object_field_ignore_ascii_case(sdk, "workloads-update-mode", "global.json sdk")?
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| {
                        anyhow!("global.json sdk.workloads-update-mode must be a string")
                    })
                    .map(str::to_string)
            })
            .transpose()?;
    let global_json_specified_workload_sets = update_mode.as_deref().and_then(|mode| {
        if mode.eq_ignore_ascii_case("workload-set") {
            Some(true)
        } else if mode.eq_ignore_ascii_case("manifests") {
            Some(false)
        } else {
            None
        }
    });
    Ok(GlobalWorkloadConfig {
        workload_version,
        global_json_specified_workload_sets,
    })
}

fn read_install_state(config: &WorkloadResolverConfig) -> Result<InstallState> {
    let Some(path) = install_state_path(config) else {
        return Ok(InstallState {
            workload_version: None,
            manifests: None,
            use_workload_sets: true,
        });
    };
    let document = if path.is_file() {
        Some(
            parse_relaxed_json(
                &fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read {}", path.display()))?,
            )
            .with_context(|| format!("Failed to parse {}", path.display()))?,
        )
    } else {
        None
    };
    let object = document
        .as_ref()
        .map(|document| {
            document.as_object().ok_or_else(|| {
                anyhow!(
                    "Workload install state {} must be an object",
                    path.display()
                )
            })
        })
        .transpose()?;
    let workload_version = object
        .and_then(|object| object.get("workloadVersion"))
        .map(|value| {
            if value.is_null() {
                Ok(None)
            } else {
                value
                    .as_str()
                    .map(|value| (!value.is_empty()).then(|| value.to_string()))
                    .ok_or_else(|| {
                        anyhow!(
                            "Workload install state {} workloadVersion must be a string or null",
                            path.display()
                        )
                    })
            }
        })
        .transpose()?
        .flatten();
    let manifests = object
        .and_then(|object| object.get("manifests"))
        .map(|value| {
            if value.is_null() {
                return Ok(None);
            }
            let manifests = value.as_object().ok_or_else(|| {
                anyhow!(
                    "Workload install state {} manifests must be an object or null",
                    path.display()
                )
            })?;
            if let Some((id, _)) = manifests.iter().find(|(_, value)| !value.is_string()) {
                bail!(
                    "Workload install state {} manifest '{id}' must have a string specifier",
                    path.display()
                );
            }
            Ok(Some(manifests.clone()))
        })
        .transpose()?
        .flatten();
    let use_workload_sets = object
        .and_then(|object| object.get("useWorkloadSets"))
        .map(|value| {
            if value.is_null() {
                Ok(None)
            } else {
                value.as_bool().map(Some).ok_or_else(|| {
                    anyhow!(
                        "Workload install state {} useWorkloadSets must be a Boolean or null",
                        path.display()
                    )
                })
            }
        })
        .transpose()?
        .flatten()
        .unwrap_or(true);
    Ok(InstallState {
        workload_version,
        manifests,
        use_workload_sets,
    })
}

fn install_state_path(config: &WorkloadResolverConfig) -> Option<PathBuf> {
    let architecture = match env::consts::ARCH {
        "x86_64" => "X64",
        "x86" => "X86",
        "aarch64" => "Arm64",
        "arm" => "Arm",
        other => other,
    };
    if is_user_local(config) {
        return dotnet_user_profile(&config.environment).map(|profile| {
            profile
                .join("metadata")
                .join("workloads")
                .join(architecture)
                .join(&config.feature_band)
                .join("InstallState")
                .join("default.json")
        });
    }
    let is_msi = config
        .dotnet_root
        .join("metadata")
        .join("workloads")
        .join(&config.feature_band)
        .join("installertype")
        .join("msi")
        .is_file();
    if is_msi {
        return config.environment.get("PROGRAMDATA").map(|program_data| {
            PathBuf::from(program_data)
                .join("dotnet")
                .join("workloads")
                .join(architecture)
                .join(&config.feature_band)
                .join("InstallState")
                .join("default.json")
        });
    }
    Some(
        config
            .dotnet_root
            .join("metadata")
            .join("workloads")
            .join(architecture)
            .join(&config.feature_band)
            .join("InstallState")
            .join("default.json"),
    )
}

fn order_manifests(
    manifests: HashMap<String, ManifestRecord>,
    known_manifest_ids: &[String],
) -> Vec<ManifestRecord> {
    let known_order = known_manifest_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (ascii_key(id), index))
        .collect::<HashMap<_, _>>();
    let mut manifests = manifests.into_values().collect::<Vec<_>>();
    manifests.sort_by(|left, right| {
        let left_order = known_order.get(&ascii_key(&left.id)).copied();
        let right_order = known_order.get(&ascii_key(&right.id)).copied();
        left_order
            .unwrap_or(usize::MAX)
            .cmp(&right_order.unwrap_or(usize::MAX))
            .then_with(|| ascii_case_compare(&left.id, &right.id))
    });
    manifests
}

fn resolve_pack_alias<'a>(pack: &'a SdkPack, runtime_identifiers: &[String]) -> Option<&'a str> {
    if let Some(aliases) = &pack.aliases {
        runtime_identifiers.iter().find_map(|rid| {
            aliases
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(rid))
                .map(|(_, value)| value.as_str())
        })
    } else {
        Some(&pack.id)
    }
}

fn find_installed_pack(roots: &[PathBuf], id: &str, version: &str) -> Option<PathBuf> {
    roots
        .iter()
        .map(|root| root.join("packs").join(id).join(version))
        .find(|path| path.is_dir())
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path_key(path)));
}

fn nearest_global_json(project_path: &Path) -> Option<PathBuf> {
    project_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .ancestors()
        .map(|directory| directory.join("global.json"))
        .find(|path| path.is_file())
}

fn read_nonempty_lines(path: &Path) -> Option<Vec<String>> {
    fs::read_to_string(path).ok().map(|contents| {
        contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn object_field_ignore_ascii_case<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
    context: &str,
) -> Result<Option<&'a serde_json::Value>> {
    let mut matches = object
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name));
    let result = matches.next().map(|(_, value)| value);
    if matches.next().is_some() {
        bail!("{context} defines '{name}' more than once with different casing");
    }
    Ok(result)
}

fn read_sdk_packs(document: &serde_json::Value, path: &Path) -> Result<Vec<SdkPack>> {
    let context = path.display().to_string();
    let root = document
        .as_object()
        .ok_or_else(|| anyhow!("Workload manifest {context} must contain a JSON object"))?;
    let version = object_field_ignore_ascii_case(root, "version", &context)?
        .ok_or_else(|| anyhow!("Workload manifest {context} has no version"))?;
    let valid_version = version.as_str().is_some()
        || version
            .as_i64()
            .is_some_and(|version| (0..i64::from(i32::MAX)).contains(&version));
    if !valid_version {
        bail!("Workload manifest {context} has an invalid version");
    }
    let Some(packs) = object_field_ignore_ascii_case(root, "packs", &context)? else {
        return Ok(Vec::new());
    };
    let packs = packs
        .as_object()
        .ok_or_else(|| anyhow!("Workload manifest {context} 'packs' must be an object"))?;
    let mut result = Vec::new();
    for (id, value) in packs {
        let pack_context = format!("Workload manifest {context} pack '{id}'");
        let pack = value
            .as_object()
            .ok_or_else(|| anyhow!("{pack_context} must be an object"))?;
        for key in pack.keys() {
            if !["version", "kind", "alias-to"]
                .iter()
                .any(|known| key.eq_ignore_ascii_case(known))
            {
                bail!("{pack_context} contains unknown key '{key}'");
            }
        }
        let version = object_field_ignore_ascii_case(pack, "version", &pack_context)?
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("{pack_context} must have a string version"))?;
        let kind = object_field_ignore_ascii_case(pack, "kind", &pack_context)?
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("{pack_context} must have a string kind"))?;
        if !["sdk", "framework", "library", "template", "tool"]
            .iter()
            .any(|known| kind.eq_ignore_ascii_case(known))
        {
            bail!("{pack_context} has unknown kind '{kind}'");
        }
        let aliases =
            object_field_ignore_ascii_case(pack, "alias-to", &pack_context)?
                .map(|aliases| {
                    let aliases = aliases
                        .as_object()
                        .ok_or_else(|| anyhow!("{pack_context} 'alias-to' must be an object"))?;
                    aliases
                        .iter()
                        .map(|(rid, alias)| {
                            alias
                                .as_str()
                                .map(|alias| (rid.clone(), alias.to_string()))
                                .ok_or_else(|| {
                                    anyhow!(
                                        "{pack_context} alias for runtime identifier '{rid}' must be a string"
                                    )
                                })
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?;
        if kind.eq_ignore_ascii_case("sdk") {
            result.push(SdkPack {
                id: id.clone(),
                version: version.to_string(),
                aliases,
            });
        }
    }
    Ok(result)
}

fn parse_relaxed_json(contents: &str) -> serde_json::Result<serde_json::Value> {
    let bytes = contents.as_bytes();
    let mut without_comments = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            without_comments.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            without_comments.push(byte);
            index += 1;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                index += 1;
            }
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                if matches!(bytes[index], b'\r' | b'\n') {
                    without_comments.push(bytes[index]);
                }
                index += 1;
            }
            index = (index + 2).min(bytes.len());
        } else {
            without_comments.push(byte);
            index += 1;
        }
    }

    let mut normalized = Vec::with_capacity(without_comments.len());
    let mut index = 0;
    in_string = false;
    escaped = false;
    while index < without_comments.len() {
        let byte = without_comments[index];
        if in_string {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
            normalized.push(byte);
        } else if byte == b',' {
            let mut next = index + 1;
            while without_comments
                .get(next)
                .is_some_and(u8::is_ascii_whitespace)
            {
                next += 1;
            }
            if !without_comments
                .get(next)
                .is_some_and(|next| matches!(next, b'}' | b']'))
            {
                normalized.push(byte);
            }
        } else {
            normalized.push(byte);
        }
        index += 1;
    }
    serde_json::from_slice(&normalized)
}

fn sdk_feature_band(version: &str) -> Option<String> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    Some(format!("{major}.{minor}.{}", patch / 100 * 100))
}

fn feature_band_compare(left: &str, right: &str) -> Option<Ordering> {
    let left = sdk_feature_band(left)?;
    let right = sdk_feature_band(right)?;
    Some(compare_versions(&left, &right))
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    fn parts(value: &str) -> Option<(Vec<i32>, Option<&str>)> {
        let core_end = value.find('-').unwrap_or(value.len());
        let numbers = value[..core_end]
            .split('.')
            .map(str::parse::<i32>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        if !(2..=4).contains(&numbers.len()) {
            return None;
        }
        let suffix = (core_end < value.len()).then(|| &value[core_end..]);
        Some((numbers, suffix))
    }

    fn compare_identifiers(left: Option<&str>, right: Option<&str>) -> Ordering {
        match (left, right) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => {
                let left = left.split('.').collect::<Vec<_>>();
                let right = right.split('.').collect::<Vec<_>>();
                for (left, right) in left.iter().zip(&right) {
                    let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
                    let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
                    let order = match (left_numeric, right_numeric) {
                        (true, true) => left.len().cmp(&right.len()).then_with(|| left.cmp(right)),
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        (false, false) => left.cmp(right),
                    };
                    if !order.is_eq() {
                        return order;
                    }
                }
                left.len().cmp(&right.len())
            }
        }
    }

    let (Some((left_numbers, left_suffix)), Some((right_numbers, right_suffix))) =
        (parts(left), parts(right))
    else {
        return left.cmp(right);
    };
    let length = left_numbers.len().max(right_numbers.len());
    for index in 0..length {
        let left = left_numbers
            .get(index)
            .copied()
            .map(i64::from)
            .unwrap_or(-1);
        let right = right_numbers
            .get(index)
            .copied()
            .map(i64::from)
            .unwrap_or(-1);
        let order = left.cmp(&right);
        if !order.is_eq() {
            return order;
        }
    }
    compare_identifiers(left_suffix, right_suffix)
}

fn ascii_key(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn ascii_case_compare(left: &str, right: &str) -> Ordering {
    ascii_key(left)
        .cmp(&ascii_key(right))
        .then_with(|| left.cmp(right))
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn toolset(root: &Path) -> ActiveToolset {
        let tools_path = root.join("sdk").join("10.0.302");
        fs::create_dir_all(tools_path.join("Sdks")).unwrap();
        ActiveToolset {
            sdk_root: tools_path.join("Sdks"),
            tools_path,
            msbuild_version: "18.6.0".to_string(),
            msbuild_semantic_version: "18.6.0".to_string(),
        }
    }

    fn write_manifest(dotnet_root: &Path, id: &str, manifest_version: &str) -> Result<PathBuf> {
        let directory = dotnet_root
            .join("sdk-manifests")
            .join("10.0.300")
            .join(id)
            .join(manifest_version);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("WorkloadManifest.json"),
            format!(r#"{{ "version": "{manifest_version}", "workloads": {{}}, "packs": {{}} }}"#),
        )?;
        fs::write(directory.join("WorkloadManifest.targets"), "<Project />")?;
        Ok(directory)
    }

    fn write_workload_set(
        dotnet_root: &Path,
        set_version: &str,
        manifest_version: &str,
    ) -> Result<()> {
        let directory = dotnet_root
            .join("sdk-manifests")
            .join("10.0.300")
            .join("workloadsets")
            .join(set_version);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("baseline.workloadset.json"),
            format!(r#"{{ "example.manifest": "{manifest_version}/10.0.300" }}"#),
        )?;
        Ok(())
    }

    fn write_install_state(config: &WorkloadResolverConfig, contents: &str) -> Result<()> {
        let path = install_state_path(config).expect("install state path");
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, contents)?;
        Ok(())
    }

    #[test]
    fn virtual_locators_use_selected_manifests_and_installed_sdk_packs() -> Result<()> {
        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let dotnet_root = directory.path().join("dotnet");
        let toolset = toolset(&dotnet_root);
        fs::write(
            toolset.tools_path.join("KnownWorkloadManifests.txt"),
            "example.manifest\n",
        )?;
        fs::write(
            toolset
                .tools_path
                .join("NETCoreSdkRuntimeIdentifierChain.txt"),
            "test-rid\nany\n",
        )?;
        let manifest = dotnet_root
            .join("sdk-manifests")
            .join("10.0.300")
            .join("example.manifest")
            .join("1.2.3");
        fs::create_dir_all(&manifest)?;
        fs::write(
            manifest.join("WorkloadManifest.json"),
            r#"{
  "VERSION": "1.2.3",
  "workloads": {},
  "PACKS": {
    "Example.Sdk": {
      "KIND": "sdk",
      "VERSION": "4.5.6",
      "ALIAS-TO": { "TEST-RID": "Example.Sdk.Host" }
    }
  }
}"#,
        )?;
        fs::write(manifest.join("WorkloadManifest.targets"), "<Project />")?;
        let pack_sdk = dotnet_root
            .join("packs")
            .join("Example.Sdk.Host")
            .join("4.5.6")
            .join("Sdk");
        fs::create_dir_all(&pack_sdk)?;
        fs::write(pack_sdk.join("AutoImport.props"), "<Project />")?;

        let project = directory.path().join("project.csproj");
        fs::write(&project, "<Project />")?;
        let config = WorkloadResolverConfig::new(&PropertyMap::new(), &toolset, &project).unwrap();
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(AUTO_IMPORT_PROPS_LOCATOR),
            Some([pack_sdk.clone()].as_slice())
        );
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest.clone()].as_slice())
        );
        assert_eq!(resolver.resolve("Example.Sdk"), Some([pack_sdk].as_slice()));
        Ok(())
    }

    #[test]
    fn explicit_manifest_and_pack_roots_honor_global_workload_set() -> Result<()> {
        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let dotnet_root = directory.path().join("dotnet");
        let toolset = toolset(&dotnet_root);
        let manifest_root = directory.path().join("manifests");
        let pack_root = directory.path().join("packs-root");
        let set_version = "10.0.300-test";
        let set = manifest_root
            .join("10.0.300")
            .join("workloadsets")
            .join(set_version);
        fs::create_dir_all(&set)?;
        fs::write(
            set.join("baseline.workloadset.json"),
            r#"{ "example.manifest": "1.0.0/10.0.300" }"#,
        )?;
        let manifest = manifest_root
            .join("10.0.300")
            .join("example.manifest")
            .join("1.0.0");
        fs::create_dir_all(&manifest)?;
        fs::write(
            manifest.join("WorkloadManifest.json"),
            r#"{
  "version": "1.0.0",
  "workloads": {},
  "packs": { "Example.Sdk": { "kind": "sdk", "version": "2.0.0" } }
}"#,
        )?;
        fs::write(manifest.join("WorkloadManifest.targets"), "<Project />")?;
        let pack_sdk = pack_root
            .join("packs")
            .join("Example.Sdk")
            .join("2.0.0")
            .join("Sdk");
        fs::create_dir_all(&pack_sdk)?;
        fs::write(pack_sdk.join("AutoImport.props"), "<Project />")?;

        let project_directory = directory.path().join("repo");
        fs::create_dir_all(&project_directory)?;
        let project = project_directory.join("project.csproj");
        fs::write(&project, "<Project />")?;
        fs::write(
            project_directory.join("global.json"),
            format!(r#"{{ "sdk": {{ "workloadVersion": "{set_version}" }} }}"#),
        )?;
        let mut environment = PropertyMap::new();
        environment.insert(
            "DOTNETSDK_WORKLOAD_MANIFEST_ROOTS".to_string(),
            manifest_root.to_string_lossy().into_owned(),
        );
        environment.insert(
            "DOTNETSDK_WORKLOAD_MANIFEST_IGNORE_DEFAULT_ROOTS".to_string(),
            "1".to_string(),
        );
        environment.insert(
            "DOTNETSDK_WORKLOAD_PACK_ROOTS".to_string(),
            pack_root.to_string_lossy().into_owned(),
        );
        let config = WorkloadResolverConfig::new(&environment, &toolset, &project).unwrap();
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(AUTO_IMPORT_PROPS_LOCATOR),
            Some([pack_sdk].as_slice())
        );
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest].as_slice())
        );
        Ok(())
    }

    #[test]
    fn upstream_release_version_precedence_orders_numeric_prerelease_identifiers() {
        // SdkDirectoryWorkloadManifestProvider.VersionCompare delegates prerelease
        // ordering to Microsoft.Deployment.DotNet.Releases.ReleaseVersion.
        for (lower, higher) in [
            ("10.0.300-preview.2", "10.0.300-preview.10"),
            ("1.0.0-preview.1.2", "1.0.0-preview.2"),
            (
                "1.0.0-preview.1234567890123456",
                "1.0.0-preview.12345678901234567",
            ),
            ("1.0.0-preview.4", "1.0.0"),
        ] {
            assert_eq!(compare_versions(lower, higher), Ordering::Less);
            assert_eq!(compare_versions(higher, lower), Ordering::Greater);
        }
        assert_eq!(
            compare_versions("1.0.0-preview.4", "1.0.0-preview.4"),
            Ordering::Equal
        );
    }

    #[test]
    fn workload_set_source_controls_install_state_manifest_overlay() -> Result<()> {
        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let dotnet_root = directory.path().join("dotnet");
        let toolset = toolset(&dotnet_root);
        fs::write(
            toolset.tools_path.join("KnownWorkloadManifests.txt"),
            "example.manifest\n",
        )?;
        let manifest_one = write_manifest(&dotnet_root, "example.manifest", "1.0.0")?;
        let manifest_two = write_manifest(&dotnet_root, "example.manifest", "2.0.0")?;
        let manifest_three = write_manifest(&dotnet_root, "example.manifest", "3.0.0")?;
        write_workload_set(&dotnet_root, "10.0.300-preview.2", "1.0.0")?;
        write_workload_set(&dotnet_root, "10.0.300-preview.10", "2.0.0")?;

        let project_directory = directory.path().join("repo");
        fs::create_dir_all(&project_directory)?;
        let project = project_directory.join("project.csproj");
        fs::write(&project, "<Project />")?;
        let config = WorkloadResolverConfig::new(&PropertyMap::new(), &toolset, &project).unwrap();

        write_install_state(
            &config,
            r#"{
  "useWorkloadSets": true,
  "manifests": { "example.manifest": "3.0.0/10.0.300" }
}"#,
        )?;
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest_two.clone()].as_slice()),
            "an automatically selected latest set must not be overlaid by install-state manifests"
        );

        write_install_state(
            &config,
            r#"{
  "workloadVersion": "10.0.300-preview.2",
  "manifests": { "example.manifest": "3.0.0/10.0.300" }
}"#,
        )?;
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest_three.clone()].as_slice()),
            "an install-state-selected set may carry extra install-state manifests"
        );

        fs::write(
            project_directory.join("global.json"),
            r#"{ "sdk": { "workloadVersion": "10.0.300-preview.2" } }"#,
        )?;
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest_one].as_slice()),
            "a global.json-pinned set must not be overlaid by install state"
        );

        write_install_state(
            &config,
            r#"{
  "useWorkloadSets": true,
  "manifests": { "example.manifest": "3.0.0/10.0.300" }
}"#,
        )?;
        fs::write(
            project_directory.join("global.json"),
            r#"{ "SDK": { "WORKLOADS-UPDATE-MODE": "manifests" } }"#,
        )?;
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest_three.clone()].as_slice()),
            "global.json manifests mode overrides install state's workload-set preference"
        );

        write_install_state(
            &config,
            r#"{
  "useWorkloadSets": false,
  "manifests": { "example.manifest": "3.0.0/10.0.300" }
}"#,
        )?;
        fs::write(
            project_directory.join("global.json"),
            r#"{ "sdk": { "workloads-update-mode": "workload-set" } }"#,
        )?;
        let resolver = InstalledWorkloadSdkResolver::discover(&config)?;
        assert_eq!(
            resolver.resolve(MANIFEST_TARGETS_LOCATOR),
            Some([manifest_two].as_slice()),
            "global.json workload-set mode overrides install state's manifest preference"
        );
        Ok(())
    }

    #[test]
    fn install_state_uses_user_local_shared_and_msi_roots() -> Result<()> {
        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let dotnet_root = directory.path().join("dotnet");
        let toolset = toolset(&dotnet_root);
        let project = directory.path().join("project.csproj");
        fs::write(&project, "<Project />")?;
        let home = directory.path().join("home");
        let program_data = directory.path().join("program-data");
        let mut environment = PropertyMap::new();
        environment.insert(
            "DOTNET_CLI_HOME".to_string(),
            home.to_string_lossy().into_owned(),
        );
        environment.insert(
            "PROGRAMDATA".to_string(),
            program_data.to_string_lossy().into_owned(),
        );
        let config = WorkloadResolverConfig::new(&environment, &toolset, &project).unwrap();

        let user_local = dotnet_root
            .join("metadata")
            .join("workloads")
            .join("10.0.300")
            .join("userlocal");
        fs::create_dir_all(user_local.parent().unwrap())?;
        fs::write(&user_local, "")?;
        write_install_state(&config, r#"{ "workloadVersion": "user-local" }"#)?;
        assert_eq!(
            read_install_state(&config)?.workload_version.as_deref(),
            Some("user-local")
        );
        assert!(
            install_state_path(&config)
                .unwrap()
                .starts_with(home.join(".dotnet"))
        );

        fs::remove_file(user_local)?;
        write_install_state(&config, r#"{ "workloadVersion": "shared" }"#)?;
        assert_eq!(
            read_install_state(&config)?.workload_version.as_deref(),
            Some("shared")
        );
        assert!(
            install_state_path(&config)
                .unwrap()
                .starts_with(&dotnet_root)
        );

        let msi = dotnet_root
            .join("metadata")
            .join("workloads")
            .join("10.0.300")
            .join("installertype")
            .join("msi");
        fs::create_dir_all(msi.parent().unwrap())?;
        fs::write(msi, "")?;
        write_install_state(&config, r#"{ "workloadVersion": "msi" }"#)?;
        assert_eq!(
            read_install_state(&config)?.workload_version.as_deref(),
            Some("msi")
        );
        assert!(
            install_state_path(&config)
                .unwrap()
                .starts_with(program_data)
        );
        Ok(())
    }

    #[test]
    fn workload_manifest_fields_are_case_insensitive_and_validated() -> Result<()> {
        let path = Path::new("WorkloadManifest.json");
        let document = parse_relaxed_json(
            r#"{
  "VERSION": "1.0.0",
  "PACKS": {
    "Example.Sdk": {
      "KIND": "SDK",
      "VERSION": "2.0.0",
      "ALIAS-TO": { "TEST-RID": "Example.Host" }
    }
  }
}"#,
        )?;
        let packs = read_sdk_packs(&document, path)?;
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].id, "Example.Sdk");
        assert_eq!(packs[0].version, "2.0.0");
        assert_eq!(
            resolve_pack_alias(&packs[0], &["test-rid".to_string()]),
            Some("Example.Host")
        );

        for invalid in [
            r#"{ "version": "1.0.0", "packs": [] }"#,
            r#"{ "version": "1.0.0", "packs": { "P": { "version": "1.0.0" } } }"#,
            r#"{ "version": "1.0.0", "packs": { "P": { "kind": "sdk", "version": 1 } } }"#,
            r#"{ "version": "1.0.0", "VERSION": "2.0.0", "packs": {} }"#,
            r#"{ "version": "1.0.0", "packs": { "P": { "kind": "sdk", "version": "1.0.0", "unknown": true } } }"#,
        ] {
            let document = parse_relaxed_json(invalid)?;
            assert!(
                read_sdk_packs(&document, path).is_err(),
                "invalid manifest should fail: {invalid}"
            );
        }
        Ok(())
    }
}
