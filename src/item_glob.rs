use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::escaping::{ItemSpecKind, classify_item_spec, unescape_once};
use crate::properties::{display_path, lexical_absolute};

#[derive(Debug, Clone)]
pub(crate) enum ItemSpec {
    Literal { value: String },
    Glob(MsBuildGlob),
}

#[derive(Debug, Clone)]
pub(crate) struct MsBuildGlob {
    project_root: PathBuf,
    fixed_root: PathBuf,
    components: Vec<GlobComponent>,
    subtree_pattern: Option<Vec<GlobComponent>>,
    absolute_identity: bool,
}

#[derive(Debug, Clone)]
struct GlobComponent {
    pattern: String,
    recursive: bool,
}

#[derive(Debug)]
pub(crate) struct GlobMatch {
    pub path: PathBuf,
    pub recursive_dir: String,
}

#[derive(Debug, Default, Clone)]
struct DirectoryEntries {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

impl ItemSpec {
    pub(crate) fn parse(project_root: &Path, escaped_spec: &str) -> Result<Self> {
        let value = unescape_once(escaped_spec);
        if classify_item_spec(escaped_spec) == ItemSpecKind::Literal || !is_legal_glob(&value) {
            return Ok(Self::Literal { value });
        }

        Ok(Self::Glob(MsBuildGlob::parse(project_root, &value)?))
    }
}

impl MsBuildGlob {
    fn parse(project_root: &Path, spec: &str) -> Result<Self> {
        let project_root = lexical_absolute(project_root)?;
        let native_spec = native_path(spec);
        let spec_path = Path::new(&native_spec);
        let absolute_identity = spec_path.is_absolute();
        let first_wildcard = native_spec
            .find(['*', '?'])
            .expect("a glob has an authored wildcard");
        let fixed_end = native_spec[..first_wildcard]
            .rfind(std::path::MAIN_SEPARATOR)
            .map_or(0, |position| {
                position + std::path::MAIN_SEPARATOR.len_utf8()
            });
        let fixed_part = &native_spec[..fixed_end];
        let wildcard_part = &native_spec[fixed_end..];
        let fixed_root = rooted_lexical_path(&project_root, fixed_part)?;
        let ended_with_separator = native_spec.ends_with(std::path::MAIN_SEPARATOR);
        let mut components = split_components(wildcard_part)
            .into_iter()
            .map(|pattern| GlobComponent {
                recursive: pattern == "**",
                pattern,
            })
            .collect::<Vec<_>>();

        let subtree_pattern = (components.last().is_some_and(|part| part.recursive)
            && !ended_with_separator)
            .then(|| components[..components.len() - 1].to_vec());
        if subtree_pattern.is_some() {
            components.push(GlobComponent {
                pattern: "*.*".to_string(),
                recursive: false,
            });
        }

        Ok(Self {
            project_root,
            fixed_root,
            components,
            subtree_pattern,
            absolute_identity,
        })
    }

    pub(crate) fn matches(&self, identity: &str) -> bool {
        let Ok(candidate) = rooted_lexical_path(&self.project_root, identity) else {
            return false;
        };
        let Some(relative) = relative_components(&self.fixed_root, &candidate) else {
            return false;
        };
        matches_components(&self.components, &relative, true)
    }

    pub(crate) fn covers_directory(&self, directory: &Path) -> bool {
        let Some(pattern) = &self.subtree_pattern else {
            return false;
        };
        let Some(relative) = relative_components(&self.fixed_root, directory) else {
            return false;
        };

        (0..=relative.len()).any(|length| matches_components(pattern, &relative[..length], false))
    }

    pub(crate) fn enumerate(&self, excludes: &[MsBuildGlob]) -> Vec<GlobMatch> {
        let mut paths = Vec::new();
        let mut seen_paths = HashSet::new();
        let mut seen_states = HashSet::new();
        let mut directory_cache = HashMap::new();
        self.walk(
            &self.fixed_root,
            0,
            excludes,
            &mut paths,
            &mut seen_paths,
            &mut seen_states,
            &mut directory_cache,
        );

        paths
            .into_iter()
            .map(|path| GlobMatch {
                recursive_dir: self.recursive_dir(&path),
                path,
            })
            .collect()
    }

    pub(crate) fn identity_for_path(&self, path: &Path) -> String {
        if self.absolute_identity {
            return display_path(path);
        }
        lexical_relative(&self.project_root, path)
            .map(|relative| display_path(&relative))
            .unwrap_or_else(|| display_path(path))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        directory: &Path,
        component_index: usize,
        excludes: &[MsBuildGlob],
        paths: &mut Vec<PathBuf>,
        seen_paths: &mut HashSet<String>,
        seen_states: &mut HashSet<(String, usize)>,
        directory_cache: &mut HashMap<PathBuf, DirectoryEntries>,
    ) {
        if excludes
            .iter()
            .any(|exclude| exclude.covers_directory(directory))
        {
            return;
        }

        let state = (path_compare_key(directory), component_index);
        if !seen_states.insert(state) || component_index >= self.components.len() {
            return;
        }

        let component = &self.components[component_index];
        let is_filename = component_index + 1 == self.components.len();
        if is_filename {
            let entries = read_directory(directory, directory_cache);
            for path in &entries.files {
                let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                    continue;
                };
                if component_matches(name, &component.pattern, true)
                    && !excludes
                        .iter()
                        .any(|exclude| exclude.matches(&display_path(path)))
                {
                    let key = path_compare_key(path);
                    if seen_paths.insert(key) {
                        paths.push(path.clone());
                    }
                }
            }
            return;
        }

        if component.recursive {
            self.walk(
                directory,
                component_index + 1,
                excludes,
                paths,
                seen_paths,
                seen_states,
                directory_cache,
            );
            let entries = read_directory(directory, directory_cache);
            for child in &entries.directories {
                self.walk(
                    child,
                    component_index,
                    excludes,
                    paths,
                    seen_paths,
                    seen_states,
                    directory_cache,
                );
            }
            return;
        }

        let entries = read_directory(directory, directory_cache);
        for child in &entries.directories {
            let Some(name) = child.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if component_matches(name, &component.pattern, false) {
                self.walk(
                    child,
                    component_index + 1,
                    excludes,
                    paths,
                    seen_paths,
                    seen_states,
                    directory_cache,
                );
            }
        }
    }

    fn recursive_dir(&self, path: &Path) -> String {
        let Some(mut relative) = relative_components(&self.fixed_root, path) else {
            return String::new();
        };
        relative.pop();
        if relative.is_empty() {
            return String::new();
        }
        let mut value = relative.join(std::path::MAIN_SEPARATOR_STR);
        value.push(std::path::MAIN_SEPARATOR);
        value
    }
}

pub(crate) fn normalized_identity_key(project_root: &Path, identity: &str) -> String {
    rooted_lexical_path(project_root, identity)
        .map(|path| path_compare_key(&path))
        .unwrap_or_else(|_| {
            let path = native_path(identity);
            if cfg!(windows) {
                path.to_lowercase()
            } else {
                path
            }
        })
}

fn is_legal_glob(spec: &str) -> bool {
    if spec.contains(['\0', '"', '<', '>', '|'])
        || spec.contains("...")
        || spec.rfind(':').is_some_and(|position| position != 1)
    {
        return false;
    }

    let components = split_components(spec);
    let Some(first_wildcard) = components
        .iter()
        .position(|component| component.contains(['*', '?']))
    else {
        return false;
    };

    for (index, component) in components.iter().enumerate() {
        if index >= first_wildcard && index + 1 < components.len() && component.contains("..") {
            return false;
        }
        if component.contains("**") && component != "**" {
            return false;
        }
    }
    true
}

fn split_components(value: &str) -> Vec<String> {
    value
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_string)
        .collect()
}

fn rooted_lexical_path(project_root: &Path, value: &str) -> std::io::Result<PathBuf> {
    lexical_absolute(&project_root.join(native_path(value)))
}

fn native_path(value: &str) -> String {
    if std::path::MAIN_SEPARATOR == '\\' {
        value.replace('/', "\\")
    } else {
        value.replace('\\', "/")
    }
}

fn relative_components(base: &Path, candidate: &Path) -> Option<Vec<String>> {
    let base_components = base.components().collect::<Vec<_>>();
    let candidate_components = candidate.components().collect::<Vec<_>>();
    if base_components.len() > candidate_components.len()
        || !base_components
            .iter()
            .zip(&candidate_components)
            .all(|(left, right)| path_component_eq(*left, *right))
    {
        return None;
    }

    candidate_components[base_components.len()..]
        .iter()
        .map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn lexical_relative(base: &Path, candidate: &Path) -> Option<PathBuf> {
    let base_components = base.components().collect::<Vec<_>>();
    let candidate_components = candidate.components().collect::<Vec<_>>();
    if base_components
        .first()
        .zip(candidate_components.first())
        .is_some_and(|(left, right)| !path_component_eq(*left, *right))
    {
        return None;
    }

    let common = base_components
        .iter()
        .zip(&candidate_components)
        .take_while(|(left, right)| path_component_eq(**left, **right))
        .count();
    if base_components[..common]
        .iter()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        != candidate_components[..common]
            .iter()
            .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &base_components[common..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &candidate_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn path_component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    std::mem::discriminant(&left) == std::mem::discriminant(&right)
        && os_eq(left.as_os_str(), right.as_os_str())
}

fn os_eq(left: &OsStr, right: &OsStr) -> bool {
    if cfg!(windows) {
        let left = left.to_string_lossy();
        let right = right.to_string_lossy();
        left.eq_ignore_ascii_case(&right) || left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

fn path_compare_key(path: &Path) -> String {
    let value = display_path(path);
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn path_sort_key(path: &Path) -> (String, String) {
    (path_compare_key(path), display_path(path))
}

fn matches_components(
    patterns: &[GlobComponent],
    values: &[String],
    final_component_is_filename: bool,
) -> bool {
    fn visit(
        patterns: &[GlobComponent],
        values: &[String],
        pattern_index: usize,
        value_index: usize,
        final_component_is_filename: bool,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = if pattern_index == patterns.len() {
            value_index == values.len()
        } else if patterns[pattern_index].recursive {
            visit(
                patterns,
                values,
                pattern_index + 1,
                value_index,
                final_component_is_filename,
                memo,
            ) || (value_index < values.len()
                && visit(
                    patterns,
                    values,
                    pattern_index,
                    value_index + 1,
                    final_component_is_filename,
                    memo,
                ))
        } else {
            value_index < values.len()
                && component_matches(
                    &values[value_index],
                    &patterns[pattern_index].pattern,
                    final_component_is_filename && pattern_index + 1 == patterns.len(),
                )
                && visit(
                    patterns,
                    values,
                    pattern_index + 1,
                    value_index + 1,
                    final_component_is_filename,
                    memo,
                )
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    visit(
        patterns,
        values,
        0,
        0,
        final_component_is_filename,
        &mut HashMap::new(),
    )
}

fn component_matches(value: &str, pattern: &str, is_filename: bool) -> bool {
    let normalized_pattern = if is_filename {
        pattern.replace("*.*", "*")
    } else {
        pattern.to_string()
    };
    let value = value.chars().collect::<Vec<_>>();
    let pattern = normalized_pattern.chars().collect::<Vec<_>>();
    let mut memo = HashMap::new();

    fn visit(
        value: &[char],
        pattern: &[char],
        value_index: usize,
        pattern_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(value_index, pattern_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            value_index == value.len()
        } else {
            match pattern[pattern_index] {
                '*' => {
                    visit(value, pattern, value_index, pattern_index + 1, memo)
                        || (value_index < value.len()
                            && visit(value, pattern, value_index + 1, pattern_index, memo))
                }
                '?' => {
                    value_index < value.len()
                        && visit(value, pattern, value_index + 1, pattern_index + 1, memo)
                }
                expected => {
                    value_index < value.len()
                        && character_eq(value[value_index], expected)
                        && visit(value, pattern, value_index + 1, pattern_index + 1, memo)
                }
            }
        };
        memo.insert((value_index, pattern_index), result);
        result
    }

    visit(&value, &pattern, 0, 0, &mut memo)
}

fn character_eq(left: char, right: char) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(&right)
            || left.to_lowercase().to_string() == right.to_lowercase().to_string()
    } else {
        left == right
    }
}

fn read_directory(
    directory: &Path,
    cache: &mut HashMap<PathBuf, DirectoryEntries>,
) -> DirectoryEntries {
    if let Some(entries) = cache.get(directory) {
        return entries.clone();
    }

    let mut result = DirectoryEntries::default();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => result.directories.push(path),
                Ok(file_type) if file_type.is_file() => result.files.push(path),
                Ok(file_type) if file_type.is_symlink() && path.is_file() => {
                    result.files.push(path);
                }
                _ => {}
            }
        }
    }
    result
        .directories
        .sort_by_cached_key(|path| path_sort_key(path));
    result.files.sort_by_cached_key(|path| path_sort_key(path));
    cache.insert(directory.to_path_buf(), result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_matching_uses_msbuild_filename_rules() {
        assert!(component_matches("Dockerfile", "*.*", true));
        assert!(component_matches("literal[1].txt", "literal[1].txt", true));
        assert!(!component_matches("literal1.txt", "literal[1].txt", true));
        assert!(component_matches("abc.txt", "a?c.*", true));
    }

    #[test]
    fn illegal_recursive_operators_are_not_globs() -> Result<()> {
        let root = lexical_absolute(Path::new("."))?;
        assert!(matches!(
            ItemSpec::parse(&root, "tree/**.txt")?,
            ItemSpec::Literal { .. }
        ));
        assert!(matches!(
            ItemSpec::parse(&root, "%2A-*.txt")?,
            ItemSpec::Literal { .. }
        ));
        assert!(matches!(
            ItemSpec::parse(&root, "tree/**")?,
            ItemSpec::Glob(_)
        ));
        Ok(())
    }

    #[test]
    fn recursive_excludes_identify_only_safe_subtree_prunes() -> Result<()> {
        let root = lexical_absolute(Path::new("."))?;
        let ItemSpec::Glob(fixed) = ItemSpec::parse(&root, "node_modules/**")? else {
            panic!("expected a glob");
        };
        assert!(fixed.covers_directory(&root.join("node_modules")));
        assert!(fixed.covers_directory(&root.join("node_modules").join("package")));
        assert!(!fixed.covers_directory(&root.join("src")));

        let ItemSpec::Glob(anywhere) = ItemSpec::parse(&root, "**/node_modules/**")? else {
            panic!("expected a glob");
        };
        assert!(anywhere.covers_directory(&root.join("src").join("node_modules")));
        assert!(anywhere.covers_directory(&root.join("src").join("node_modules").join("package")));
        assert!(!anywhere.covers_directory(&root.join("src").join("packages")));
        Ok(())
    }
}
