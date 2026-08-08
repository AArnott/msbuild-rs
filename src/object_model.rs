use indexmap::{Equivalent, IndexMap};
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::escaping::{EscapedString, ItemSpecKind, classify_item_spec, escape, unescape_once};
use crate::file_times::FileTimes;
use crate::properties::{display_path, lexical_absolute};

/// An insertion-ordered map with O(1) ASCII case-insensitive lookup.
///
/// Stored and borrowed keys use the same folded hash, so lookup allocates
/// nothing while the spelling from the first definition remains stable.
#[derive(Debug, Clone)]
pub struct CaseInsensitiveMap<V> {
    entries: IndexMap<CaseInsensitiveKey, (String, V)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaseInsensitiveKey(String);

struct CaseInsensitiveLookup<'a>(&'a str);

impl Hash for CaseInsensitiveKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_name(&self.0, state);
    }
}

impl Hash for CaseInsensitiveLookup<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_name(self.0, state);
    }
}

impl Equivalent<CaseInsensitiveKey> for CaseInsensitiveLookup<'_> {
    fn equivalent(&self, key: &CaseInsensitiveKey) -> bool {
        self.0.eq_ignore_ascii_case(&key.0)
    }
}

impl<V> CaseInsensitiveMap<V> {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
        }
    }

    pub fn insert(&mut self, name: String, value: V) -> Option<V> {
        if let Some((_, existing_value)) = self.entries.get_mut(&CaseInsensitiveLookup(&name)) {
            return Some(std::mem::replace(existing_value, value));
        }
        self.entries
            .insert(CaseInsensitiveKey(name.clone()), (name, value));
        None
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        self.entries
            .get(&CaseInsensitiveLookup(name))
            .map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut V> {
        self.entries
            .get_mut(&CaseInsensitiveLookup(name))
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.entries.contains_key(&CaseInsensitiveLookup(name))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.entries.values().map(|(name, value)| (name, value))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut V)> {
        self.entries
            .values_mut()
            .map(|(name, value)| (&*name, value))
    }
}

impl<V> Default for CaseInsensitiveMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_name<H: Hasher>(name: &str, state: &mut H) {
    state.write_usize(name.len());
    for byte in name.bytes() {
        state.write_u8(byte.to_ascii_lowercase());
    }
}

#[derive(Debug, Clone)]
pub struct EscapedValue {
    escaped: EscapedString,
    value: OnceLock<String>,
}

impl EscapedValue {
    pub fn new(escaped: String) -> Self {
        let escaped = EscapedString::new(escaped);
        Self {
            escaped,
            value: OnceLock::new(),
        }
    }

    pub fn value(&self) -> &str {
        self.value_string()
    }

    fn value_string(&self) -> &String {
        self.value
            .get_or_init(|| self.escaped.decode().into_string())
    }

    pub fn escaped(&self) -> &str {
        self.escaped.as_str()
    }

    fn into_value(self) -> String {
        let Self { escaped, value } = self;
        value
            .into_inner()
            .unwrap_or_else(|| escaped.decode().into_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct PropertyMap {
    entries: CaseInsensitiveMap<EscapedValue>,
}

impl PropertyMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, value: String) -> Option<String> {
        self.entries
            .insert(name, EscapedValue::new(value))
            .map(EscapedValue::into_value)
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.entries.get(name).map(EscapedValue::value_string)
    }

    pub fn get_escaped(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(EscapedValue::escaped)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.entries
            .iter()
            .map(|(name, value)| (name, value.value_string()))
    }

    pub fn iter_escaped(&self) -> impl Iterator<Item = (&String, &str)> {
        self.entries
            .iter()
            .map(|(name, value)| (name, value.escaped()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MetadataMap {
    entries: CaseInsensitiveMap<MetadataValue>,
}

#[derive(Debug, Clone)]
struct MetadataValue {
    value: EscapedValue,
}

impl MetadataValue {
    fn new(escaped: String) -> Self {
        Self {
            value: EscapedValue::new(escaped),
        }
    }
}

impl MetadataMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, escaped_value: String) -> Option<String> {
        self.entries
            .insert(name, MetadataValue::new(escaped_value))
            .map(|value| value.value.into_value())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(|value| value.value.value())
    }

    pub fn get_escaped(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(|value| value.value.escaped())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &str)> {
        self.entries
            .iter()
            .map(|(name, value)| (name, value.value.value()))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub item_type: String,
    pub name: String,
    pub(crate) escaped_name: String,
    #[allow(dead_code)] // Kept for object-model consumers that inspect authored item specs.
    pub(crate) spec_kind: ItemSpecKind,
    pub metadata: MetadataMap,
    defaults: Arc<MetadataMap>,
    inherited_defaults: Vec<Arc<MetadataMap>>,
    evaluation_directory: PathBuf,
    filesystem_directory: PathBuf,
    defining_project: PathBuf,
    escaped_recursive_dir: String,
    active: bool,
}

impl Item {
    pub fn new(
        item_type: String,
        escaped_name: String,
        defaults: Arc<MetadataMap>,
        evaluation_directory: PathBuf,
        defining_project: PathBuf,
    ) -> Self {
        Self {
            item_type,
            name: unescape_once(&escaped_name),
            spec_kind: classify_item_spec(&escaped_name),
            escaped_name,
            metadata: MetadataMap::new(),
            defaults,
            inherited_defaults: Vec::new(),
            filesystem_directory: evaluation_directory.clone(),
            evaluation_directory,
            defining_project,
            escaped_recursive_dir: String::new(),
            active: true,
        }
    }

    pub(crate) fn with_filesystem_directory(mut self, directory: PathBuf) -> Self {
        self.filesystem_directory = directory;
        self
    }

    pub(crate) fn with_recursive_dir(mut self, escaped_recursive_dir: String) -> Self {
        self.escaped_recursive_dir = escaped_recursive_dir;
        self
    }

    pub fn set_metadata(&mut self, name: String, escaped_value: String) {
        self.metadata.insert(name, escaped_value);
    }

    pub(crate) fn apply_update_defaults(&mut self, defaults: Arc<MetadataMap>) {
        if !defaults.is_empty()
            && !Arc::ptr_eq(&defaults, &self.defaults)
            && !self
                .inherited_defaults
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &defaults))
        {
            self.inherited_defaults.insert(0, defaults);
        }
    }

    #[cfg(test)]
    pub(crate) fn inherited_default_layer_count(&self) -> usize {
        self.inherited_defaults.len()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn copy_for_type(
        &self,
        item_type: String,
        escaped_name: String,
        defaults: Arc<MetadataMap>,
        defining_project: PathBuf,
        preserve_recursive_dir: bool,
    ) -> Self {
        let mut copy = Self::new(
            item_type,
            escaped_name,
            defaults,
            self.evaluation_directory.clone(),
            defining_project,
        );
        copy.metadata = self.metadata.clone();
        copy.inherited_defaults = self.inherited_defaults.clone();
        copy.inherited_defaults.push(Arc::clone(&self.defaults));
        copy.filesystem_directory = self.filesystem_directory.clone();
        if preserve_recursive_dir {
            copy.escaped_recursive_dir = self.escaped_recursive_dir.clone();
        }
        copy
    }

    pub(crate) fn copy_for_type_without_metadata(
        &self,
        item_type: String,
        escaped_name: String,
        defaults: Arc<MetadataMap>,
        defining_project: PathBuf,
    ) -> Self {
        let mut copy = Self::new(
            item_type,
            escaped_name,
            defaults,
            self.evaluation_directory.clone(),
            defining_project,
        );
        copy.filesystem_directory = self.filesystem_directory.clone();
        copy
    }

    #[allow(dead_code)] // Public object-model query.
    pub fn get_metadata(&self, name: &str) -> Option<Cow<'_, str>> {
        if name.eq_ignore_ascii_case("Identity") {
            return Some(Cow::Borrowed(&self.name));
        }
        self.get_metadata_for_identity(name, &self.name, false)
    }

    pub(crate) fn get_metadata_for_identity<'a>(
        &'a self,
        name: &str,
        identity: &str,
        metadata_cleared: bool,
    ) -> Option<Cow<'a, str>> {
        if name.eq_ignore_ascii_case("Identity") {
            return Some(Cow::Owned(identity.to_string()));
        }
        if !metadata_cleared
            && let Some(value) = self
                .metadata
                .get(name)
                .or_else(|| {
                    self.inherited_defaults
                        .iter()
                        .find_map(|metadata| metadata.get(name))
                })
                .or_else(|| self.defaults.get(name))
        {
            return Some(Cow::Borrowed(value));
        }
        self.well_known_metadata_for_identity(name, identity, metadata_cleared)
            .map(Cow::Owned)
    }

    #[allow(dead_code)] // Internal escaped-value query retained for library extraction.
    pub fn get_metadata_escaped(&self, name: &str) -> Option<Cow<'_, str>> {
        if name.eq_ignore_ascii_case("Identity") {
            return Some(Cow::Borrowed(&self.escaped_name));
        }
        self.get_metadata_for_identity_escaped(name, &self.escaped_name, false)
    }

    pub(crate) fn get_metadata_for_identity_escaped<'a>(
        &'a self,
        name: &str,
        escaped_identity: &str,
        metadata_cleared: bool,
    ) -> Option<Cow<'a, str>> {
        if name.eq_ignore_ascii_case("Identity") {
            return Some(Cow::Owned(escaped_identity.to_string()));
        }
        if !metadata_cleared
            && let Some(value) = self
                .metadata
                .get_escaped(name)
                .or_else(|| {
                    self.inherited_defaults
                        .iter()
                        .find_map(|metadata| metadata.get_escaped(name))
                })
                .or_else(|| self.defaults.get_escaped(name))
        {
            return Some(Cow::Borrowed(value));
        }
        self.well_known_metadata_for_identity(
            name,
            &unescape_once(escaped_identity),
            metadata_cleared,
        )
        .map(|value| Cow::Owned(escape(&value)))
    }

    pub(crate) fn identity_exists(&self, escaped_identity: &str) -> bool {
        let identity = normalized_item_path(&unescape_once(escaped_identity));
        if identity.is_absolute() {
            identity.exists()
        } else {
            self.evaluation_directory.join(identity).exists()
        }
    }

    pub(crate) fn directory_name(&self, escaped_identity: &str) -> String {
        let identity = normalized_item_path(&unescape_once(escaped_identity));
        let full_path = if identity.is_absolute() {
            lexical_absolute(&identity).ok()
        } else {
            lexical_absolute(&self.evaluation_directory.join(identity)).ok()
        };
        escape(
            &full_path
                .as_deref()
                .and_then(Path::parent)
                .map(display_path)
                .unwrap_or_default(),
        )
    }

    pub fn evaluated_metadata(&self) -> CaseInsensitiveMap<String> {
        let mut result = CaseInsensitiveMap::new();
        for (name, value) in self.defaults.iter() {
            result.insert(name.clone(), value.to_string());
        }
        for defaults in self.inherited_defaults.iter().rev() {
            for (name, value) in defaults.iter() {
                result.insert(name.clone(), value.to_string());
            }
        }
        for (name, value) in self.metadata.iter() {
            result.insert(name.clone(), value.to_string());
        }
        let file_times = self.read_file_times(&self.name);
        for name in WELL_KNOWN_METADATA {
            let value = if name.eq_ignore_ascii_case("ModifiedTime") {
                file_times.modified.clone()
            } else if name.eq_ignore_ascii_case("CreatedTime") {
                file_times.created.clone()
            } else if name.eq_ignore_ascii_case("AccessedTime") {
                file_times.accessed.clone()
            } else {
                self.well_known_metadata_for_identity(name, &self.name, false)
                    .unwrap_or_default()
            };
            result.insert((*name).to_string(), value);
        }
        result
    }

    fn well_known_metadata_for_identity(
        &self,
        name: &str,
        identity: &str,
        metadata_cleared: bool,
    ) -> Option<String> {
        if name.eq_ignore_ascii_case("Identity") {
            return Some(identity.to_string());
        }
        if name.eq_ignore_ascii_case("RecursiveDir") {
            return Some(if metadata_cleared {
                String::new()
            } else {
                unescape_once(&self.escaped_recursive_dir)
            });
        }

        let item_path = normalized_item_path(identity);
        let full_path = if item_path.is_absolute() {
            lexical_absolute(&item_path).ok()?
        } else {
            lexical_absolute(&self.evaluation_directory.join(&item_path)).ok()?
        };

        if name.eq_ignore_ascii_case("FullPath") {
            return Some(display_path(&full_path));
        }
        if name.eq_ignore_ascii_case("RootDir") {
            return Some(
                full_path
                    .ancestors()
                    .last()
                    .map(display_path)
                    .map(with_trailing_separator)
                    .unwrap_or_default(),
            );
        }
        if name.eq_ignore_ascii_case("Filename") {
            return Some(
                item_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if name.eq_ignore_ascii_case("Extension") {
            return Some(
                item_path
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
                    .unwrap_or_default(),
            );
        }
        if name.eq_ignore_ascii_case("RelativeDir") {
            return Some(
                item_path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(display_path)
                    .map(with_trailing_separator)
                    .unwrap_or_default(),
            );
        }
        if name.eq_ignore_ascii_case("Directory") {
            let root = full_path
                .ancestors()
                .last()
                .unwrap_or_else(|| Path::new(""));
            return Some(
                full_path
                    .parent()
                    .and_then(|parent| parent.strip_prefix(root).ok())
                    .map(display_path)
                    .map(with_trailing_separator)
                    .unwrap_or_default(),
            );
        }
        if name.eq_ignore_ascii_case("ModifiedTime")
            || name.eq_ignore_ascii_case("CreatedTime")
            || name.eq_ignore_ascii_case("AccessedTime")
        {
            let times = self.read_file_times(identity);
            return Some(if name.eq_ignore_ascii_case("ModifiedTime") {
                times.modified
            } else if name.eq_ignore_ascii_case("CreatedTime") {
                times.created
            } else {
                times.accessed
            });
        }
        if name.eq_ignore_ascii_case("DefiningProjectFullPath") {
            return Some(display_path(&self.defining_project));
        }
        if name.eq_ignore_ascii_case("DefiningProjectDirectory") {
            return Some(
                self.defining_project
                    .parent()
                    .map(display_path)
                    .map(with_trailing_separator)
                    .unwrap_or_default(),
            );
        }
        if name.eq_ignore_ascii_case("DefiningProjectName") {
            return Some(
                self.defining_project
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if name.eq_ignore_ascii_case("DefiningProjectExtension") {
            return Some(
                self.defining_project
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
                    .unwrap_or_default(),
            );
        }
        None
    }

    fn read_file_times(&self, identity: &str) -> FileTimes {
        let item_path = normalized_item_path(identity);
        let path = if item_path.is_absolute() {
            item_path
        } else {
            self.filesystem_directory.join(item_path)
        };
        FileTimes::read(&path)
    }

    pub(crate) fn evaluation_directory(&self) -> &Path {
        &self.evaluation_directory
    }
}

const WELL_KNOWN_METADATA: &[&str] = &[
    "Identity",
    "FullPath",
    "RootDir",
    "Filename",
    "Extension",
    "RelativeDir",
    "Directory",
    "RecursiveDir",
    "ModifiedTime",
    "CreatedTime",
    "AccessedTime",
    "DefiningProjectFullPath",
    "DefiningProjectDirectory",
    "DefiningProjectName",
    "DefiningProjectExtension",
];

pub fn is_well_known_metadata(name: &str) -> bool {
    WELL_KNOWN_METADATA
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn normalized_item_path(value: &str) -> PathBuf {
    if std::path::MAIN_SEPARATOR == '\\' {
        PathBuf::from(value.replace('/', "\\"))
    } else {
        PathBuf::from(value.replace('\\', "/"))
    }
}

fn with_trailing_separator(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with(['/', '\\']) {
        value.push(std::path::MAIN_SEPARATOR);
    }
    value
}

#[derive(Debug, Clone)]
pub struct EvaluatedItemDefinitionMetadata {
    #[allow(dead_code)] // Exposed through the evaluation history API.
    pub item_type: String,
    #[allow(dead_code)] // Exposed through the evaluation history API.
    pub name: String,
    #[allow(dead_code)] // Exposed through the evaluation history API.
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub depends_on: Vec<String>,
    pub condition: Option<String>,
    pub tasks: Vec<Task>,
    pub source_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub attributes: HashMap<String, String>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Import {
    #[allow(dead_code)]
    pub project: String,
    #[allow(dead_code)]
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectModel {
    pub properties: PropertyMap,
    environment: Option<Vec<(String, String)>>,
    pub items: CaseInsensitiveMap<Vec<Item>>,
    item_definitions: CaseInsensitiveMap<Arc<MetadataMap>>,
    all_evaluated_item_definition_metadata: Vec<EvaluatedItemDefinitionMetadata>,
    pub targets: IndexMap<String, Target>,
    initial_targets: Vec<String>,
    pub imports: Vec<Import>,
    pub using_tasks: HashMap<String, String>,
    pub project_file_path: Option<PathBuf>,
    #[cfg(test)]
    identity_index_peak_bucket_len: usize,
    #[cfg(test)]
    identity_index_peak_bucket_capacity: usize,
}

impl ProjectModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_property(&mut self, name: String, value: String) {
        self.properties.insert(name, value);
    }

    pub fn get_property(&self, name: &str) -> Option<&String> {
        self.properties.get(name)
    }

    pub(crate) fn set_environment(&mut self, environment: Vec<(String, String)>) {
        self.environment = Some(environment);
    }

    pub(crate) fn environment(&self) -> Option<&[(String, String)]> {
        self.environment.as_deref()
    }

    pub(crate) fn get_property_escaped(&self, name: &str) -> Option<&str> {
        self.properties.get_escaped(name)
    }

    pub fn add_item(&mut self, item: Item) -> usize {
        if let Some(items) = self.items.get_mut(&item.item_type) {
            let index = items.len();
            items.push(item);
            index
        } else {
            self.items.insert(item.item_type.clone(), vec![item]);
            0
        }
    }

    pub fn get_items(&self, item_type: &str) -> Option<&Vec<Item>> {
        self.items.get(item_type)
    }

    pub(crate) fn get_items_mut(&mut self, item_type: &str) -> Option<&mut Vec<Item>> {
        self.items.get_mut(item_type)
    }

    pub(crate) fn iter_items<'a>(&'a self, item_type: &str) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .get(item_type)
            .into_iter()
            .flat_map(|items| items.iter())
            .filter(|item| item.is_active())
    }

    pub(crate) fn compact_inactive_items(&mut self) {
        for (_, items) in self.items.iter_mut() {
            items.retain(Item::is_active);
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_identity_bucket(&mut self, len: usize, capacity: usize) {
        self.identity_index_peak_bucket_len = self.identity_index_peak_bucket_len.max(len);
        self.identity_index_peak_bucket_capacity =
            self.identity_index_peak_bucket_capacity.max(capacity);
    }

    #[cfg(test)]
    pub(crate) fn identity_index_peak_bucket(&self) -> (usize, usize) {
        (
            self.identity_index_peak_bucket_len,
            self.identity_index_peak_bucket_capacity,
        )
    }

    pub fn item_defaults(&self, item_type: &str) -> Arc<MetadataMap> {
        self.item_definitions
            .get(item_type)
            .cloned()
            .unwrap_or_else(|| Arc::new(MetadataMap::new()))
    }

    #[allow(dead_code)] // Public object-model query used by compatibility consumers.
    pub fn get_item_definition_metadata(&self, item_type: &str, name: &str) -> Option<&str> {
        self.item_definitions
            .get(item_type)
            .and_then(|metadata| metadata.get(name))
    }

    pub(crate) fn get_item_definition_metadata_escaped(
        &self,
        item_type: &str,
        name: &str,
    ) -> Option<&str> {
        self.item_definitions
            .get(item_type)
            .and_then(|metadata| metadata.get_escaped(name))
    }

    pub fn set_item_definition_metadata(
        &mut self,
        item_type: String,
        name: String,
        escaped_value: String,
    ) {
        let definitions = if let Some(definitions) = self.item_definitions.get_mut(&item_type) {
            definitions
        } else {
            self.item_definitions
                .insert(item_type.clone(), Arc::new(MetadataMap::new()));
            self.item_definitions
                .get_mut(&item_type)
                .expect("definition was just inserted")
        };
        Arc::make_mut(definitions).insert(name.clone(), escaped_value.clone());
        self.all_evaluated_item_definition_metadata
            .push(EvaluatedItemDefinitionMetadata {
                item_type,
                name,
                value: unescape_once(&escaped_value),
            });
    }

    #[allow(dead_code)] // Public object-model evaluation history.
    pub fn all_evaluated_item_definition_metadata(&self) -> &[EvaluatedItemDefinitionMetadata] {
        &self.all_evaluated_item_definition_metadata
    }

    pub fn add_target(&mut self, target: Target) {
        self.targets.insert(target.name.clone(), target);
    }

    pub fn get_target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    pub(crate) fn add_initial_targets<I>(&mut self, targets: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.initial_targets.extend(targets);
    }

    pub(crate) fn initial_targets(&self) -> &[String] {
        &self.initial_targets
    }

    pub fn add_import(&mut self, import: Import) {
        self.imports.push(import);
    }

    pub fn add_using_task(&mut self, task_name: String, assembly: String) {
        self.using_tasks.insert(task_name, assembly);
    }

    pub fn set_project_file_path(&mut self, path: PathBuf) {
        self.project_file_path = Some(path);
    }

    pub fn get_project_directory(&self) -> Option<PathBuf> {
        self.project_file_path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_times::{reset_stat_calls, stat_calls};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn escaped_values_decode_lazily() {
        let value = EscapedValue::new("left%3Bright".to_string());

        assert!(value.value.get().is_none());
        assert_eq!(value.value(), "left;right");
        assert!(value.value.get().is_some());
    }

    #[test]
    fn property_lookup_is_case_insensitive_and_decodes_once() {
        let mut model = ProjectModel::new();
        model.set_property("NETCoreSdkVersion".to_string(), "10.0.400".to_string());
        model.set_property("netcoresdkversion".to_string(), "%2512".to_string());

        assert_eq!(
            model.get_property("NetCoreSdkVersion"),
            Some(&"%12".to_string())
        );
        assert_eq!(
            model.get_property_escaped("NETCORESdkVersion"),
            Some("%2512")
        );
        assert_eq!(model.properties.len(), 1);
        assert_eq!(
            model.properties.iter().next().unwrap().0,
            "NETCoreSdkVersion"
        );
    }

    #[test]
    fn metadata_lookup_is_indexed_case_insensitive_and_preserves_spelling() {
        let mut metadata = MetadataMap::new();
        metadata.insert("SENSITIVE".to_string(), "first".to_string());
        metadata.insert("sensitive".to_string(), "second".to_string());

        assert_eq!(metadata.get("SeNsItIvE"), Some("second"));
        assert_eq!(metadata.iter().next().unwrap().0, "SENSITIVE");
    }

    #[test]
    fn timestamp_metadata_is_lazy_fresh_and_bulk_queries_share_one_stat() -> anyhow::Result<()> {
        let directory = TempDir::new()?;
        let path = directory.path().join("timestamp.txt");
        fs::write(&path, "timestamp")?;
        let item = Item::new(
            "I".to_string(),
            "timestamp.txt".to_string(),
            Arc::new(MetadataMap::new()),
            directory.path().join("project-root-is-not-the-time-root"),
            directory.path().join("imports").join("child.props"),
        )
        .with_filesystem_directory(directory.path().to_path_buf());

        reset_stat_calls();
        assert_eq!(item.get_metadata("Filename").as_deref(), Some("timestamp"));
        assert_eq!(stat_calls(), 0);

        let modified = item.get_metadata("ModifiedTime").unwrap();
        assert_eq!(stat_calls(), 1);
        assert_file_time_format(&modified);
        assert_file_time_format(&item.get_metadata("CreatedTime").unwrap());
        assert_file_time_format(&item.get_metadata("AccessedTime").unwrap());
        assert_eq!(stat_calls(), 3);

        let file = fs::OpenOptions::new().write(true).open(&path)?;
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)),
        )?;
        let refreshed = item.get_metadata("ModifiedTime").unwrap();
        assert_ne!(modified, refreshed);
        assert_eq!(stat_calls(), 4);

        reset_stat_calls();
        let evaluated = item.evaluated_metadata();
        assert_eq!(
            evaluated.get("ModifiedTime").map(String::as_str),
            Some(refreshed.as_ref())
        );
        assert_file_time_format(evaluated.get("CreatedTime").unwrap());
        assert_file_time_format(evaluated.get("AccessedTime").unwrap());
        assert_eq!(stat_calls(), 1);

        let missing = Item::new(
            "I".to_string(),
            "missing.txt".to_string(),
            Arc::new(MetadataMap::new()),
            directory.path().join("different-project-root"),
            directory.path().join("project.proj"),
        )
        .with_filesystem_directory(directory.path().to_path_buf());
        assert_eq!(missing.get_metadata("ModifiedTime").as_deref(), Some(""));
        assert_eq!(missing.get_metadata("CreatedTime").as_deref(), Some(""));
        assert_eq!(missing.get_metadata("AccessedTime").as_deref(), Some(""));
        assert_eq!(stat_calls(), 4);
        Ok(())
    }

    fn assert_file_time_format(value: &str) {
        assert_eq!(value.len(), 27, "{value}");
        for index in [4, 7] {
            assert_eq!(value.as_bytes()[index], b'-', "{value}");
        }
        assert_eq!(value.as_bytes()[10], b' ', "{value}");
        for index in [13, 16] {
            assert_eq!(value.as_bytes()[index], b':', "{value}");
        }
        assert_eq!(value.as_bytes()[19], b'.', "{value}");
        assert!(
            value
                .bytes()
                .enumerate()
                .all(|(index, byte)| [4, 7, 10, 13, 16, 19].contains(&index)
                    || byte.is_ascii_digit()),
            "{value}"
        );
    }
}
