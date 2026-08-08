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
    logical_fixed_root: PathBuf,
    components: Vec<GlobComponent>,
    logical_root: LogicalRoot,
    logical_components: Vec<GlobComponent>,
    logical_subtree_pattern: Option<Vec<GlobComponent>>,
    terminal_separator: bool,
}

#[derive(Debug, Clone)]
struct GlobComponent {
    pattern: String,
    recursive: bool,
}

impl GlobComponent {
    fn new(pattern: String) -> Self {
        Self {
            recursive: pattern == "**",
            pattern,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogicalRoot {
    prefix: Option<String>,
    rooted: bool,
}

#[derive(Debug)]
struct LogicalPath {
    root: LogicalRoot,
    components: Vec<String>,
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
        let terminal_separator = native_spec.ends_with(std::path::MAIN_SEPARATOR);
        let mut components = split_components(wildcard_part)
            .into_iter()
            .map(GlobComponent::new)
            .collect::<Vec<_>>();

        let subtree_pattern = (components.last().is_some_and(|part| part.recursive)
            && !terminal_separator)
            .then(|| components[..components.len() - 1].to_vec());
        if subtree_pattern.is_some() {
            components.push(GlobComponent::new("*.*".to_string()));
        }

        let logical_path = split_logical_path(&native_spec);
        let mut logical_components = logical_path
            .components
            .into_iter()
            .map(GlobComponent::new)
            .collect::<Vec<_>>();
        let logical_subtree_pattern =
            (logical_components.last().is_some_and(|part| part.recursive) && !terminal_separator)
                .then(|| logical_components[..logical_components.len() - 1].to_vec());
        if logical_subtree_pattern.is_some() {
            logical_components.push(GlobComponent::new("*.*".to_string()));
        }

        Ok(Self {
            project_root,
            fixed_root,
            logical_fixed_root: PathBuf::from(fixed_part),
            components,
            logical_root: logical_path.root,
            logical_components,
            logical_subtree_pattern,
            terminal_separator,
        })
    }

    pub(crate) fn matches(&self, identity: &str) -> bool {
        if self.terminal_separator {
            return false;
        }
        let Ok(candidate) = rooted_lexical_path(&self.project_root, identity) else {
            return false;
        };
        let Some(relative) = relative_components(&self.fixed_root, &candidate) else {
            return false;
        };
        matches_components(&self.components, &relative, true)
    }

    pub(crate) fn matches_exclude(&self, identity: &str) -> bool {
        if self.terminal_separator {
            return false;
        }
        let candidate = split_logical_path(&native_path(identity));
        if logical_root_eq(&self.logical_root, &candidate.root)
            && matches_components(&self.logical_components, &candidate.components, true)
        {
            return true;
        }
        physical_exclude_fallback_allowed(
            &self.logical_root,
            self.logical_components
                .first()
                .is_some_and(|component| component.pattern == "."),
            has_embedded_parent_pattern(&self.logical_components),
            &candidate.root,
            candidate
                .components
                .first()
                .is_some_and(|component| component == "."),
            has_embedded_parent(&candidate.components),
        ) && self.matches(identity)
    }

    pub(crate) fn covers_logical_directory(&self, identity: &str) -> bool {
        if self.terminal_separator {
            return false;
        }
        let Some(pattern) = &self.logical_subtree_pattern else {
            return false;
        };
        let candidate = split_logical_path(&native_path(identity));
        if !logical_root_eq(&self.logical_root, &candidate.root) {
            return false;
        }

        (0..=candidate.components.len())
            .any(|length| matches_components(pattern, &candidate.components[..length], false))
    }

    pub(crate) fn enumerate(&self, excludes: &[MsBuildGlob]) -> Vec<GlobMatch> {
        if self.terminal_separator {
            return Vec::new();
        }
        let mut paths = Vec::new();
        let mut seen_paths = HashSet::new();
        let mut seen_states = HashSet::new();
        let mut directory_cache = HashMap::new();
        let mut canonical_ancestors = HashSet::new();
        if let Some(identity) = canonical_directory_identity(&self.fixed_root) {
            canonical_ancestors.insert(identity);
        }
        self.walk(
            &self.fixed_root,
            0,
            excludes,
            &mut paths,
            &mut seen_paths,
            &mut seen_states,
            &mut directory_cache,
            &mut canonical_ancestors,
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
        let Some(relative) = lexical_relative(&self.fixed_root, path) else {
            return display_path(path);
        };
        let mut identity = self.logical_fixed_root.clone();
        if !relative.as_os_str().is_empty() {
            identity.push(relative);
        }
        display_path(&identity)
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
        canonical_ancestors: &mut HashSet<String>,
    ) {
        let logical_directory = self.identity_for_path(directory);
        if excludes
            .iter()
            .any(|exclude| exclude.covers_logical_directory(&logical_directory))
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
                if !component_matches(name, &component.pattern, true) {
                    continue;
                }
                let identity = self.identity_for_path(path);
                if excludes
                    .iter()
                    .any(|exclude| exclude.matches_exclude(&identity))
                {
                    continue;
                }
                let key = path_compare_key(path);
                if seen_paths.insert(key) {
                    paths.push(path.clone());
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
                canonical_ancestors,
            );
            let entries = read_directory(directory, directory_cache);
            for child in &entries.directories {
                self.walk_child(
                    child,
                    component_index,
                    excludes,
                    paths,
                    seen_paths,
                    seen_states,
                    directory_cache,
                    canonical_ancestors,
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
                self.walk_child(
                    child,
                    component_index + 1,
                    excludes,
                    paths,
                    seen_paths,
                    seen_states,
                    directory_cache,
                    canonical_ancestors,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_child(
        &self,
        child: &Path,
        component_index: usize,
        excludes: &[MsBuildGlob],
        paths: &mut Vec<PathBuf>,
        seen_paths: &mut HashSet<String>,
        seen_states: &mut HashSet<(String, usize)>,
        directory_cache: &mut HashMap<PathBuf, DirectoryEntries>,
        canonical_ancestors: &mut HashSet<String>,
    ) {
        let canonical_identity = canonical_directory_identity(child);
        if canonical_identity
            .as_ref()
            .is_some_and(|identity| !canonical_ancestors.insert(identity.clone()))
        {
            return;
        }
        self.walk(
            child,
            component_index,
            excludes,
            paths,
            seen_paths,
            seen_states,
            directory_cache,
            canonical_ancestors,
        );
        if let Some(identity) = canonical_identity {
            canonical_ancestors.remove(&identity);
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

pub(crate) fn exclude_literal_matches(project_root: &Path, pattern: &str, identity: &str) -> bool {
    let native_pattern = native_path(pattern);
    let native_identity = native_path(identity);
    let pattern = split_logical_path(&native_pattern);
    let candidate = split_logical_path(&native_identity);
    if logical_root_eq(&pattern.root, &candidate.root)
        && pattern.components.len() == candidate.components.len()
        && pattern
            .components
            .iter()
            .zip(&candidate.components)
            .all(|(left, right)| path_text_eq(left, right))
    {
        return true;
    }
    physical_exclude_fallback_allowed(
        &pattern.root,
        pattern
            .components
            .first()
            .is_some_and(|component| component == "."),
        has_embedded_parent(&pattern.components),
        &candidate.root,
        candidate
            .components
            .first()
            .is_some_and(|component| component == "."),
        has_embedded_parent(&candidate.components),
    ) && normalized_identity_key(project_root, &native_pattern)
        == normalized_identity_key(project_root, identity)
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

fn split_logical_path(value: &str) -> LogicalPath {
    let mut remainder = value;
    let mut root = LogicalRoot {
        prefix: None,
        rooted: false,
    };

    if cfg!(windows) {
        if let Some(without_prefix) = remainder.strip_prefix(r"\\") {
            let mut parts = without_prefix.splitn(3, '\\');
            let server = parts.next().unwrap_or_default();
            let share = parts.next().unwrap_or_default();
            if !server.is_empty() && !share.is_empty() {
                root.prefix = Some(format!(r"\\{server}\{share}"));
                root.rooted = true;
                remainder = parts.next().unwrap_or_default();
            } else {
                root.rooted = true;
                remainder = without_prefix.trim_start_matches('\\');
            }
        } else if remainder.as_bytes().get(1) == Some(&b':') {
            root.prefix = Some(remainder[..2].to_string());
            remainder = &remainder[2..];
            if remainder.starts_with('\\') {
                root.rooted = true;
                remainder = remainder.trim_start_matches('\\');
            }
        } else if remainder.starts_with('\\') {
            root.rooted = true;
            remainder = remainder.trim_start_matches('\\');
        }
    } else if remainder.starts_with('/') {
        root.rooted = true;
        remainder = remainder.trim_start_matches('/');
    }

    LogicalPath {
        root,
        components: remainder
            .split(std::path::MAIN_SEPARATOR)
            .filter(|component| !component.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

fn logical_root_eq(left: &LogicalRoot, right: &LogicalRoot) -> bool {
    left.rooted == right.rooted
        && match (&left.prefix, &right.prefix) {
            (Some(left), Some(right)) => path_text_eq(left, right),
            (None, None) => true,
            _ => false,
        }
}

fn physical_exclude_fallback_allowed(
    pattern_root: &LogicalRoot,
    pattern_has_leading_curdir: bool,
    pattern_has_embedded_parent: bool,
    candidate_root: &LogicalRoot,
    candidate_has_leading_curdir: bool,
    candidate_has_embedded_parent: bool,
) -> bool {
    !is_windows_root_relative(pattern_root)
        && !is_windows_root_relative(candidate_root)
        && !pattern_has_leading_curdir
        && !candidate_has_leading_curdir
        && !pattern_has_embedded_parent
        && !candidate_has_embedded_parent
}

fn is_windows_root_relative(root: &LogicalRoot) -> bool {
    cfg!(windows) && root.rooted && root.prefix.is_none()
}

fn has_embedded_parent_pattern(components: &[GlobComponent]) -> bool {
    let mut has_non_parent = false;
    for component in components {
        if component.pattern == "." {
            continue;
        }
        if component.pattern == ".." {
            if has_non_parent {
                return true;
            }
        } else {
            has_non_parent = true;
        }
    }
    false
}

fn has_embedded_parent(components: &[String]) -> bool {
    let mut has_non_parent = false;
    for component in components {
        if component == "." {
            continue;
        }
        if component == ".." {
            if has_non_parent {
                return true;
            }
        } else {
            has_non_parent = true;
        }
    }
    false
}

fn path_text_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right) || left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
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

fn canonical_directory_identity(path: &Path) -> Option<String> {
    path.canonicalize()
        .ok()
        .map(|canonical| path_compare_key(&canonical))
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
    let normalized_pattern = if is_filename && pattern == "*.*" {
        "*".to_string()
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
                Ok(file_type) if file_type.is_symlink() => {
                    if path.is_dir() {
                        result.directories.push(path);
                    } else if path.is_file() {
                        result.files.push(path);
                    }
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
    use tempfile::TempDir;

    #[test]
    fn component_matching_uses_msbuild_filename_rules() {
        assert!(component_matches("Dockerfile", "*.*", true));
        assert!(!component_matches("Dockerfile", "D*.*", true));
        assert!(component_matches("Dockerfile.txt", "D*.*", true));
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
        assert!(fixed.covers_logical_directory("node_modules"));
        assert!(fixed.covers_logical_directory("node_modules/package"));
        assert!(!fixed.covers_logical_directory("src"));

        let ItemSpec::Glob(anywhere) = ItemSpec::parse(&root, "**/node_modules/**")? else {
            panic!("expected a glob");
        };
        assert!(anywhere.covers_logical_directory("src/node_modules"));
        assert!(anywhere.covers_logical_directory("src/node_modules/package"));
        assert!(!anywhere.covers_logical_directory("src/packages"));
        Ok(())
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn recursive_globs_follow_directory_symlinks_without_cycles() -> Result<()> {
        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let real = directory.path().join("real");
        fs::create_dir_all(real.join("deep"))?;
        fs::write(real.join("deep").join("input.txt"), "input")?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, directory.path().join("alias"))?;
            std::os::unix::fs::symlink(&real, real.join("loop"))?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_dir;
            if symlink_dir(&real, directory.path().join("alias")).is_err()
                || symlink_dir(&real, real.join("loop")).is_err()
            {
                return Ok(());
            }
        }

        let ItemSpec::Glob(pattern) = ItemSpec::parse(directory.path(), "alias/**/*.txt")? else {
            panic!("expected a glob");
        };
        let identities = pattern
            .enumerate(&[])
            .iter()
            .map(|matched| pattern.identity_for_path(&matched.path))
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            [display_path(
                &PathBuf::from("alias").join("deep").join("input.txt")
            )]
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn recursive_symlink_glob_identities_match_dotnet_msbuild() -> Result<()> {
        use std::process::Command;

        let directory = TempDir::new_in(env!("CARGO_MANIFEST_DIR"))?;
        let real = directory.path().join("real");
        fs::create_dir_all(real.join("deep"))?;
        fs::write(real.join("deep").join("input.txt"), "input")?;
        std::os::unix::fs::symlink(&real, directory.path().join("alias"))?;
        let project = directory.path().join("project.proj");
        fs::write(
            &project,
            r#"<Project><ItemGroup><I Include="**/*.txt" /></ItemGroup></Project>"#,
        )?;

        let ItemSpec::Glob(pattern) = ItemSpec::parse(directory.path(), "**/*.txt")? else {
            panic!("expected a glob");
        };
        let rust = pattern
            .enumerate(&[])
            .iter()
            .map(|matched| pattern.identity_for_path(&matched.path))
            .collect::<Vec<_>>();
        let output = match Command::new("dotnet")
            .args([
                "msbuild",
                project.to_str().unwrap(),
                "-nologo",
                "-getItem:I",
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let dotnet = result["Items"]["I"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Identity"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(rust, dotnet);
        Ok(())
    }
}
