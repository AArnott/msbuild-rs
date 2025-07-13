use indexmap::IndexMap;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Property {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub item_type: String,
    pub name: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub depends_on: Vec<String>,
    pub condition: Option<String>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub attributes: HashMap<String, String>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub project: String,
    pub condition: Option<String>,
}

#[derive(Debug, Default)]
pub struct ProjectModel {
    pub properties: IndexMap<String, String>,
    pub items: IndexMap<String, Vec<Item>>,
    pub targets: IndexMap<String, Target>,
    pub imports: Vec<Import>,
    pub using_tasks: HashMap<String, String>, // task name -> assembly
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
            .or_insert_with(Vec::new)
            .push(item);
    }

    pub fn get_items(&self, item_type: &str) -> Option<&Vec<Item>> {
        self.items.get(item_type)
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
            items.iter()
                .map(|item| &item.name)
                .cloned()
                .collect::<Vec<String>>()
                .join(";")
        } else {
            String::new()
        }
    }
}
