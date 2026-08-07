use std::path::{Component, Path, PathBuf};

use crate::object_model::ProjectModel;

const RESERVED_PROPERTIES: &[&str] = &[
    "MSBuildProjectDirectory",
    "MSBuildProjectDirectoryNoRoot",
    "MSBuildProjectFile",
    "MSBuildProjectExtension",
    "MSBuildProjectFullPath",
    "MSBuildProjectName",
    "MSBuildThisFile",
    "MSBuildThisFileDirectory",
    "MSBuildThisFileDirectoryNoRoot",
    "MSBuildThisFileExtension",
    "MSBuildThisFileFullPath",
    "MSBuildThisFileName",
    "MSBuildBinPath",
    "MSBuildProjectDefaultTargets",
    "MSBuildToolsPath",
    "MSBuildToolsVersion",
    "MSBuildRuntimeType",
    "MSBuildStartupDirectory",
    "MSBuildNodeCount",
    "MSBuildLastTaskResult",
    "MSBuildProgramFiles32",
    "MSBuildAssemblyVersion",
    "MSBuildVersion",
    "MSBuildInteractive",
    "MSBuildDisableFeaturesFromVersion",
];

const THIS_FILE_PROPERTIES: &[&str] = &[
    "MSBuildThisFile",
    "MSBuildThisFileDirectory",
    "MSBuildThisFileDirectoryNoRoot",
    "MSBuildThisFileExtension",
    "MSBuildThisFileFullPath",
    "MSBuildThisFileName",
];

pub(crate) fn is_reserved_property(name: &str) -> bool {
    RESERVED_PROPERTIES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

pub(crate) fn set_reserved_project_properties(model: &mut ProjectModel, path: &Path) {
    let full_path = absolute_path(path);
    let directory = full_path.parent().unwrap_or_else(|| Path::new(""));

    set(model, "MSBuildProjectFullPath", display_path(&full_path));
    set(model, "MSBuildProjectDirectory", display_path(directory));
    set(
        model,
        "MSBuildProjectDirectoryNoRoot",
        directory_without_root(directory, false),
    );
    set(
        model,
        "MSBuildProjectFile",
        full_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );
    set(model, "MSBuildProjectExtension", extension(&full_path));
    set(
        model,
        "MSBuildProjectName",
        full_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );
}

pub(crate) fn this_file_property(name: &str, path: &Path) -> Option<String> {
    if !THIS_FILE_PROPERTIES
        .iter()
        .any(|property| property.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let full_path = absolute_path(path);
    let directory = full_path.parent().unwrap_or_else(|| Path::new(""));
    if name.eq_ignore_ascii_case("MSBuildThisFile") {
        Some(
            full_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        )
    } else if name.eq_ignore_ascii_case("MSBuildThisFileDirectory") {
        Some(with_trailing_separator(display_path(directory)))
    } else if name.eq_ignore_ascii_case("MSBuildThisFileDirectoryNoRoot") {
        Some(directory_without_root(directory, true))
    } else if name.eq_ignore_ascii_case("MSBuildThisFileExtension") {
        Some(extension(&full_path))
    } else if name.eq_ignore_ascii_case("MSBuildThisFileFullPath") {
        Some(display_path(&full_path))
    } else if name.eq_ignore_ascii_case("MSBuildThisFileName") {
        Some(
            full_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    let display = path.display().to_string();
    if let Some(path) = display.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        display
            .strip_prefix(r"\\?\")
            .unwrap_or(&display)
            .to_string()
    }
}

pub(crate) fn lexical_absolute(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn set(model: &mut ProjectModel, name: &str, value: String) {
    model.set_property(name.to_string(), value);
}

fn absolute_path(path: &Path) -> PathBuf {
    lexical_absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default()
}

fn with_trailing_separator(mut value: String) -> String {
    if !value.ends_with(['/', '\\']) {
        value.push(std::path::MAIN_SEPARATOR);
    }
    value
}

fn directory_without_root(path: &Path, trailing_separator: bool) -> String {
    let mut relative = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(component) = component {
            relative.push(component);
        }
    }
    let value = display_path(&relative);
    if trailing_separator && !value.is_empty() {
        with_trailing_separator(value)
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_properties_include_directory_without_root() {
        let mut model = ProjectModel::new();
        let path = if cfg!(windows) {
            Path::new(r"C:\src\example\app.csproj")
        } else {
            Path::new("/src/example/app.csproj")
        };
        set_reserved_project_properties(&mut model, path);

        assert_eq!(
            model
                .get_property("MSBuildProjectDirectoryNoRoot")
                .map(String::as_str),
            Some(if cfg!(windows) {
                r"src\example"
            } else {
                "src/example"
            })
        );
        assert_eq!(
            model
                .get_property("MSBuildProjectExtension")
                .map(String::as_str),
            Some(".csproj")
        );
    }

    #[test]
    fn reserved_property_set_matches_upstream_built_in_only_names() {
        for name in [
            "MSBuildProjectDirectory",
            "MSBuildProjectDirectoryNoRoot",
            "MSBuildProjectFile",
            "MSBuildProjectExtension",
            "MSBuildProjectFullPath",
            "MSBuildProjectName",
            "MSBuildThisFileDirectory",
            "MSBuildThisFileDirectoryNoRoot",
            "MSBuildThisFile",
            "MSBuildThisFileExtension",
            "MSBuildThisFileFullPath",
            "MSBuildThisFileName",
            "MSBuildBinPath",
            "MSBuildProjectDefaultTargets",
            "MSBuildToolsPath",
            "MSBuildToolsVersion",
            "MSBuildRuntimeType",
            "MSBuildStartupDirectory",
            "MSBuildNodeCount",
            "MSBuildLastTaskResult",
            "MSBuildProgramFiles32",
            "MSBuildAssemblyVersion",
            "MSBuildVersion",
            "MSBuildInteractive",
            "MSBuildDisableFeaturesFromVersion",
        ] {
            assert!(is_reserved_property(name), "{name}");
        }

        assert!(!is_reserved_property("MSBuildSDKsPath"));
        assert!(!is_reserved_property("MSBuildExtensionsPath"));
    }
}
