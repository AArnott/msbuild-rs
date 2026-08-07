use indexmap::Equivalent;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// An insertion-ordered map with O(1) ASCII case-insensitive lookup.
///
/// Both stored and borrowed keys use the same folded hash, avoiding an
/// allocation on lookup while the spelling from the first definition remains
/// stable.
#[derive(Debug, Clone)]
pub struct PropertyMap {
    entries: IndexMap<PropertyKey, (String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PropertyKey(String);

struct PropertyLookup<'a>(&'a str);

impl Hash for PropertyKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_property_name(&self.0, state);
    }
}

impl Hash for PropertyLookup<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_property_name(self.0, state);
    }
}

impl Equivalent<PropertyKey> for PropertyLookup<'_> {
    fn equivalent(&self, key: &PropertyKey) -> bool {
        self.0.eq_ignore_ascii_case(&key.0)
    }
}

impl PropertyMap {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
        }
    }

    pub fn insert(&mut self, name: String, value: String) -> Option<String> {
        if let Some((_, existing_value)) = self.entries.get_mut(&PropertyLookup(&name)) {
            return Some(std::mem::replace(existing_value, value));
        }
        self.entries
            .insert(PropertyKey(name.clone()), (name, value));
        None
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.entries
            .get(&PropertyLookup(name))
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.entries.contains_key(&PropertyLookup(name))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.entries.values().map(|(name, value)| (name, value))
    }
}

impl Default for PropertyMap {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_property_name<H: Hasher>(name: &str, state: &mut H) {
    state.write_usize(name.len());
    for byte in name.bytes() {
        state.write_u8(byte.to_ascii_lowercase());
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub item_type: String,
    pub name: String,
    #[allow(dead_code)] // Metadata support planned for future implementation
    pub metadata: HashMap<String, String>,
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
    pub items: IndexMap<String, Vec<Item>>,
    pub targets: IndexMap<String, Target>,
    pub imports: Vec<Import>,
    pub using_tasks: HashMap<String, String>, // task name -> assembly
    pub project_file_path: Option<PathBuf>,   // Path to the project file
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

    pub fn add_item(&mut self, item: Item) {
        self.items
            .entry(item.item_type.clone())
            .or_default()
            .push(item);
    }

    pub fn get_items(&self, item_type: &str) -> Option<&Vec<Item>> {
        self.items.get(item_type).or_else(|| {
            self.items
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(item_type))
                .map(|(_, items)| items)
        })
    }

    pub fn add_target(&mut self, target: Target) {
        self.targets.insert(target.name.clone(), target);
    }

    pub fn get_target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    pub fn add_import(&mut self, import: Import) {
        self.imports.push(import);
    }

    pub fn add_using_task(&mut self, task_name: String, assembly: String) {
        self.using_tasks.insert(task_name, assembly);
    }

    pub fn get_all_item_names(&self, item_type: &str) -> String {
        if let Some(items) = self.get_items(item_type) {
            items
                .iter()
                .map(|item| &item.name)
                .cloned()
                .collect::<Vec<String>>()
                .join(";")
        } else {
            String::new()
        }
    }

    pub fn set_project_file_path(&mut self, path: PathBuf) {
        self.project_file_path = Some(path);
    }

    pub fn get_project_directory(&self) -> Option<PathBuf> {
        self.project_file_path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_lookup_is_case_insensitive() {
        let mut model = ProjectModel::new();
        model.set_property("NETCoreSdkVersion".to_string(), "10.0.400".to_string());
        model.set_property("netcoresdkversion".to_string(), "10.0.401".to_string());

        assert_eq!(
            model.get_property("NetCoreSdkVersion"),
            Some(&"10.0.401".to_string())
        );
        assert_eq!(model.properties.len(), 1);
        assert_eq!(
            model.properties.iter().next().unwrap().0,
            "NETCoreSdkVersion"
        );
    }
}
