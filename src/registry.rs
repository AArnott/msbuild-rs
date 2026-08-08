#[cfg(windows)]
use anyhow::bail;
use anyhow::{Result, anyhow};

#[cfg(windows)]
use crate::escaping::unescape_once;

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryView {
    Default,
    Registry32,
    Registry64,
}

#[cfg(windows)]
impl RegistryView {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let value = if let Some((prefix, leaf)) = value.rsplit_once('.') {
            if !prefix.eq_ignore_ascii_case("RegistryView")
                && !prefix.eq_ignore_ascii_case("Microsoft.Win32.RegistryView")
            {
                bail!("MSB4184: '{value}' is not a valid Microsoft.Win32.RegistryView value");
            }
            leaf
        } else {
            value
        };
        if value.eq_ignore_ascii_case("Default") {
            Ok(Self::Default)
        } else if value.eq_ignore_ascii_case("Registry32") {
            Ok(Self::Registry32)
        } else if value.eq_ignore_ascii_case("Registry64") {
            Ok(Self::Registry64)
        } else {
            bail!("MSB4184: '{value}' is not a valid Microsoft.Win32.RegistryView value")
        }
    }
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryData {
    String(String),
    DWord(i32),
    QWord(i64),
    MultiString(Vec<String>),
    Binary(Vec<u8>),
}

#[cfg(any(windows, test))]
impl RegistryData {
    fn into_msbuild_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::DWord(value) => value.to_string(),
            Self::QWord(value) => value.to_string(),
            Self::MultiString(values) => values.join(";"),
            Self::Binary(values) => values
                .into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(";"),
        }
    }
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
        Ok(read_registry_value(
            &unescape_once(key),
            value_name.map(unescape_once).as_deref(),
            &[RegistryView::Default],
        )?
        .unwrap_or_default())
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn read_registry_value(
    key: &str,
    value_name: Option<&str>,
    views: &[RegistryView],
) -> Result<Option<String>> {
    #[cfg(not(windows))]
    {
        let _ = (key, value_name, views);
        Ok(None)
    }

    #[cfg(windows)]
    {
        let views = if views.is_empty() {
            &[RegistryView::Default][..]
        } else {
            views
        };
        for view in views {
            if let Some(value) = windows::read(key, value_name, *view)? {
                return Ok(Some(value.into_msbuild_string()));
            }
        }
        Ok(None)
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
        HKEY_USERS, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_QWORD, REG_SZ,
        RRF_RT_ANY, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };

    use super::{RegistryData, RegistryView};

    pub(super) fn read(
        key: &str,
        value_name: Option<&str>,
        view: RegistryView,
    ) -> Result<Option<RegistryData>> {
        let (root, subkey) = split_key(key)?;
        let subkey = wide(subkey);
        let value_name = value_name.map(wide);
        let value_pointer = value_name
            .as_ref()
            .map_or(ptr::null(), |value| value.as_ptr());
        let flags = RRF_RT_ANY
            | match view {
                RegistryView::Default => 0,
                RegistryView::Registry32 => RRF_SUBKEY_WOW6432KEY,
                RegistryView::Registry64 => RRF_SUBKEY_WOW6464KEY,
            };
        let mut kind = 0;
        let mut byte_count = 0;
        // SAFETY: all pointers are either null or point to live, nul-terminated
        // UTF-16 buffers; the first call only asks Windows for the required size.
        let status = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value_pointer,
                flags,
                &mut kind,
                ptr::null_mut(),
                &mut byte_count,
            )
        };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
            return Err(registry_error(key, value_name.as_deref(), status));
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
                root,
                subkey.as_ptr(),
                value_pointer,
                flags,
                &mut kind,
                data_pointer,
                &mut byte_count,
            )
        };
        if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(registry_error(key, value_name.as_deref(), status));
        }
        bytes.truncate(byte_count as usize);
        decode(kind, &bytes).map(Some)
    }

    fn split_key(key: &str) -> Result<(HKEY, &str)> {
        let (root_name, subkey) = key.split_once('\\').unwrap_or((key, ""));
        let root = if root_name.eq_ignore_ascii_case("HKEY_CURRENT_USER")
            || root_name.eq_ignore_ascii_case("HKCU")
        {
            HKEY_CURRENT_USER
        } else if root_name.eq_ignore_ascii_case("HKEY_LOCAL_MACHINE")
            || root_name.eq_ignore_ascii_case("HKLM")
        {
            HKEY_LOCAL_MACHINE
        } else if root_name.eq_ignore_ascii_case("HKEY_CLASSES_ROOT")
            || root_name.eq_ignore_ascii_case("HKCR")
        {
            HKEY_CLASSES_ROOT
        } else if root_name.eq_ignore_ascii_case("HKEY_USERS")
            || root_name.eq_ignore_ascii_case("HKU")
        {
            HKEY_USERS
        } else if root_name.eq_ignore_ascii_case("HKEY_CURRENT_CONFIG")
            || root_name.eq_ignore_ascii_case("HKCC")
        {
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
            REG_BINARY => Ok(RegistryData::Binary(bytes.to_vec())),
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

    fn registry_error(key: &str, value_name: Option<&[u16]>, status: u32) -> anyhow::Error {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_registry_values_use_msbuild_conversions() {
        assert_eq!(
            RegistryData::String("String".to_string()).into_msbuild_string(),
            "String"
        );
        assert_eq!(RegistryData::DWord(123456).into_msbuild_string(), "123456");
        assert_eq!(
            RegistryData::QWord(123456789123456789).into_msbuild_string(),
            "123456789123456789"
        );
        assert_eq!(
            RegistryData::MultiString(vec!["A".into(), "B".into(), "C".into(), "D".into()])
                .into_msbuild_string(),
            "A;B;C;D"
        );
        assert_eq!(
            RegistryData::Binary(b"String".to_vec()).into_msbuild_string(),
            "83;116;114;105;110;103"
        );
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
        let error = missing_registry_prefix(
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@XXXXDBDirectory",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Registry:"));
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
        use std::ptr;

        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_QWORD,
            REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteTreeW, RegSetValueExW,
        };

        use super::*;

        struct TestKey {
            subkey: String,
            handle: HKEY,
        }

        impl TestKey {
            fn new() -> Self {
                let subkey = format!(
                    r"Software\Microsoft\MSBuild_rs_property_functions\{}",
                    uuid::Uuid::new_v4()
                );
                let wide = wide(&subkey);
                let mut handle = ptr::null_mut();
                // SAFETY: wide is nul-terminated and handle points to writable
                // storage. The test only creates a unique HKCU subkey.
                let status =
                    unsafe { RegCreateKeyW(HKEY_CURRENT_USER, wide.as_ptr(), &mut handle) };
                assert_eq!(status, ERROR_SUCCESS);
                Self { subkey, handle }
            }

            fn set(&self, kind: u32, bytes: &[u8]) {
                let name = wide("Value");
                // SAFETY: self.handle remains open, name is nul-terminated, and
                // bytes is live for this test-only HKCU write.
                let status = unsafe {
                    RegSetValueExW(
                        self.handle,
                        name.as_ptr(),
                        0,
                        kind,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                    )
                };
                assert_eq!(status, ERROR_SUCCESS);
            }

            fn expression(&self) -> String {
                format!(r"Registry:HKEY_CURRENT_USER\{}@Value", self.subkey)
            }
        }

        impl Drop for TestKey {
            fn drop(&mut self) {
                // SAFETY: handle was returned by RegCreateKeyW and is closed
                // exactly once before deleting the unique HKCU test tree.
                unsafe {
                    RegCloseKey(self.handle);
                    let subkey = wide(&self.subkey);
                    RegDeleteTreeW(HKEY_CURRENT_USER, subkey.as_ptr());
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
            key.set(kind, bytes);
            assert_eq!(
                expand_registry_property(&key.expression()).unwrap(),
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
        fn malformed_registry_property_is_an_error_on_windows() {
            let error = expand_registry_property("Registry:HKEY_CURRENT_USER\\X@A@B")
                .unwrap_err()
                .to_string();
            assert!(error.contains("only one '@'"));
        }
    }
}
