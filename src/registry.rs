use anyhow::{Result, anyhow, bail};

use crate::escaping::escape;
#[cfg(windows)]
use crate::escaping::unescape_once;

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryView {
    Default,
    Registry32,
    Registry64,
}

impl RegistryView {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        // IntrinsicFunctions strips these two prefixes with case-sensitive
        // String.Replace calls, then asks Enum.Parse to match the leaf
        // case-insensitively.
        let value = value
            .replace("Microsoft.Win32.RegistryView.", "")
            .replace("RegistryView.", "");
        if value.eq_ignore_ascii_case("Default") {
            Ok(Self::Default)
        } else if value.eq_ignore_ascii_case("Registry32") {
            Ok(Self::Registry32)
        } else if value.eq_ignore_ascii_case("Registry64") {
            Ok(Self::Registry64)
        } else if value == "0" {
            Ok(Self::Default)
        } else if value == "512" {
            Ok(Self::Registry32)
        } else if value == "256" {
            Ok(Self::Registry64)
        } else {
            bail!("MSB4184: '{value}' is not a valid Microsoft.Win32.RegistryView value")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryData {
    String(String),
    DWord(i32),
    QWord(i64),
    MultiString(Vec<String>),
    Binary(Vec<u8>),
}

impl RegistryData {
    fn into_scalar_escaped_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::DWord(value) => value.to_string(),
            Self::QWord(value) => value.to_string(),
            Self::MultiString(values) => values
                .into_iter()
                .map(|value| escape(&value))
                .collect::<Vec<_>>()
                .join(";"),
            Self::Binary(values) => values
                .into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(";"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryReadResult {
    KeyMissing,
    ValueMissing,
    Value(RegistryData),
}

pub(crate) fn expand_registry_property(expression: &str) -> Result<String> {
    debug_assert!(
        expression
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Registry:"))
    );

    // Modern .NET MSBuild preserves the historical .NET Core behavior on
    // non-Windows: every Registry: expression evaluates to empty, even one
    // whose key/value delimiter would be malformed on Windows.
    #[cfg(not(windows))]
    {
        let _ = expression;
        return Ok(String::new());
    }

    #[cfg(windows)]
    {
        let location = &expression[9..];
        if location.matches('@').count() > 1 {
            bail!(
                "MSB4184: The registry property expression '$({expression})' is invalid: only one '@' value delimiter is permitted"
            );
        }
        let (key, value_name) = location
            .split_once('@')
            .map_or((location, None), |(key, value)| {
                (key, (!value.is_empty()).then_some(value))
            });
        if key.is_empty() {
            bail!(
                "MSB4184: The registry property expression '$({expression})' has no registry key"
            );
        }
        Ok(
            match read_registry_value(
                &unescape_once(key),
                value_name.map(unescape_once).as_deref(),
                RegistryView::Default,
            )? {
                RegistryReadResult::Value(value) => value.into_scalar_escaped_string(),
                RegistryReadResult::KeyMissing | RegistryReadResult::ValueMissing => String::new(),
            },
        )
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn read_registry_value(
    key: &str,
    value_name: Option<&str>,
    view: RegistryView,
) -> Result<RegistryReadResult> {
    #[cfg(not(windows))]
    {
        let _ = (key, value_name, view);
        Ok(RegistryReadResult::KeyMissing)
    }

    #[cfg(windows)]
    {
        windows::read(key, value_name, view)
    }
}

pub(crate) fn missing_registry_prefix(expression: &str) -> Result<Option<String>> {
    if !expression
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("HKEY_"))
        || !expression.contains('@')
    {
        return Ok(None);
    }

    // Long-standing compatibility exception ported from
    // RegistryPropertyInvalidPrefixSpecialCase.
    if expression.eq_ignore_ascii_case(
        r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@VSTSDBDirectory",
    ) {
        return Ok(Some(String::new()));
    }

    Err(anyhow!(
        "MSB4184: The registry property expression '$({expression})' is missing the required 'Registry:' prefix"
    ))
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::ptr;

    use anyhow::{Result, anyhow, bail};
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        HKEY_PERFORMANCE_DATA, HKEY_USERS, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_BINARY,
        REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_NONE, REG_QWORD, REG_SZ, RRF_RT_ANY,
        RegCloseKey, RegGetValueW, RegOpenKeyExW,
    };

    use super::{RegistryData, RegistryReadResult, RegistryView};

    pub(super) fn read(
        key: &str,
        value_name: Option<&str>,
        view: RegistryView,
    ) -> Result<RegistryReadResult> {
        let (root, subkey) = split_key(key)?;
        // Performance data is a pseudo-hive. The Registry.GetValue and
        // RegistryKey.OpenSubKey paths used by MSBuild report it as missing,
        // while RegOpenKeyExW/RegGetValueW return handle/buffer errors.
        if root == HKEY_PERFORMANCE_DATA {
            return Ok(RegistryReadResult::KeyMissing);
        }
        let opened_key = if subkey.is_empty() {
            None
        } else {
            let subkey = wide(subkey);
            let mut handle = ptr::null_mut();
            let access = KEY_READ
                | match view {
                    RegistryView::Default => 0,
                    RegistryView::Registry32 => KEY_WOW64_32KEY,
                    RegistryView::Registry64 => KEY_WOW64_64KEY,
                };
            // SAFETY: subkey is a live nul-terminated UTF-16 buffer and handle
            // points to writable storage for the returned registry handle.
            let status = unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, access, &mut handle) };
            if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                return Ok(RegistryReadResult::KeyMissing);
            }
            if status != ERROR_SUCCESS {
                return Err(registry_error(key, value_name, status));
            }
            Some(OwnedKey(handle))
        };
        let handle = opened_key.as_ref().map_or(root, |key| key.0);
        let value_name = value_name.map(wide);
        let value_pointer = value_name
            .as_ref()
            .map_or(ptr::null(), |value| value.as_ptr());
        let mut kind = 0;
        let mut byte_count = 0;
        // SAFETY: all pointers are either null or point to live, nul-terminated
        // UTF-16 buffers; the first call only asks Windows for the required size.
        let status = unsafe {
            RegGetValueW(
                handle,
                ptr::null(),
                value_pointer,
                RRF_RT_ANY,
                &mut kind,
                ptr::null_mut(),
                &mut byte_count,
            )
        };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(RegistryReadResult::ValueMissing);
        }
        if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
            return Err(registry_error_wide(key, value_name.as_deref(), status));
        }

        let mut bytes = vec![0u8; byte_count as usize];
        // A zero-byte registry value is legal. RegGetValueW still accepts a
        // null data pointer for it.
        let data_pointer = if bytes.is_empty() {
            ptr::null_mut()
        } else {
            bytes.as_mut_ptr().cast::<c_void>()
        };
        // SAFETY: data_pointer references byte_count writable bytes and the
        // key/value pointers remain live for the duration of the call.
        let status = unsafe {
            RegGetValueW(
                handle,
                ptr::null(),
                value_pointer,
                RRF_RT_ANY,
                &mut kind,
                data_pointer,
                &mut byte_count,
            )
        };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(RegistryReadResult::ValueMissing);
        }
        if status != ERROR_SUCCESS {
            return Err(registry_error_wide(key, value_name.as_deref(), status));
        }
        bytes.truncate(byte_count as usize);
        decode(kind, &bytes).map(RegistryReadResult::Value)
    }

    fn split_key(key: &str) -> Result<(HKEY, &str)> {
        let (root_name, subkey) = key.split_once('\\').unwrap_or((key, ""));
        let root = if root_name.eq_ignore_ascii_case("HKEY_CURRENT_USER") {
            HKEY_CURRENT_USER
        } else if root_name.eq_ignore_ascii_case("HKEY_LOCAL_MACHINE") {
            HKEY_LOCAL_MACHINE
        } else if root_name.eq_ignore_ascii_case("HKEY_CLASSES_ROOT") {
            HKEY_CLASSES_ROOT
        } else if root_name.eq_ignore_ascii_case("HKEY_USERS") {
            HKEY_USERS
        } else if root_name.eq_ignore_ascii_case("HKEY_PERFORMANCE_DATA") {
            HKEY_PERFORMANCE_DATA
        } else if root_name.eq_ignore_ascii_case("HKEY_CURRENT_CONFIG") {
            HKEY_CURRENT_CONFIG
        } else {
            bail!("MSB4184: '{root_name}' is not a supported registry hive")
        };
        Ok((root, subkey))
    }

    fn decode(kind: u32, bytes: &[u8]) -> Result<RegistryData> {
        match kind {
            REG_SZ | REG_EXPAND_SZ => Ok(RegistryData::String(
                decode_wide(bytes)?
                    .split('\0')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            )),
            REG_DWORD if bytes.len() >= 4 => Ok(RegistryData::DWord(i32::from_le_bytes(
                bytes[..4].try_into().expect("length was checked"),
            ))),
            REG_QWORD if bytes.len() >= 8 => Ok(RegistryData::QWord(i64::from_le_bytes(
                bytes[..8].try_into().expect("length was checked"),
            ))),
            REG_MULTI_SZ => {
                let decoded = decode_wide(bytes)?;
                Ok(RegistryData::MultiString(
                    decoded
                        .split('\0')
                        .take_while(|value| !value.is_empty())
                        .map(ToString::to_string)
                        .collect(),
                ))
            }
            REG_BINARY | REG_NONE => Ok(RegistryData::Binary(bytes.to_vec())),
            _ => Err(anyhow!(
                "MSB4184: Registry value kind {kind} is not supported"
            )),
        }
    }

    fn decode_wide(bytes: &[u8]) -> Result<String> {
        if !bytes.len().is_multiple_of(2) {
            bail!("MSB4184: Registry string data has an odd byte length");
        }
        let units = bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units)
            .map_err(|error| anyhow!("MSB4184: Registry string is not valid UTF-16: {error}"))
    }

    fn registry_error(key: &str, value_name: Option<&str>, status: u32) -> anyhow::Error {
        anyhow!(
            "MSB4184: Could not read registry value '{}' from '{key}': {}",
            value_name.unwrap_or_default(),
            std::io::Error::from_raw_os_error(status as i32)
        )
    }

    fn registry_error_wide(key: &str, value_name: Option<&[u16]>, status: u32) -> anyhow::Error {
        let value_name = value_name
            .map(|value| {
                String::from_utf16_lossy(value)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_default();
        anyhow!(
            "MSB4184: Could not read registry value '{value_name}' from '{key}': {}",
            std::io::Error::from_raw_os_error(status as i32)
        )
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    struct OwnedKey(HKEY);

    impl Drop for OwnedKey {
        fn drop(&mut self) {
            // SAFETY: this handle was returned by RegOpenKeyExW and is owned by
            // this guard.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_scalar_values_use_msbuild_conversions_and_list_boundaries() {
        assert_eq!(
            RegistryData::String("String".to_string()).into_scalar_escaped_string(),
            "String"
        );
        assert_eq!(
            RegistryData::DWord(123456).into_scalar_escaped_string(),
            "123456"
        );
        assert_eq!(
            RegistryData::QWord(123456789123456789).into_scalar_escaped_string(),
            "123456789123456789"
        );
        assert_eq!(
            RegistryData::MultiString(vec!["A;X".into(), "B".into()]).into_scalar_escaped_string(),
            "A%3BX;B"
        );
        assert_eq!(
            RegistryData::Binary(b"String".to_vec()).into_scalar_escaped_string(),
            "83;116;114;105;110;103"
        );
    }

    #[test]
    fn registry_view_parsing_matches_enum_names_numbers_and_prefix_rules() {
        for (value, expected) in [
            ("Default", RegistryView::Default),
            ("default", RegistryView::Default),
            ("RegistryView.Default", RegistryView::Default),
            (
                "Microsoft.Win32.RegistryView.Registry32",
                RegistryView::Registry32,
            ),
            ("Registry64", RegistryView::Registry64),
            ("0", RegistryView::Default),
            ("512", RegistryView::Registry32),
            ("256", RegistryView::Registry64),
        ] {
            assert_eq!(RegistryView::parse(value).unwrap(), expected, "{value}");
        }
        for value in [
            "registryview.default",
            "microsoft.win32.registryview.default",
            "513",
            "-1",
            "Bogus",
        ] {
            assert!(RegistryView::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn invalid_prefix_special_case_and_errors_match_upstream() {
        assert_eq!(
            missing_registry_prefix(
                r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@VSTSDBDirectory"
            )
            .unwrap(),
            Some(String::new())
        );
        for expression in [
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@XXXXDBDirectory",
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@VSTSDBDirectoryX",
        ] {
            let error = missing_registry_prefix(expression).unwrap_err().to_string();
            assert!(error.contains("Registry:"), "{expression}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn registry_properties_are_empty_on_non_windows_even_when_malformed() {
        assert_eq!(
            expand_registry_property("Registry:HKEY_CURRENT_USER\\X@A@B").unwrap(),
            ""
        );
    }

    #[cfg(windows)]
    mod windows_registry {
        use std::fs;
        use std::ptr;

        use tempfile::TempDir;
        use windows_sys::Win32::Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
        };
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD, REG_EXPAND_SZ,
            REG_MULTI_SZ, REG_NONE, REG_QWORD, REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteTreeW,
            RegOpenKeyExW, RegSetValueExW,
        };

        use super::*;
        use crate::evaluation::ProjectEvaluator;

        struct TestKey {
            parent: HKEY,
            leaf: String,
            handle: HKEY,
        }

        impl TestKey {
            fn new() -> Self {
                let parent_name = wide("Software");
                let mut parent = ptr::null_mut();
                // SAFETY: the parent name is nul-terminated and parent points
                // to writable handle storage. Tests require this existing,
                // user-writable HKCU parent and never create a system key.
                let status = unsafe {
                    RegOpenKeyExW(
                        HKEY_CURRENT_USER,
                        parent_name.as_ptr(),
                        0,
                        KEY_READ | KEY_WRITE,
                        &mut parent,
                    )
                };
                assert_eq!(status, ERROR_SUCCESS, "HKCU\\Software must exist");

                let leaf = format!("MSBuild_rs_property_functions_{}", uuid::Uuid::new_v4());
                let wide_leaf = wide(&leaf);
                let mut handle = ptr::null_mut();
                // SAFETY: parent is the opened HKCU\Software key, wide_leaf is
                // nul-terminated, and handle points to writable storage.
                let status = unsafe { RegCreateKeyW(parent, wide_leaf.as_ptr(), &mut handle) };
                assert_eq!(status, ERROR_SUCCESS);
                Self {
                    parent,
                    leaf,
                    handle,
                }
            }

            fn set(&self, name: Option<&str>, kind: u32, bytes: &[u8]) {
                let name = name.map(wide);
                let name_pointer = name.as_ref().map_or(ptr::null(), |name| name.as_ptr());
                // SAFETY: self.handle remains open, name is nul-terminated, and
                // bytes is live for this test-only HKCU write.
                let status = unsafe {
                    RegSetValueExW(
                        self.handle,
                        name_pointer,
                        0,
                        kind,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                    )
                };
                assert_eq!(status, ERROR_SUCCESS);
            }

            fn registry_path(&self) -> String {
                format!(r"HKEY_CURRENT_USER\Software\{}", self.leaf)
            }

            fn expression(&self, value_name: &str) -> String {
                format!("Registry:{}@{value_name}", self.registry_path())
            }
        }

        impl Drop for TestKey {
            fn drop(&mut self) {
                let wide_leaf = wide(&self.leaf);
                // SAFETY: both handles are owned by this guard. The child is
                // closed before its unique tree is deleted from HKCU\Software.
                unsafe {
                    assert_eq!(RegCloseKey(self.handle), ERROR_SUCCESS);
                    assert_eq!(
                        RegDeleteTreeW(self.parent, wide_leaf.as_ptr()),
                        ERROR_SUCCESS,
                        "failed to remove HKCU\\Software\\{}",
                        self.leaf
                    );
                    let mut deleted = ptr::null_mut();
                    let status =
                        RegOpenKeyExW(self.parent, wide_leaf.as_ptr(), 0, KEY_READ, &mut deleted);
                    if status == ERROR_SUCCESS {
                        RegCloseKey(deleted);
                    }
                    assert!(
                        matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND),
                        "registry test key still exists after cleanup: {} (status {status})",
                        self.leaf
                    );
                    assert_eq!(RegCloseKey(self.parent), ERROR_SUCCESS);
                }
            }
        }

        fn wide(value: &str) -> Vec<u16> {
            value.encode_utf16().chain(Some(0)).collect()
        }

        fn wide_bytes(value: &str, extra_terminator: bool) -> Vec<u8> {
            value
                .encode_utf16()
                .chain(Some(0))
                .chain(extra_terminator.then_some(0))
                .flat_map(u16::to_le_bytes)
                .collect()
        }

        fn assert_value(kind: u32, bytes: &[u8], expected: &str) {
            let key = TestKey::new();
            key.set(Some("Value"), kind, bytes);
            assert_eq!(
                expand_registry_property(&key.expression("Value")).unwrap(),
                expected
            );
        }

        #[test]
        fn registry_property_string() {
            assert_value(REG_SZ, &wide_bytes("String", false), "String");
        }

        #[test]
        fn registry_property_binary() {
            assert_value(REG_BINARY, b"String", "83;116;114;105;110;103");
        }

        #[test]
        fn registry_property_none_uses_byte_list_semantics() {
            assert_value(REG_NONE, &[1, 2, 3], "1;2;3");
        }

        #[test]
        fn registry_property_dword() {
            assert_value(REG_DWORD, &123456i32.to_le_bytes(), "123456");
        }

        #[test]
        fn registry_property_expand_string() {
            let expected = std::env::var("TEMP").expect("Windows test process has TEMP");
            assert_value(REG_EXPAND_SZ, &wide_bytes("%TEMP%", false), &expected);
        }

        #[test]
        fn registry_property_qword() {
            assert_value(
                REG_QWORD,
                &123456789123456789i64.to_le_bytes(),
                "123456789123456789",
            );
        }

        #[test]
        fn registry_property_multi_string() {
            assert_value(REG_MULTI_SZ, &wide_bytes("A\0B\0C\0D", true), "A;B;C;D");
        }

        #[test]
        fn registry_reader_distinguishes_missing_keys_values_and_supported_hives() {
            let key = TestKey::new();
            assert_eq!(
                read_registry_value(
                    &format!(r"{}\Missing", key.registry_path()),
                    Some("Value"),
                    RegistryView::Default,
                )
                .unwrap(),
                RegistryReadResult::KeyMissing
            );
            assert_eq!(
                read_registry_value(&key.registry_path(), Some("Missing"), RegistryView::Default,)
                    .unwrap(),
                RegistryReadResult::ValueMissing
            );

            let missing = format!("MSBuild_rs_missing_{}", uuid::Uuid::new_v4());
            for hive in [
                "HKEY_CURRENT_USER",
                "HKEY_LOCAL_MACHINE",
                "HKEY_CLASSES_ROOT",
                "HKEY_USERS",
                "HKEY_PERFORMANCE_DATA",
                "HKEY_CURRENT_CONFIG",
            ] {
                assert_eq!(
                    read_registry_value(
                        &format!(r"{hive}\{missing}"),
                        Some("Value"),
                        RegistryView::Default,
                    )
                    .unwrap(),
                    RegistryReadResult::KeyMissing,
                    "{hive}"
                );
            }
            assert_eq!(
                read_registry_value(
                    "HKEY_PERFORMANCE_DATA",
                    Some("Global"),
                    RegistryView::Default,
                )
                .unwrap(),
                RegistryReadResult::KeyMissing
            );
            for alias in ["HKCU", "HKLM", "HKCR", "HKU", "HKCC", "HKEY_DYN_DATA"] {
                assert!(
                    read_registry_value(
                        &format!(r"{alias}\{missing}"),
                        Some("Value"),
                        RegistryView::Default,
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("not a supported registry hive"),
                    "{alias}"
                );
            }
        }

        #[test]
        fn typed_registry_functions_and_scalar_item_boundaries_match_msbuild() {
            let key = TestKey::new();
            key.set(Some("StringSemi"), REG_SZ, &wide_bytes("A;B", false));
            key.set(Some("Dword"), REG_DWORD, &42i32.to_le_bytes());
            key.set(Some("DwordSigned"), REG_DWORD, &(-1i32).to_le_bytes());
            key.set(Some("Qword"), REG_QWORD, &42i64.to_le_bytes());
            key.set(Some("QwordSigned"), REG_QWORD, &(-1i64).to_le_bytes());
            key.set(Some("Multi"), REG_MULTI_SZ, &wide_bytes("A;X\0B", true));
            key.set(Some("Binary"), REG_BINARY, &[1, 2, 3]);
            key.set(Some("None"), REG_NONE, &[1, 2, 3]);
            key.set(Some("Expand"), REG_EXPAND_SZ, &wide_bytes("%TEMP%", false));
            key.set(None, REG_EXPAND_SZ, &wide_bytes("%TEMP%", false));
            let expanded = std::env::var("TEMP").expect("Windows test process has TEMP");

            let path = key.registry_path();
            let directory = TempDir::new().unwrap();
            let project = directory.path().join("registry.proj");
            fs::write(
                &project,
                format!(
                    r#"<Project>
  <PropertyGroup>
    <DwordCompare>$([MSBuild]::GetRegistryValue('{path}', 'Dword').CompareTo(100))</DwordCompare>
    <DwordSigned>$([MSBuild]::GetRegistryValue('{path}', 'DwordSigned'))</DwordSigned>
    <QwordCompare>$([MSBuild]::GetRegistryValue('{path}', 'Qword').CompareTo(100))</QwordCompare>
    <QwordSigned>$([MSBuild]::GetRegistryValue('{path}', 'QwordSigned'))</QwordSigned>
    <MultiLength>$([MSBuild]::GetRegistryValue('{path}', 'Multi').Length)</MultiLength>
    <MultiFirst>$([MSBuild]::GetRegistryValue('{path}', 'Multi').GetValue(0))</MultiFirst>
    <BinaryLength>$([MSBuild]::GetRegistryValue('{path}', 'Binary').Length)</BinaryLength>
    <BinarySecond>$([MSBuild]::GetRegistryValue('{path}', 'Binary').GetValue(1))</BinarySecond>
    <BinaryFirstCompare>$([MSBuild]::GetRegistryValue('{path}', 'Binary')[0].CompareTo(2))</BinaryFirstCompare>
    <BinaryFirstEquals>$([MSBuild]::GetRegistryValue('{path}', 'Binary')[0].Equals(1))</BinaryFirstEquals>
    <BinaryFirstHex>$([MSBuild]::GetRegistryValue('{path}', 'Binary')[0].ToString('X2'))</BinaryFirstHex>
    <NoneLength>$([MSBuild]::GetRegistryValue('{path}', 'None').Length)</NoneLength>
    <ExpandedValue>$([MSBuild]::GetRegistryValue('{path}', 'Expand'))</ExpandedValue>
    <DefaultValue>$([MSBuild]::GetRegistryValue('{path}', null))</DefaultValue>
    <MissingValue>$([MSBuild]::GetRegistryValue('{path}', 'Missing', 'FALLBACK'))</MissingValue>
    <MissingKey>$([MSBuild]::GetRegistryValue('{path}\Missing', 'Missing', 'FALLBACK'))</MissingKey>
    <TypedMissingValueDefault>$([MSBuild]::GetRegistryValue('{path}', 'Missing', $([System.Int32]::Parse('42'))).CompareTo(100))</TypedMissingValueDefault>
    <ViewMissingValue>$([MSBuild]::GetRegistryValueFromView('{path}', 'Missing', 'FALLBACK', RegistryView.Default))</ViewMissingValue>
    <ViewMissingKey>$([MSBuild]::GetRegistryValueFromView('{path}\Missing', 'Missing', 'FALLBACK', RegistryView.Default))</ViewMissingKey>
    <TypedViewDefault>$([MSBuild]::GetRegistryValueFromView('{path}\Missing', 'Missing', $([System.Int32]::Parse('42')), Default).CompareTo(100))</TypedViewDefault>
    <TypedViewIgnored>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', $([System.Int32]::Parse('256'))))</TypedViewIgnored>
    <ViewOmitted>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK'))</ViewOmitted>
    <ViewDefaultValue>$([MSBuild]::GetRegistryValueFromView('{path}', null, 'FALLBACK', 0))</ViewDefaultValue>
    <View0>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', 0))</View0>
    <View256>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', 256))</View256>
    <View512>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', 512))</View512>
    <ViewNamed>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', RegistryView.Default))</ViewNamed>
    <ViewFullName>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', Microsoft.Win32.RegistryView.Default))</ViewFullName>
    <ViewStopsBeforeInvalid>$([MSBuild]::GetRegistryValueFromView('{path}', 'Dword', 'FALLBACK', Default, Bogus))</ViewStopsBeforeInvalid>
    <ViewNamedDefault>$([MSBuild]::GetRegistryValueFromView('{path}', null, null, RegistryView.Default, RegistryView.Default))</ViewNamedDefault>
    <ViewFullDefault>$([MSBuild]::GetRegistryValueFromView('{path}', null, null, Microsoft.Win32.RegistryView.Default))</ViewFullDefault>
  </PropertyGroup>
  <ItemGroup>
    <ScalarString Include="$(Registry:{path}@StringSemi)" />
    <FunctionString Include="$([MSBuild]::GetRegistryValue('{path}', 'StringSemi'))" />
    <ScalarMulti Include="$(Registry:{path}@Multi)" />
    <FunctionMulti Include="$([MSBuild]::GetRegistryValue('{path}', 'Multi'))" />
    <Binary Include="$([MSBuild]::GetRegistryValue('{path}', 'Binary'))" />
    <None Include="$([MSBuild]::GetRegistryValue('{path}', 'None'))" />
  </ItemGroup>
</Project>"#
                ),
            )
            .unwrap();

            let mut evaluator = ProjectEvaluator::new();
            evaluator.load_project(&project).unwrap();
            let model = evaluator.get_model();
            for (name, expected) in [
                ("DwordCompare", "-1"),
                ("DwordSigned", "-1"),
                ("QwordCompare", "-1"),
                ("QwordSigned", "-1"),
                ("MultiLength", "2"),
                ("MultiFirst", "A;X"),
                ("BinaryLength", "3"),
                ("BinarySecond", "2"),
                ("BinaryFirstCompare", "-1"),
                ("BinaryFirstEquals", "True"),
                ("BinaryFirstHex", "01"),
                ("NoneLength", "3"),
                ("ExpandedValue", expanded.as_str()),
                ("DefaultValue", expanded.as_str()),
                ("MissingValue", "FALLBACK"),
                ("MissingKey", ""),
                ("TypedMissingValueDefault", "-1"),
                ("ViewMissingValue", ""),
                ("ViewMissingKey", "FALLBACK"),
                ("TypedViewDefault", "-1"),
                ("TypedViewIgnored", "FALLBACK"),
                ("ViewOmitted", "FALLBACK"),
                ("ViewDefaultValue", expanded.as_str()),
                ("View0", "42"),
                ("View256", "42"),
                ("View512", "42"),
                ("ViewNamed", "42"),
                ("ViewFullName", "42"),
                ("ViewStopsBeforeInvalid", "42"),
                ("ViewNamedDefault", expanded.as_str()),
                ("ViewFullDefault", expanded.as_str()),
            ] {
                assert_eq!(
                    model.get_property(name).map(String::as_str),
                    Some(expected),
                    "{name}"
                );
            }

            let identities = |item_type: &str| {
                model
                    .get_items(item_type)
                    .unwrap()
                    .iter()
                    .map(|item| item.name.as_str())
                    .collect::<Vec<_>>()
            };
            assert_eq!(identities("ScalarString"), ["A", "B"]);
            assert_eq!(identities("FunctionString"), ["A;B"]);
            assert_eq!(identities("ScalarMulti"), ["A;X", "B"]);
            assert_eq!(identities("FunctionMulti"), ["A;X", "B"]);
            assert_eq!(identities("Binary"), ["1", "2", "3"]);
            assert_eq!(identities("None"), ["1", "2", "3"]);
        }

        #[test]
        fn malformed_registry_property_is_an_error_on_windows() {
            let error = expand_registry_property("Registry:HKEY_CURRENT_USER\\X@A@B")
                .unwrap_err()
                .to_string();
            assert!(error.contains("only one '@'"));
        }
    }
}
