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

struct WorkloadSetSelection {
    feature_band: String,
    manifests: serde_json::Map<String, serde_json::Value>,
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

        let global_workload_version = nearest_global_json(&config.project_path)
            .and_then(|path| read_global_workload_version(&path));
        let install_state = read_install_state(config);
        let workload_version = global_workload_version
            .as_deref()
            .or(install_state.workload_version.as_deref());
        if let Some(workload_version) = workload_version {
            let workload_set = find_workload_set(
                &manifest_roots,
                &config.feature_band,
                workload_version,
            )?
            .ok_or_else(|| {
                    anyhow!(
                        "Workload set '{workload_version}' was not found under the configured workload manifest roots"
                    )
                })?;
            overlay_manifest_specifiers(
                &mut manifests,
                &manifest_roots,
                &workload_set.feature_band,
                &workload_set.manifests,
            )?;
        } else if install_state.use_workload_sets
            && let Some((_, workload_set)) =
                latest_workload_set(&manifest_roots, &config.feature_band)?
        {
            overlay_manifest_specifiers(
                &mut manifests,
                &manifest_roots,
                &workload_set.feature_band,
                &workload_set.manifests,
            )?;
        }
        if global_workload_version.is_none()
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
            let Some(packs) = document.get("packs").and_then(serde_json::Value::as_object) else {
                continue;
            };
            let mut pack_names = packs.keys().collect::<Vec<_>>();
            pack_names.sort_by(|left, right| ascii_case_compare(left, right));
            for pack_name in pack_names {
                let pack = &packs[pack_name];
                if !pack
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("sdk"))
                {
                    continue;
                }
                let Some(version) = pack.get("version").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(resolved_name) = resolve_pack_alias(pack_name, pack, &runtime_identifiers)
                else {
                    continue;
                };
                let Some(pack_directory) = find_installed_pack(&pack_roots, resolved_name, version)
                else {
                    continue;
                };
                let sdk_directory = pack_directory.join("Sdk");
                if !sdk_directory.is_dir() {
                    continue;
                }
                pack_sdk_paths
                    .entry(ascii_key(pack_name))
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
    let mut versions = Vec::new();
    for root in roots {
        let sets = root.join(feature_band).join("workloadsets");
        let Ok(entries) = fs::read_dir(sets) else {
            continue;
        };
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            versions.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
    }
    versions.sort_by(|left, right| compare_versions(&left.0, &right.0));
    let Some((version, directory)) = versions.pop() else {
        return Ok(None);
    };
    Ok(Some((
        version,
        WorkloadSetSelection {
            feature_band: feature_band.to_string(),
            manifests: read_workload_set_directory(&directory)?,
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

fn read_global_workload_version(path: &Path) -> Option<String> {
    let document: serde_json::Value = parse_relaxed_json(&fs::read_to_string(path).ok()?).ok()?;
    document
        .get("sdk")?
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("workloadVersion"))
        .and_then(|(_, value)| value.as_str())
        .map(str::to_string)
}

fn read_install_state(config: &WorkloadResolverConfig) -> InstallState {
    let architecture = match env::consts::ARCH {
        "x86_64" => "X64",
        "x86" => "X86",
        "aarch64" => "Arm64",
        "arm" => "Arm",
        other => other,
    };
    let mut candidates = vec![
        config
            .dotnet_root
            .join("metadata")
            .join("workloads")
            .join(architecture)
            .join(&config.feature_band)
            .join("InstallState")
            .join("default.json"),
    ];
    if let Some(program_data) = config.environment.get("PROGRAMDATA") {
        candidates.push(
            PathBuf::from(program_data)
                .join("dotnet")
                .join("workloads")
                .join(architecture)
                .join(&config.feature_band)
                .join("InstallState")
                .join("default.json"),
        );
    }
    let document = candidates.into_iter().find_map(|path| {
        fs::read_to_string(path)
            .ok()
            .and_then(|contents| parse_relaxed_json(&contents).ok())
    });
    let workload_version = document
        .as_ref()
        .and_then(|value| value.get("workloadVersion"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let manifests = document
        .as_ref()
        .and_then(|value| value.get("manifests"))
        .and_then(serde_json::Value::as_object)
        .cloned();
    let use_workload_sets = document
        .as_ref()
        .and_then(|value| value.get("useWorkloadSets"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    InstallState {
        workload_version,
        manifests,
        use_workload_sets,
    }
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

fn resolve_pack_alias<'a>(
    pack_name: &'a str,
    pack: &'a serde_json::Value,
    runtime_identifiers: &[String],
) -> Option<&'a str> {
    let aliases = pack.get("alias-to").and_then(serde_json::Value::as_object);
    if let Some(aliases) = aliases {
        runtime_identifiers.iter().find_map(|rid| {
            aliases
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(rid))
                .and_then(|(_, value)| value.as_str())
        })
    } else {
        Some(pack_name)
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
    fn parts(value: &str) -> (Vec<u64>, Option<&str>) {
        let core_end = value.find(['-', '+']).unwrap_or(value.len());
        let numbers = value[..core_end]
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or_default())
            .collect::<Vec<_>>();
        let suffix = (core_end < value.len()).then(|| &value[core_end + 1..]);
        (numbers, suffix)
    }
    let (left_numbers, left_suffix) = parts(left);
    let (right_numbers, right_suffix) = parts(right);
    let length = left_numbers.len().max(right_numbers.len());
    for index in 0..length {
        let order = left_numbers
            .get(index)
            .copied()
            .unwrap_or_default()
            .cmp(&right_numbers.get(index).copied().unwrap_or_default());
        if !order.is_eq() {
            return order;
        }
    }
    match (left_suffix, right_suffix) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    }
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
  "version": "1.2.3",
  "workloads": {},
  "packs": {
    "Example.Sdk": {
      "kind": "sdk",
      "version": "4.5.6",
      "alias-to": { "test-rid": "Example.Sdk.Host" }
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
}
