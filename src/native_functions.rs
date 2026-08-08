use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::escaping::unescape_once;
use crate::properties::display_path;
use crate::registry::{RegistryData, RegistryReadResult, RegistryView, read_registry_value};
#[cfg(windows)]
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InvocationKind {
    StaticMethod,
    StaticProperty,
    Constructor,
    InstanceMethod,
    InstanceProperty,
    Indexer,
}

impl InvocationKind {
    fn description(self) -> &'static str {
        match self {
            Self::StaticMethod => "static method",
            Self::StaticProperty => "static property",
            Self::Constructor => "constructor",
            Self::InstanceMethod => "instance method",
            Self::InstanceProperty => "instance property",
            Self::Indexer => "indexer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgumentRule {
    Decoded,
    Escaped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultRule {
    Escape,
    AlreadyEscaped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coercion {
    Any,
    String,
    StringArray,
    Char,
    Byte,
    Int16,
    Int32,
    Int64,
    ArithmeticInt64,
    UInt64,
    Version,
    Path,
    Radix,
    RegistryView,
    ExactBoolean,
    ExactByte,
    ExactInt16,
    ExactInt32,
    ExactInt64,
    ExactUInt64,
    SignedRadixValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullPolicy {
    Reject,
    Preserve,
    EmptyString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arity {
    Exact(u8),
    Range(u8, u8),
    AtLeast(u8),
}

impl Arity {
    fn accepts(self, count: usize) -> bool {
        match self {
            Self::Exact(expected) => count == usize::from(expected),
            Self::Range(minimum, maximum) => {
                (usize::from(minimum)..=usize::from(maximum)).contains(&count)
            }
            Self::AtLeast(minimum) => count >= usize::from(minimum),
        }
    }
}

#[derive(Debug)]
pub(crate) struct OverloadDescriptor {
    pub arity: Arity,
    pub coercions: &'static [Coercion],
    pub nulls: &'static [NullPolicy],
}

type Handler = fn(
    &IntrinsicDescriptor,
    &IntrinsicContext<'_>,
    Option<&IntrinsicValue>,
    &[IntrinsicArgument],
) -> Result<IntrinsicValue>;

pub(crate) struct IntrinsicDescriptor {
    pub type_name: &'static str,
    pub member: &'static str,
    pub kind: InvocationKind,
    pub argument_rule: ArgumentRule,
    pub result_rule: ResultRule,
    pub overloads: &'static [OverloadDescriptor],
    pub result_type: &'static str,
    dispatch_code: u64,
    handler: Handler,
}

impl std::fmt::Debug for IntrinsicDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntrinsicDescriptor")
            .field("type_name", &self.type_name)
            .field("member", &self.member)
            .field("kind", &self.kind)
            .field("argument_rule", &self.argument_rule)
            .field("result_rule", &self.result_rule)
            .field("overloads", &self.overloads)
            .field("result_type", &self.result_type)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct IntrinsicContext<'a> {
    pub tools_directory: Option<&'a str>,
    pub environment: Option<&'a [(String, String)]>,
    pub disable_features_from_version: Option<&'a str>,
    pub runtime_type: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) struct IntrinsicArgument {
    pub value: IntrinsicValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IntrinsicValue {
    Null,
    TypedNull(&'static str),
    String(String),
    Strings(Vec<String>),
    Byte(u8),
    Bytes(Vec<u8>),
    Char(u16),
    Boolean(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    UInt64(u64),
    Version(NativeVersion),
    Guid(Uuid),
    DateTime(NativeDateTime),
}

impl IntrinsicValue {
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "System.Object",
            Self::TypedNull(type_name) => type_name,
            Self::String(_) => "System.String",
            Self::Strings(_) => "System.String[]",
            Self::Byte(_) => "System.Byte",
            Self::Bytes(_) => "System.Byte[]",
            Self::Char(_) => "System.Char",
            Self::Boolean(_) => "System.Boolean",
            Self::Int16(_) => "System.Int16",
            Self::Int32(_) => "System.Int32",
            Self::Int64(_) => "System.Int64",
            Self::UInt64(_) => "System.UInt64",
            Self::Version(_) => "System.Version",
            Self::Guid(_) => "System.Guid",
            Self::DateTime(_) => "System.DateTime",
        }
    }

    pub(crate) fn to_msbuild_string(&self) -> Result<String> {
        match self {
            Self::Null | Self::TypedNull(_) => Ok(String::new()),
            Self::String(value) => Ok(value.clone()),
            Self::Strings(values) => Ok(values.join(";")),
            Self::Byte(value) => Ok(value.to_string()),
            Self::Bytes(values) => Ok(values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(";")),
            Self::Char(value) => String::from_utf16(&[*value])
                .map_err(|_| anyhow!("A lone UTF-16 surrogate cannot be rendered as UTF-8")),
            Self::Boolean(value) => Ok(dotnet_bool(*value).to_string()),
            Self::Int16(value) => Ok(value.to_string()),
            Self::Int32(value) => Ok(value.to_string()),
            Self::Int64(value) => Ok(value.to_string()),
            Self::UInt64(value) => Ok(value.to_string()),
            Self::Version(value) => Ok(value.to_string()),
            Self::Guid(value) => Ok(value.to_string()),
            Self::DateTime(_) => bail!(
                "Direct System.DateTime rendering is outside the native surface; use an allowlisted invariant ToString format"
            ),
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, Self::Null | Self::TypedNull(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NativeVersion {
    parts: [i32; 4],
    count: usize,
}

impl NativeVersion {
    fn parse(value: &str) -> Result<Self> {
        let mut parts = [-1; 4];
        let mut count = 0;
        for part in value.trim().split('.') {
            if count == parts.len()
                || part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                bail!("MSB4184: '{value}' is not a valid System.Version");
            }
            parts[count] = part
                .parse::<i32>()
                .with_context(|| format!("'{value}' is not a valid System.Version"))?;
            count += 1;
        }
        if !(2..=4).contains(&count) {
            bail!("MSB4184: '{value}' is not a valid System.Version");
        }
        Ok(Self { parts, count })
    }

    fn from_arguments(arguments: &[IntrinsicArgument]) -> Result<Self> {
        if !(2..=4).contains(&arguments.len()) {
            bail!("MSB4184: System.Version constructor expects two to four arguments");
        }
        let mut parts = [-1; 4];
        for (index, argument) in arguments.iter().enumerate() {
            parts[index] = argument_i32(argument, "System.Version")?;
            if parts[index] < 0 {
                bail!("MSB4184: System.Version components cannot be negative");
            }
        }
        Ok(Self {
            parts,
            count: arguments.len(),
        })
    }

    fn format_fields(&self, fields: usize) -> Result<String> {
        if fields > self.count {
            bail!(
                "MSB4184: System.Version.ToString({fields}) exceeds the {} available components",
                self.count
            );
        }
        Ok(self.parts[..fields]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("."))
    }
}

impl std::fmt::Display for NativeVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            &self.parts[..self.count]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("."),
        )
    }
}

const ENABLE_ALL_FEATURES: [i32; 4] = [999, 999, -1, -1];
// This repository pins .NET SDK 10.0.302 / MSBuild 18.6. Keep this list in
// lockstep with that toolset's ChangeWaves.AllWaves.
const FEATURE_WAVES: &[[i32; 4]] = &[
    [17, 10, -1, -1],
    [17, 12, -1, -1],
    [17, 14, -1, -1],
    [18, 3, -1, -1],
    [18, 4, -1, -1],
    [18, 5, -1, -1],
    [18, 6, -1, -1],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeatureWaveResolutionKind {
    Valid,
    InvalidFormat,
    OutOfRotation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeatureWaveResolution {
    pub version: String,
    pub kind: FeatureWaveResolutionKind,
}

pub(crate) fn resolve_feature_wave(value: Option<&str>) -> FeatureWaveResolution {
    let enable_all = || FeatureWaveResolution {
        version: "999.999".to_string(),
        kind: FeatureWaveResolutionKind::Valid,
    };
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return enable_all();
    };
    let Ok(version) = NativeVersion::parse(value) else {
        return FeatureWaveResolution {
            kind: FeatureWaveResolutionKind::InvalidFormat,
            ..enable_all()
        };
    };
    if version.parts == ENABLE_ALL_FEATURES && version.count == 2 {
        return enable_all();
    }

    let format_wave = |parts: &[i32; 4]| format!("{}.{}", parts[0], parts[1]);
    if FEATURE_WAVES.contains(&version.parts) && version.count == 2 {
        return FeatureWaveResolution {
            version: version.to_string(),
            kind: FeatureWaveResolutionKind::Valid,
        };
    }
    if version.parts < FEATURE_WAVES[0] {
        return FeatureWaveResolution {
            version: format_wave(&FEATURE_WAVES[0]),
            kind: FeatureWaveResolutionKind::OutOfRotation,
        };
    }
    if version.parts > *FEATURE_WAVES.last().expect("feature waves are nonempty") {
        return FeatureWaveResolution {
            version: format_wave(FEATURE_WAVES.last().expect("feature waves are nonempty")),
            kind: FeatureWaveResolutionKind::OutOfRotation,
        };
    }

    let next = FEATURE_WAVES
        .iter()
        .find(|wave| **wave > version.parts)
        .expect("an in-rotation version has a following wave");
    FeatureWaveResolution {
        version: format_wave(next),
        kind: FeatureWaveResolutionKind::Valid,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeDateTime {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

impl NativeDateTime {
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let (date, time) = value
            .split_once('T')
            .or_else(|| value.split_once(' '))
            .unwrap_or((value, ""));
        let date = date.split('-').collect::<Vec<_>>();
        if date.len() != 3
            || date[0].len() != 4
            || date[1].len() != 2
            || date[2].len() != 2
            || date
                .iter()
                .any(|part| !part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("MSB4184: '{value}' is not a supported ISO System.DateTime");
        }
        let (year, month, day) = (date[0].parse()?, date[1].parse()?, date[2].parse()?);
        let (hour, minute, second) = if time.is_empty() {
            (0, 0, 0)
        } else {
            let parts = time.split(':').collect::<Vec<_>>();
            if !(2..=3).contains(&parts.len()) {
                bail!("MSB4184: '{value}' is not a supported ISO System.DateTime");
            }
            if parts[0].len() != 2
                || parts[1].len() != 2
                || parts.get(2).is_some_and(|part| part.len() != 2)
                || parts
                    .iter()
                    .any(|part| !part.bytes().all(|byte| byte.is_ascii_digit()))
            {
                bail!("MSB4184: '{value}' is not a supported ISO System.DateTime");
            }
            (
                parts[0].parse()?,
                parts[1].parse()?,
                parts.get(2).map_or(Ok(0), |part| part.parse())?,
            )
        };
        if !(1..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            bail!("MSB4184: '{value}' is not a valid System.DateTime");
        }
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    fn format(&self, format: &str) -> Result<String> {
        let mut result = String::with_capacity(format.len() + 8);
        let mut position = 0;
        while position < format.len() {
            let remaining = &format[position..];
            let (token, value) = if remaining.starts_with("yyyy") {
                ("yyyy", format!("{:04}", self.year))
            } else if remaining.starts_with("MM") {
                ("MM", format!("{:02}", self.month))
            } else if remaining.starts_with("dd") {
                ("dd", format!("{:02}", self.day))
            } else if remaining.starts_with("HH") {
                ("HH", format!("{:02}", self.hour))
            } else if remaining.starts_with("mm") {
                ("mm", format!("{:02}", self.minute))
            } else if remaining.starts_with("ss") {
                ("ss", format!("{:02}", self.second))
            } else {
                let character = remaining
                    .chars()
                    .next()
                    .ok_or_else(|| anyhow!("Invalid empty DateTime format suffix"))?;
                if character.is_ascii_alphabetic() || matches!(character, '\\' | '\'' | '"') {
                    bail!(
                        "MSB4184: Unsupported native System.DateTime format token near '{remaining}'"
                    );
                }
                result.push(character);
                position += character.len_utf8();
                continue;
            };
            result.push_str(&value);
            position += token.len();
        }
        Ok(result)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

macro_rules! overload {
    ($arity:expr, [$($coercion:expr),* $(,)?]) => {
        OverloadDescriptor {
            arity: $arity,
            coercions: &[$($coercion),*],
            nulls: &[],
        }
    };
    ($arity:expr, [$($coercion:expr),* $(,)?], [$($null:expr),* $(,)?]) => {
        OverloadDescriptor {
            arity: $arity,
            coercions: &[$($coercion),*],
            nulls: &[$($null),*],
        }
    };
}

const O0: &[OverloadDescriptor] = &[overload!(Arity::Exact(0), [])];
const O1_ANY_NULL: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(1),
    [Coercion::Any],
    [NullPolicy::Preserve]
)];
const O1_STRING: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::String])];
const O1_STRING_NULL: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(1),
    [Coercion::String],
    [NullPolicy::Preserve]
)];
const O1_STRING_EMPTY_NULL: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(1),
    [Coercion::String],
    [NullPolicy::EmptyString]
)];
const O1_VERSION: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Version])];
const O1_BYTE: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Byte])];
const O1_INT16: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Int16])];
const O1_INT32: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Int32])];
const O1_INT64: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Int64])];
const O1_UINT64: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::UInt64])];
const O1_VERSION_NULL: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(1),
    [Coercion::Version],
    [NullPolicy::Preserve]
)];
const O2_STRING: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::String, Coercion::String]
)];
const O2_STRING_NULL: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::String, Coercion::String],
    [NullPolicy::Preserve, NullPolicy::Preserve]
)];
const O_CHANGE_EXTENSION: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::String, Coercion::String],
    [NullPolicy::Preserve, NullPolicy::Preserve]
)];
const O2_REPLACE: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::String, Coercion::String],
    [NullPolicy::Reject, NullPolicy::EmptyString]
)];
const O2_INT64: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::ArithmeticInt64, Coercion::ArithmeticInt64]
)];
const O2_INT32: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Int32, Coercion::Int32]
)];
const O_INT32_STRING: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Int32, Coercion::String]
)];
const O_TFM_VERSION: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::Int32]),
];
const O3_STRING_INT32: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(3),
    [Coercion::String, Coercion::Int32, Coercion::Int32]
)];
const O_STRING_1_2: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::String]),
];
const O_INT32_1_2: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::Int32]),
    overload!(Arity::Exact(2), [Coercion::Int32, Coercion::Int32]),
];
const O0_1_STRING: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::String]),
];
const O0_1_STRING_NULL: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
];
const O0_1_INT32: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::Int32]),
];
const O_MATH_ABS: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::Int16]),
    overload!(Arity::Exact(1), [Coercion::Int32]),
    overload!(Arity::Exact(1), [Coercion::Int64]),
];
const O_CONVERT_INT32: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::ExactBoolean]),
    overload!(Arity::Exact(1), [Coercion::ExactByte]),
    overload!(Arity::Exact(1), [Coercion::ExactInt16]),
    overload!(Arity::Exact(1), [Coercion::ExactInt32]),
    overload!(Arity::Exact(1), [Coercion::ExactInt64]),
    overload!(Arity::Exact(1), [Coercion::ExactUInt64]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::Radix]),
];
const O_CONVERT_INT64: &[OverloadDescriptor] = O_CONVERT_INT32;
const O_CONVERT_UINT64: &[OverloadDescriptor] = O_CONVERT_INT32;
const O_CONVERT_BOOLEAN: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
    overload!(
        Arity::Exact(1),
        [Coercion::ExactBoolean],
        [NullPolicy::Preserve]
    ),
    overload!(Arity::Exact(1), [Coercion::ExactByte]),
    overload!(Arity::Exact(1), [Coercion::ExactInt16]),
    overload!(Arity::Exact(1), [Coercion::ExactInt32]),
    overload!(Arity::Exact(1), [Coercion::ExactInt64]),
    overload!(Arity::Exact(1), [Coercion::ExactUInt64]),
];
const O_CONVERT_STRING: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
    overload!(
        Arity::Exact(1),
        [Coercion::ExactBoolean],
        [NullPolicy::Preserve]
    ),
    overload!(
        Arity::Exact(2),
        [Coercion::SignedRadixValue, Coercion::Radix]
    ),
];
const O_PATH_0_PLUS: &[OverloadDescriptor] = &[overload!(Arity::AtLeast(0), [Coercion::Path])];
const O_PATH_1_PLUS: &[OverloadDescriptor] = &[overload!(Arity::AtLeast(1), [Coercion::Path])];
const O_VERSION_NEW: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Range(2, 4), [Coercion::Int32]),
];
const O_JOIN: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(2), [Coercion::String, Coercion::StringArray]),
    overload!(Arity::Exact(2), [Coercion::Char, Coercion::StringArray]),
    overload!(
        Arity::AtLeast(2),
        [Coercion::String],
        [NullPolicy::Reject, NullPolicy::EmptyString]
    ),
    overload!(
        Arity::AtLeast(2),
        [Coercion::Char, Coercion::String],
        [NullPolicy::Reject, NullPolicy::EmptyString]
    ),
];
const O_SPLIT: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Exact(1), [Coercion::Char]),
];
const O_REGISTRY_VALUE: &[OverloadDescriptor] = &[
    overload!(
        Arity::Exact(2),
        [Coercion::String, Coercion::String],
        [NullPolicy::Preserve, NullPolicy::Preserve]
    ),
    overload!(
        Arity::Exact(3),
        [Coercion::String, Coercion::String, Coercion::Any],
        [
            NullPolicy::Preserve,
            NullPolicy::Preserve,
            NullPolicy::Preserve
        ]
    ),
];
const O_REGISTRY_VIEWS: &[OverloadDescriptor] = &[overload!(
    Arity::AtLeast(3),
    [
        Coercion::String,
        Coercion::String,
        Coercion::Any,
        Coercion::RegistryView,
    ],
    [
        NullPolicy::Preserve,
        NullPolicy::Preserve,
        NullPolicy::Preserve,
        NullPolicy::Preserve,
    ]
)];

const fn member_code(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let mut hash = 14_695_981_039_346_656_037u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index].to_ascii_lowercase() as u64;
        hash = hash.wrapping_mul(1_099_511_628_211);
        index += 1;
    }
    hash
}

macro_rules! intrinsic {
    ($type:literal, $member:literal, $kind:ident, $rule:ident, $result_rule:ident, $overloads:ident, $result:literal, $handler:ident) => {
        IntrinsicDescriptor {
            type_name: $type,
            member: $member,
            kind: InvocationKind::$kind,
            argument_rule: ArgumentRule::$rule,
            result_rule: ResultRule::$result_rule,
            overloads: $overloads,
            result_type: $result,
            dispatch_code: member_code($member),
            handler: $handler,
        }
    };
}

static INTRINSICS: &[IntrinsicDescriptor] = &[
    intrinsic!(
        "MSBuild",
        "AreFeaturesEnabled",
        StaticMethod,
        Decoded,
        Escape,
        O1_VERSION,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "IsRunningFromVisualStudio",
        StaticMethod,
        Decoded,
        Escape,
        O0,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "IsOSPlatform",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "IsOSUnixLike",
        StaticMethod,
        Decoded,
        Escape,
        O0,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "VersionGreaterThan",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "VersionGreaterThanOrEquals",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "VersionLessThan",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "VersionLessThanOrEquals",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "VersionEquals",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetTargetFrameworkIdentifier",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetTargetFrameworkVersion",
        StaticMethod,
        Decoded,
        Escape,
        O_TFM_VERSION,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetTargetPlatformIdentifier",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetTargetPlatformVersion",
        StaticMethod,
        Decoded,
        Escape,
        O_TFM_VERSION,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetDirectoryNameOfFileAbove",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetPathOfFileAbove",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "MakeRelative",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "NormalizePath",
        StaticMethod,
        Decoded,
        Escape,
        O_PATH_1_PLUS,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "NormalizeDirectory",
        StaticMethod,
        Decoded,
        Escape,
        O_PATH_1_PLUS,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "EnsureTrailingSlash",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Add",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT64,
        "System.Int64",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Subtract",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT64,
        "System.Int64",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Multiply",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT64,
        "System.Int64",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Divide",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT64,
        "System.Int64",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Modulo",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT64,
        "System.Int64",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseOr",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseAnd",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseXor",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseNot",
        StaticMethod,
        Decoded,
        Escape,
        O1_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "LeftShift",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "RightShift",
        StaticMethod,
        Decoded,
        Escape,
        O2_INT32,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "ValueOrDefault",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING_NULL,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Escape",
        StaticMethod,
        Decoded,
        AlreadyEscaped,
        O1_STRING_EMPTY_NULL,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Unescape",
        StaticMethod,
        Escaped,
        AlreadyEscaped,
        O1_STRING_EMPTY_NULL,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "DoesTaskHostExist",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING_NULL,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetToolsDirectory32",
        StaticMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "SubstringByAsciiChars",
        StaticMethod,
        Decoded,
        Escape,
        O3_STRING_INT32,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "StableStringHash",
        StaticMethod,
        Decoded,
        Escape,
        O_STRING_1_2,
        "System.Int32/System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetRegistryValue",
        StaticMethod,
        Decoded,
        Escape,
        O_REGISTRY_VALUE,
        "System.Object",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "GetRegistryValueFromView",
        StaticMethod,
        Decoded,
        Escape,
        O_REGISTRY_VIEWS,
        "System.Object",
        handle_msbuild
    ),
    intrinsic!(
        "System.String",
        "Copy",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "IsNullOrEmpty",
        StaticMethod,
        Decoded,
        Escape,
        O1_ANY_NULL,
        "System.Boolean",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "IsNullOrWhiteSpace",
        StaticMethod,
        Decoded,
        Escape,
        O1_ANY_NULL,
        "System.Boolean",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "Join",
        StaticMethod,
        Decoded,
        Escape,
        O_JOIN,
        "System.String",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "CompareOrdinal",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING_NULL,
        "System.Int32",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "Contains",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "StartsWith",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "EndsWith",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Substring",
        InstanceMethod,
        Decoded,
        Escape,
        O_INT32_1_2,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Trim",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING_NULL,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimStart",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING_NULL,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimEnd",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING_NULL,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Replace",
        InstanceMethod,
        Decoded,
        Escape,
        O2_REPLACE,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Split",
        InstanceMethod,
        Decoded,
        Escape,
        O_SPLIT,
        "System.String[]",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING_NULL,
        "System.Boolean",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Insert",
        InstanceMethod,
        Decoded,
        Escape,
        O_INT32_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Remove",
        InstanceMethod,
        Decoded,
        Escape,
        O_INT32_1_2,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Length",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Item",
        Indexer,
        Decoded,
        Escape,
        O1_INT32,
        "System.Char",
        handle_string_instance
    ),
    intrinsic!(
        "System.String[]",
        "Length",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_string_array
    ),
    intrinsic!(
        "System.String[]",
        "Item",
        Indexer,
        Decoded,
        Escape,
        O1_INT32,
        "System.String",
        handle_string_array
    ),
    intrinsic!(
        "System.String[]",
        "GetValue",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT32,
        "System.String",
        handle_string_array
    ),
    intrinsic!(
        "System.Byte[]",
        "Length",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_byte_array
    ),
    intrinsic!(
        "System.Byte[]",
        "Item",
        Indexer,
        Decoded,
        Escape,
        O1_INT32,
        "System.Byte",
        handle_byte_array
    ),
    intrinsic!(
        "System.Byte[]",
        "GetValue",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT32,
        "System.Byte",
        handle_byte_array
    ),
    intrinsic!(
        "System.Char",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_char_instance
    ),
    intrinsic!(
        "System.Int16",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT16,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int16",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT16,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int16",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Byte",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_BYTE,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Byte",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_BYTE,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Byte",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.IO.Path",
        "Combine",
        StaticMethod,
        Decoded,
        Escape,
        O_PATH_0_PLUS,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "IsPathRooted",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetDirectoryName",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetFileName",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetFileNameWithoutExtension",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetExtension",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetFullPath",
        StaticMethod,
        Decoded,
        Escape,
        O_STRING_1_2,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetPathRoot",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "HasExtension",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "ChangeExtension",
        StaticMethod,
        Decoded,
        Escape,
        O_CHANGE_EXTENSION,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "GetTempPath",
        StaticMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "DirectorySeparatorChar",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Char",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "AltDirectorySeparatorChar",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Char",
        handle_path
    ),
    intrinsic!(
        "System.IO.Path",
        "PathSeparator",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Char",
        handle_path
    ),
    intrinsic!(
        "System.Environment",
        "ExpandEnvironmentVariables",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_environment
    ),
    intrinsic!(
        "System.Environment",
        "GetEnvironmentVariable",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_environment
    ),
    intrinsic!(
        "System.Environment",
        "NewLine",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_environment
    ),
    intrinsic!(
        "System.Environment",
        "Is64BitProcess",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Boolean",
        handle_environment
    ),
    intrinsic!(
        "System.Environment",
        "ProcessorCount",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_environment
    ),
    intrinsic!(
        "Microsoft.Build.Utilities.ToolLocationHelper",
        "GetPlatformSDKLocation",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING_NULL,
        "System.String",
        handle_tool_location
    ),
    intrinsic!(
        "Microsoft.Build.Utilities.ToolLocationHelper",
        "GetPlatformSDKDisplayName",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING_NULL,
        "System.String",
        handle_tool_location
    ),
    intrinsic!(
        "System.Math",
        "Abs",
        StaticMethod,
        Decoded,
        Escape,
        O_MATH_ABS,
        "System.Int16/System.Int32/System.Int64",
        handle_math
    ),
    intrinsic!(
        "System.Convert",
        "ToInt32",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INT32,
        "System.Int32",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToInt64",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INT64,
        "System.Int64",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToUInt64",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_UINT64,
        "System.UInt64",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToBoolean",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_BOOLEAN,
        "System.Boolean",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToString",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_STRING,
        "System.String",
        handle_convert
    ),
    intrinsic!(
        "System.Version",
        "Parse",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Version",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "new",
        Constructor,
        Decoded,
        Escape,
        O_VERSION_NEW,
        "System.Version",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Major",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Minor",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Build",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Revision",
        InstanceProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_INT32,
        "System.String",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_VERSION_NULL,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_VERSION_NULL,
        "System.Boolean",
        handle_version
    ),
    intrinsic!(
        "System.Guid",
        "Parse",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Guid",
        handle_guid
    ),
    intrinsic!(
        "System.Guid",
        "NewGuid",
        StaticMethod,
        Decoded,
        Escape,
        O0,
        "System.Guid",
        handle_guid
    ),
    intrinsic!(
        "System.Guid",
        "Empty",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Guid",
        handle_guid
    ),
    intrinsic!(
        "System.Guid",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING_NULL,
        "System.String",
        handle_guid
    ),
    intrinsic!(
        "System.DateTime",
        "Parse",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.DateTime",
        handle_datetime
    ),
    intrinsic!(
        "System.DateTime",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String",
        handle_datetime
    ),
    intrinsic!(
        "System.Int32",
        "Parse",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Int32",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int32",
        "MaxValue",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int32",
        "MinValue",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Int32",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int64",
        "Parse",
        StaticMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Int64",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int64",
        "MaxValue",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Int64",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int64",
        "MinValue",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Int64",
        handle_integer_static
    ),
    intrinsic!(
        "System.Int32",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT32,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int32",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT32,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int32",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int64",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT64,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int64",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_INT64,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int64",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.UInt64",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_UINT64,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.UInt64",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_UINT64,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.UInt64",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Boolean",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_boolean_instance
    ),
];

#[derive(Debug, Hash, PartialEq, Eq)]
struct IntrinsicKey {
    type_code: u64,
    member_code: u64,
    kind: InvocationKind,
}

struct IntrinsicIndex {
    members: HashMap<IntrinsicKey, usize>,
    types: HashSet<u64>,
}

static INDEX: OnceLock<IntrinsicIndex> = OnceLock::new();

fn index() -> &'static IntrinsicIndex {
    INDEX.get_or_init(|| {
        let mut members = HashMap::with_capacity(INTRINSICS.len());
        let mut types = HashSet::new();
        for (position, descriptor) in allowed_intrinsics().iter().enumerate() {
            let type_code = member_code(descriptor.type_name);
            let member_code = descriptor.dispatch_code;
            types.insert(type_code);
            let previous = members.insert(
                IntrinsicKey {
                    type_code,
                    member_code,
                    kind: descriptor.kind,
                },
                position,
            );
            assert!(
                previous.is_none(),
                "duplicate native intrinsic registry key"
            );
        }
        IntrinsicIndex { members, types }
    })
}

pub(crate) fn allowed_intrinsics() -> &'static [IntrinsicDescriptor] {
    INTRINSICS
}

pub(crate) fn is_allowed(type_name: &str, member: &str, kind: InvocationKind) -> bool {
    index().members.contains_key(&IntrinsicKey {
        type_code: member_code(type_name),
        member_code: member_code(member),
        kind,
    })
}

pub(crate) fn resolve(
    type_name: &str,
    member: &str,
    kind: InvocationKind,
    argument_count: usize,
) -> Result<&'static IntrinsicDescriptor> {
    let type_code = member_code(type_name);
    let registry = index();
    let Some(position) = registry.members.get(&IntrinsicKey {
        type_code,
        member_code: member_code(member),
        kind,
    }) else {
        if registry.types.contains(&type_code) {
            bail!(
                "MSB4185: The {} '{}.{}' is not in the native MSBuild property-function allowlist",
                kind.description(),
                type_name,
                member
            );
        }
        bail!(
            "MSB4212: Invalid property-function invocation '[{type_name}]::{member}': the type '{type_name}' is not available for execution and the member was not invoked"
        );
    };
    let descriptor = &INTRINSICS[*position];
    if !descriptor
        .overloads
        .iter()
        .any(|overload| overload.arity.accepts(argument_count))
    {
        bail!(
            "MSB4186: The {} '[{}]::{}' has no allowlisted overload accepting {} argument(s)",
            kind.description(),
            descriptor.type_name,
            descriptor.member,
            argument_count
        );
    }
    debug_assert!(
        descriptor.overloads.iter().all(|overload| {
            !overload.coercions.is_empty() || overload.arity == Arity::Exact(0)
        })
    );
    Ok(descriptor)
}

pub(crate) fn invoke(
    descriptor: &'static IntrinsicDescriptor,
    context: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let arguments = select_and_coerce_overload(descriptor, arguments)?;
    let value =
        (descriptor.handler)(descriptor, context, receiver, &arguments).with_context(|| {
            format!(
                "MSB4184: The expression invoking [{}]::{} could not be evaluated",
                descriptor.type_name, descriptor.member
            )
        })?;
    Ok(if matches!(value, IntrinsicValue::Null) {
        IntrinsicValue::TypedNull(descriptor.result_type)
    } else {
        value
    })
}

const ITEM_STRING_FUNCTIONS: &[&str] = &[
    "Contains",
    "Equals",
    "Substring",
    "Trim",
    "TrimStart",
    "TrimEnd",
    "Replace",
    "get_Length",
];

pub(crate) fn resolve_item_string_function(member: &str) -> Option<&'static str> {
    ITEM_STRING_FUNCTIONS
        .iter()
        .copied()
        .find(|candidate| member.eq_ignore_ascii_case(candidate))
}

pub(crate) fn invoke_item_string_function(
    member: &str,
    receiver: &str,
    arguments: &[String],
) -> Result<String> {
    debug_assert!(ITEM_STRING_FUNCTIONS.contains(&member));
    if member.eq_ignore_ascii_case("get_Length") {
        if !arguments.is_empty() {
            bail!("{member} expects 0 argument(s), found {}", arguments.len());
        }
        return Ok(receiver.encode_utf16().count().to_string());
    }

    if matches_ignore_ascii_case(member, &["Trim", "TrimStart", "TrimEnd"]) && arguments.len() == 1
    {
        return trim_utf16(receiver, &arguments[0], member);
    }

    let descriptor = resolve(
        "System.String",
        member,
        InvocationKind::InstanceMethod,
        arguments.len(),
    )?;
    let receiver = IntrinsicValue::String(receiver.to_string());
    let arguments = arguments
        .iter()
        .map(|argument| IntrinsicArgument {
            value: IntrinsicValue::String(argument.clone()),
        })
        .collect::<Vec<_>>();
    let context = IntrinsicContext {
        tools_directory: None,
        environment: None,
        disable_features_from_version: None,
        runtime_type: None,
    };
    invoke(descriptor, &context, Some(&receiver), &arguments)
        .and_then(|value| value.to_msbuild_string())
}

fn trim_utf16(receiver: &str, characters: &str, member: &str) -> Result<String> {
    if characters.is_empty() {
        return Ok(if member.eq_ignore_ascii_case("TrimStart") {
            receiver.trim_start_matches(char::is_whitespace)
        } else if member.eq_ignore_ascii_case("TrimEnd") {
            receiver.trim_end_matches(char::is_whitespace)
        } else {
            receiver.trim_matches(char::is_whitespace)
        }
        .to_string());
    }
    let receiver = receiver.encode_utf16().collect::<Vec<_>>();
    let characters = characters.encode_utf16().collect::<Vec<_>>();
    let mut start = 0;
    let mut end = receiver.len();
    if !member.eq_ignore_ascii_case("TrimEnd") {
        while start < end && characters.contains(&receiver[start]) {
            start += 1;
        }
    }
    if !member.eq_ignore_ascii_case("TrimStart") {
        while end > start && characters.contains(&receiver[end - 1]) {
            end -= 1;
        }
    }
    String::from_utf16(&receiver[start..end])
        .map_err(|_| anyhow!("{member} produced a lone UTF-16 surrogate"))
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn select_and_coerce_overload(
    descriptor: &IntrinsicDescriptor,
    arguments: &[IntrinsicArgument],
) -> Result<Vec<IntrinsicArgument>> {
    let mut best: Option<(u32, Vec<IntrinsicArgument>)> = None;
    let mut ambiguous = false;
    let mut failures = Vec::new();

    for overload in descriptor
        .overloads
        .iter()
        .filter(|overload| overload.arity.accepts(arguments.len()))
    {
        match coerce_overload(overload, arguments, descriptor.member) {
            Ok((score, coerced)) => match &best {
                None => {
                    best = Some((score, coerced));
                    ambiguous = false;
                }
                Some((best_score, _)) if score < *best_score => {
                    best = Some((score, coerced));
                    ambiguous = false;
                }
                Some((best_score, _)) if score == *best_score => ambiguous = true,
                Some(_) => {}
            },
            Err(error) => failures.push(error.to_string()),
        }
    }

    if ambiguous {
        bail!(
            "MSB4186: Ambiguous native overload match for [{}]::{} with {} argument(s)",
            descriptor.type_name,
            descriptor.member,
            arguments.len()
        );
    }
    best.map(|(_, arguments)| arguments).ok_or_else(|| {
        let detail = failures
            .into_iter()
            .next()
            .unwrap_or_else(|| "no coercion was applicable".to_string());
        anyhow!(
            "MSB4186: No native overload of [{}]::{} accepts the supplied argument types: {detail}",
            descriptor.type_name,
            descriptor.member
        )
    })
}

fn coerce_overload(
    overload: &OverloadDescriptor,
    arguments: &[IntrinsicArgument],
    member: &str,
) -> Result<(u32, Vec<IntrinsicArgument>)> {
    let mut score = 0u32;
    let mut result = Vec::with_capacity(arguments.len());
    for (index, argument) in arguments.iter().enumerate() {
        let coercion = overload
            .coercions
            .get(index)
            .or_else(|| overload.coercions.last())
            .copied()
            .ok_or_else(|| anyhow!("{member} overload has no coercion for argument {index}"))?;
        let null_policy = overload
            .nulls
            .get(index)
            .or_else(|| overload.nulls.last())
            .copied()
            .unwrap_or(NullPolicy::Reject);
        let (value, conversion_score) =
            coerce_argument(&argument.value, coercion, null_policy, member)?;
        score = score.saturating_add(conversion_score);
        result.push(IntrinsicArgument { value });
    }
    Ok((score, result))
}

fn coerce_argument(
    value: &IntrinsicValue,
    coercion: Coercion,
    null_policy: NullPolicy,
    member: &str,
) -> Result<(IntrinsicValue, u32)> {
    if value.is_null() {
        if matches!(value, IntrinsicValue::TypedNull(type_name) if type_name.eq_ignore_ascii_case("System.String"))
            && matches!(coercion, Coercion::String | Coercion::Path)
            && null_policy == NullPolicy::Reject
        {
            return Ok((IntrinsicValue::String(String::new()), 0));
        }
        return match null_policy {
            NullPolicy::Reject => bail!("{member} does not accept null for this overload"),
            NullPolicy::Preserve => match value {
                IntrinsicValue::Null => Ok((IntrinsicValue::Null, 0)),
                IntrinsicValue::TypedNull(type_name) => {
                    let score =
                        typed_null_conversion_score(type_name, coercion).ok_or_else(|| {
                            anyhow!(
                                "{member} cannot bind null declared as {type_name} to {}",
                                coercion_type_name(coercion)
                            )
                        })?;
                    Ok((value.clone(), score))
                }
                _ => unreachable!(),
            },
            NullPolicy::EmptyString => Ok((IntrinsicValue::String(String::new()), 0)),
        };
    }

    let result = match coercion {
        Coercion::Any | Coercion::RegistryView => (value.clone(), 0),
        Coercion::String | Coercion::Path => match value {
            IntrinsicValue::String(value) => (IntrinsicValue::String(value.clone()), 0),
            _ => bail!("{member} cannot coerce {} to String", value.type_name()),
        },
        Coercion::StringArray => match value {
            IntrinsicValue::Strings(values) => (IntrinsicValue::Strings(values.clone()), 0),
            _ => bail!(
                "{member} cannot coerce {} to System.String[]",
                value.type_name()
            ),
        },
        Coercion::Char => match value {
            IntrinsicValue::Char(value) => (IntrinsicValue::Char(*value), 0),
            IntrinsicValue::String(value) => {
                let mut units = value.encode_utf16();
                let character = units
                    .next()
                    .filter(|_| units.next().is_none())
                    .ok_or_else(|| anyhow!("{member} requires a single UTF-16 character"))?;
                (IntrinsicValue::Char(character), 1)
            }

            _ => bail!("{member} cannot coerce {} to Char", value.type_name()),
        },
        Coercion::Byte => match value {
            IntrinsicValue::Byte(value) => (IntrinsicValue::Byte(*value), 0),
            IntrinsicValue::Int16(value) => (IntrinsicValue::Byte((*value).try_into()?), 5),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Byte((*value).try_into()?), 5),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Byte((*value).try_into()?), 6),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Byte((*value).try_into()?), 6),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::Byte(value.trim().parse::<u8>()?), 20)
            }
            _ => bail!("{member} cannot coerce {} to Byte", value.type_name()),
        },
        Coercion::Int16 => match value {
            IntrinsicValue::Byte(value) => (IntrinsicValue::Int16(i16::from(*value)), 1),
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int16(*value), 0),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int16((*value).try_into()?), 5),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int16((*value).try_into()?), 6),
            IntrinsicValue::String(value) => (IntrinsicValue::Int16(parse_decimal_i16(value)?), 20),
            _ => bail!("{member} cannot coerce {} to Int16", value.type_name()),
        },
        Coercion::Int32 | Coercion::Radix => {
            let (value, score) = match value {
                IntrinsicValue::Byte(value) => (i32::from(*value), 1),
                IntrinsicValue::Int16(value) => (i32::from(*value), 1),
                IntrinsicValue::Int32(value) => (*value, 0),
                IntrinsicValue::Int64(value) => ((*value).try_into()?, 5),
                IntrinsicValue::String(value) => (parse_decimal_i32(value)?, 21),
                _ => bail!("{member} cannot coerce {} to Int32", value.type_name()),
            };
            if coercion == Coercion::Radix && !matches!(value, 2 | 8 | 10 | 16) {
                bail!("{member} radix must be 2, 8, 10, or 16");
            }
            (IntrinsicValue::Int32(value), score)
        }
        Coercion::Int64 | Coercion::ArithmeticInt64 => match value {
            IntrinsicValue::Byte(value) => (IntrinsicValue::Int64(i64::from(*value)), 1),
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int64(i64::from(*value)), 1),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int64(i64::from(*value)), 1),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int64(*value), 0),
            IntrinsicValue::String(value) => (
                IntrinsicValue::Int64(if coercion == Coercion::ArithmeticInt64 {
                    parse_arithmetic_i64(value)?
                } else {
                    parse_decimal_i64(value)?
                }),
                22,
            ),
            _ => bail!("{member} cannot coerce {} to Int64", value.type_name()),
        },
        Coercion::UInt64 => match value {
            IntrinsicValue::Byte(value) => (IntrinsicValue::UInt64(u64::from(*value)), 1),
            IntrinsicValue::Int16(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::Int32(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::Int64(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::UInt64(*value), 0),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::UInt64(parse_decimal_u64(value)?), 23)
            }
            _ => bail!("{member} cannot coerce {} to UInt64", value.type_name()),
        },
        Coercion::Version => match value {
            IntrinsicValue::Version(value) => (IntrinsicValue::Version(value.clone()), 0),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::Version(NativeVersion::parse(value)?), 20)
            }
            _ => bail!("{member} cannot coerce {} to Version", value.type_name()),
        },
        Coercion::ExactBoolean => match value {
            IntrinsicValue::Boolean(value) => (IntrinsicValue::Boolean(*value), 0),
            _ => bail!("{member} requires a typed Boolean argument"),
        },
        Coercion::ExactByte => match value {
            IntrinsicValue::Byte(value) => (IntrinsicValue::Byte(*value), 0),
            _ => bail!("{member} requires a typed Byte argument"),
        },
        Coercion::ExactInt16 => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int16(*value), 0),
            _ => bail!("{member} requires a typed Int16 argument"),
        },
        Coercion::ExactInt32 => match value {
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int32(*value), 0),
            _ => bail!("{member} requires a typed Int32 argument"),
        },
        Coercion::ExactInt64 => match value {
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int64(*value), 0),
            _ => bail!("{member} requires a typed Int64 argument"),
        },
        Coercion::ExactUInt64 => match value {
            IntrinsicValue::UInt64(value) => (IntrinsicValue::UInt64(*value), 0),
            _ => bail!("{member} requires a typed UInt64 argument"),
        },
        Coercion::SignedRadixValue => (coerce_signed_radix_value(value, member)?, 0),
    };
    Ok(result)
}

fn coercion_type_name(coercion: Coercion) -> &'static str {
    match coercion {
        Coercion::Any => "System.Object",
        Coercion::String | Coercion::Path => "System.String",
        Coercion::StringArray => "System.String[]",
        Coercion::Char => "System.Char",
        Coercion::Byte | Coercion::ExactByte => "System.Byte",
        Coercion::Int16 | Coercion::ExactInt16 => "System.Int16",
        Coercion::Int32 | Coercion::ExactInt32 | Coercion::Radix => "System.Int32",
        Coercion::Int64 | Coercion::ArithmeticInt64 | Coercion::ExactInt64 => "System.Int64",
        Coercion::UInt64 | Coercion::ExactUInt64 => "System.UInt64",
        Coercion::Version => "System.Version",
        Coercion::RegistryView => "Microsoft.Win32.RegistryView",
        Coercion::ExactBoolean => "System.Boolean",
        Coercion::SignedRadixValue => "System.Int64",
    }
}

fn typed_null_conversion_score(type_name: &str, coercion: Coercion) -> Option<u32> {
    if coercion == Coercion::Any {
        return Some(10);
    }
    type_name
        .eq_ignore_ascii_case(coercion_type_name(coercion))
        .then_some(0)
}

fn handle_string_static(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("IsNullOrEmpty") {
        Ok(IntrinsicValue::Boolean(match &arguments[0].value {
            IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => true,
            IntrinsicValue::String(value) => value.is_empty(),
            _ => false,
        }))
    } else if operation == member_code("IsNullOrWhiteSpace") {
        Ok(IntrinsicValue::Boolean(match &arguments[0].value {
            IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => true,
            IntrinsicValue::String(value) => value.chars().all(char::is_whitespace),
            _ => false,
        }))
    } else if operation == member_code("Join") {
        let separator = match &arguments[0].value {
            IntrinsicValue::String(value) => value.clone(),
            IntrinsicValue::Char(value) => String::from_utf16(&[*value])
                .map_err(|_| anyhow!("String.Join separator is a lone UTF-16 surrogate"))?,
            value => bail!(
                "String.Join requires a String or Char separator, found {}",
                value.type_name()
            ),
        };
        if let [
            _,
            IntrinsicArgument {
                value: IntrinsicValue::Strings(values),
            },
        ] = arguments
        {
            return Ok(IntrinsicValue::String(values.join(&separator)));
        }
        Ok(IntrinsicValue::String(
            arguments[1..]
                .iter()
                .map(|argument| argument_string(argument, descriptor.member))
                .collect::<Result<Vec<_>>>()?
                .join(&separator),
        ))
    } else if operation == member_code("CompareOrdinal") {
        let left = argument_optional_string(&arguments[0], descriptor.member)?;
        let right = argument_optional_string(&arguments[1], descriptor.member)?;
        Ok(IntrinsicValue::Int32(match (left, right) {
            (None, None) => 0,
            (None, Some(_)) => -1,
            (Some(_), None) => 1,
            (Some(left), Some(right)) => compare_ordinal(left, right),
        }))
    } else {
        Ok(IntrinsicValue::String(
            argument_string(&arguments[0], descriptor.member)?.to_string(),
        ))
    }
}

fn handle_string_instance(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let receiver = require_string(receiver, descriptor)?;
    let member = descriptor.member;
    let operation = descriptor.dispatch_code;
    if operation == member_code("Contains") {
        Ok(IntrinsicValue::Boolean(
            receiver.contains(argument_string(&arguments[0], member)?),
        ))
    } else if operation == member_code("StartsWith") || operation == member_code("EndsWith") {
        let argument = argument_string(&arguments[0], member)?;
        if !receiver.is_ascii() || !argument.is_ascii() {
            bail!("{member} is retained only for deterministic ASCII SDK inputs");
        }
        Ok(IntrinsicValue::Boolean(
            if operation == member_code("StartsWith") {
                receiver.starts_with(argument)
            } else {
                receiver.ends_with(argument)
            },
        ))
    } else if operation == member_code("Substring") {
        let start = argument_usize(&arguments[0], member)?;
        let length = arguments
            .get(1)
            .map(|argument| argument_usize(argument, member))
            .transpose()?;
        Ok(IntrinsicValue::String(utf16_substring(
            receiver, start, length,
        )?))
    } else if matches!(
        operation,
        value
            if value == member_code("Trim")
                || value == member_code("TrimStart")
                || value == member_code("TrimEnd")
    ) {
        let characters = arguments
            .first()
            .map(|argument| argument_optional_string(argument, member))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        Ok(IntrinsicValue::String(trim_utf16(
            receiver, characters, member,
        )?))
    } else if operation == member_code("Replace") {
        let old = argument_string(&arguments[0], member)?;
        if old.is_empty() {
            bail!("String.Replace oldValue cannot be empty");
        }
        Ok(IntrinsicValue::String(
            receiver.replace(old, argument_string(&arguments[1], member)?),
        ))
    } else if operation == member_code("Split") {
        let separator_storage;
        let separators = match arguments.first().map(|argument| &argument.value) {
            None => None,
            Some(IntrinsicValue::String(value)) if value.is_empty() => None,
            Some(IntrinsicValue::String(value)) => Some(value.as_str()),
            Some(IntrinsicValue::Char(value)) => {
                separator_storage = String::from_utf16(&[*value])
                    .map_err(|_| anyhow!("String.Split separator is a lone UTF-16 surrogate"))?;
                Some(separator_storage.as_str())
            }
            Some(value) => bail!(
                "String.Split requires a String or Char separator, found {}",
                value.type_name()
            ),
        };
        if separators.is_some_and(|value| value.chars().any(|character| character.len_utf16() > 1))
        {
            bail!("Native String.Split does not support surrogate-pair separators");
        }
        let values = if separators.is_none() {
            receiver
                .split(char::is_whitespace)
                .map(ToString::to_string)
                .collect()
        } else {
            receiver
                .split(|character| separators.is_some_and(|value| value.contains(character)))
                .map(ToString::to_string)
                .collect()
        };
        Ok(IntrinsicValue::Strings(values))
    } else if operation == member_code("Equals") {
        Ok(IntrinsicValue::Boolean(match &arguments[0].value {
            IntrinsicValue::String(value) => receiver == value,
            IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => false,
            _ => false,
        }))
    } else if operation == member_code("Insert") {
        let index = argument_usize(&arguments[0], member)?;
        let byte_index = utf16_byte_index(receiver, index)?;
        let mut result = receiver.clone();
        result.insert_str(byte_index, argument_string(&arguments[1], member)?);
        Ok(IntrinsicValue::String(result))
    } else if operation == member_code("Remove") {
        let start = argument_usize(&arguments[0], member)?;
        let total = receiver.encode_utf16().count();
        if start > total {
            bail!("Remove index {start} is out of bounds");
        }
        let length = arguments
            .get(1)
            .map(|argument| argument_usize(argument, member))
            .transpose()?;
        let prefix = utf16_substring(receiver, 0, Some(start))?;
        let suffix_start = start
            .checked_add(length.unwrap_or(total - start))
            .filter(|end| *end <= total)
            .ok_or_else(|| anyhow!("Remove range is out of bounds"))?;
        let suffix = utf16_substring(receiver, suffix_start, None)?;
        Ok(IntrinsicValue::String(prefix + &suffix))
    } else if operation == member_code("Length") {
        Ok(IntrinsicValue::Int32(
            receiver
                .encode_utf16()
                .count()
                .try_into()
                .context("String length exceeds System.Int32")?,
        ))
    } else if operation == member_code("Item") {
        let index = argument_usize(&arguments[0], member)?;
        Ok(IntrinsicValue::Char(
            receiver
                .encode_utf16()
                .nth(index)
                .ok_or_else(|| anyhow!("String index {index} is out of range"))?,
        ))
    } else if operation == member_code("ToString") {
        Ok(IntrinsicValue::String(receiver.clone()))
    } else {
        unreachable!("all registered string members are handled")
    }
}

fn handle_string_array(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let Some(IntrinsicValue::Strings(receiver)) = receiver else {
        bail!("{} requires a System.String[] receiver", descriptor.member);
    };
    if descriptor.dispatch_code == member_code("Length") {
        Ok(IntrinsicValue::Int32(
            receiver
                .len()
                .try_into()
                .context("Array length exceeds System.Int32")?,
        ))
    } else {
        let index = argument_usize(&arguments[0], descriptor.member)?;
        receiver
            .get(index)
            .cloned()
            .map(IntrinsicValue::String)
            .ok_or_else(|| anyhow!("String array index {index} is out of range"))
    }
}

fn handle_byte_array(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let Some(IntrinsicValue::Bytes(receiver)) = receiver else {
        bail!("{} requires a System.Byte[] receiver", descriptor.member);
    };
    if descriptor.dispatch_code == member_code("Length") {
        Ok(IntrinsicValue::Int32(
            receiver
                .len()
                .try_into()
                .context("Array length exceeds System.Int32")?,
        ))
    } else {
        let index = argument_usize(&arguments[0], descriptor.member)?;
        receiver
            .get(index)
            .copied()
            .map(IntrinsicValue::Byte)
            .ok_or_else(|| anyhow!("Byte array index {index} is out of range"))
    }
}

fn handle_char_instance(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    _: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let Some(IntrinsicValue::Char(value)) = receiver else {
        bail!("{} requires a System.Char receiver", descriptor.member);
    };
    Ok(IntrinsicValue::String(
        String::from_utf16(&[*value])
            .map_err(|_| anyhow!("A lone UTF-16 surrogate cannot be converted to String"))?,
    ))
}

fn handle_path(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("Combine") {
        let paths = arguments
            .iter()
            .map(|argument| argument_string(argument, descriptor.member))
            .collect::<Result<Vec<_>>>()?;
        Ok(IntrinsicValue::String(host_combine(&paths)))
    } else if operation == member_code("IsPathRooted") {
        Ok(IntrinsicValue::Boolean(
            argument_optional_string(&arguments[0], descriptor.member)?
                .is_some_and(|path| path_is_rooted(host_path_style(), path)),
        ))
    } else if operation == member_code("GetDirectoryName") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        Ok(path_get_directory_name(host_path_style(), path)
            .map(IntrinsicValue::String)
            .unwrap_or(IntrinsicValue::Null))
    } else if operation == member_code("GetFileName") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        Ok(IntrinsicValue::String(
            path_get_file_name(host_path_style(), path).to_string(),
        ))
    } else if operation == member_code("GetFileNameWithoutExtension") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        let file_name = path_get_file_name(host_path_style(), path);
        Ok(IntrinsicValue::String(
            file_name
                .rfind('.')
                .map_or(file_name, |index| &file_name[..index])
                .to_string(),
        ))
    } else if operation == member_code("GetExtension") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        Ok(IntrinsicValue::String(
            path_get_extension(host_path_style(), path).to_string(),
        ))
    } else if operation == member_code("GetFullPath") {
        let path = argument_string(&arguments[0], descriptor.member)?;
        let base = arguments
            .get(1)
            .map(|argument| argument_string(argument, descriptor.member))
            .transpose()?;
        Ok(IntrinsicValue::String(host_get_full_path(path, base)?))
    } else if operation == member_code("GetPathRoot") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        if path_is_effectively_empty(host_path_style(), path) {
            return Ok(IntrinsicValue::Null);
        }
        let root_length = path_root_length(host_path_style(), path);
        Ok(IntrinsicValue::String(normalize_path_separators(
            host_path_style(),
            &path[..root_length],
        )))
    } else if operation == member_code("HasExtension") {
        Ok(IntrinsicValue::Boolean(
            argument_optional_string(&arguments[0], descriptor.member)?
                .is_some_and(|path| !path_get_extension(host_path_style(), path).is_empty()),
        ))
    } else if operation == member_code("ChangeExtension") {
        let Some(path) = argument_optional_string(&arguments[0], descriptor.member)? else {
            return Ok(IntrinsicValue::Null);
        };
        Ok(IntrinsicValue::String(path_change_extension(
            host_path_style(),
            path,
            argument_optional_string(&arguments[1], descriptor.member)?,
        )))
    } else if operation == member_code("GetTempPath") {
        Ok(IntrinsicValue::String(ensure_trailing_separator(
            display_path(&std::env::temp_dir()),
        )))
    } else if operation == member_code("DirectorySeparatorChar") {
        Ok(IntrinsicValue::Char(std::path::MAIN_SEPARATOR as u16))
    } else if operation == member_code("AltDirectorySeparatorChar") {
        Ok(IntrinsicValue::Char('/' as u16))
    } else if operation == member_code("PathSeparator") {
        Ok(IntrinsicValue::Char(
            (if cfg!(windows) { ';' } else { ':' }) as u16,
        ))
    } else {
        unreachable!("all registered path members are handled")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStyle {
    Windows,
    Unix,
}

const fn host_path_style() -> PathStyle {
    if cfg!(windows) {
        PathStyle::Windows
    } else {
        PathStyle::Unix
    }
}

const fn path_directory_separator(style: PathStyle) -> char {
    match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    }
}

fn path_is_separator(style: PathStyle, byte: u8) -> bool {
    match style {
        PathStyle::Windows => matches!(byte, b'\\' | b'/'),
        PathStyle::Unix => byte == b'/',
    }
}

fn windows_is_device(path: &[u8]) -> bool {
    (path.len() >= 4
        && path[0] == b'\\'
        && matches!(path[1], b'\\' | b'?')
        && path[2] == b'?'
        && path[3] == b'\\')
        || (path.len() >= 4
            && path_is_separator(PathStyle::Windows, path[0])
            && path_is_separator(PathStyle::Windows, path[1])
            && matches!(path[2], b'.' | b'?')
            && path_is_separator(PathStyle::Windows, path[3]))
}

fn windows_is_device_unc(path: &[u8]) -> bool {
    path.len() >= 8
        && windows_is_device(path)
        && path_is_separator(PathStyle::Windows, path[7])
        && path[4..7].eq_ignore_ascii_case(b"UNC")
}

fn path_root_length(style: PathStyle, path: &str) -> usize {
    let bytes = path.as_bytes();
    if style == PathStyle::Unix {
        return usize::from(bytes.first() == Some(&b'/'));
    }

    let mut index = 0;
    let device = windows_is_device(bytes);
    let device_unc = device && windows_is_device_unc(bytes);
    if (!device || device_unc)
        && bytes
            .first()
            .is_some_and(|byte| path_is_separator(style, *byte))
    {
        if device_unc || (bytes.len() > 1 && path_is_separator(style, bytes[1])) {
            index = if device_unc { 8 } else { 2 };
            let mut separators = 2;
            while index < bytes.len() {
                if path_is_separator(style, bytes[index]) {
                    separators -= 1;
                    if separators == 0 {
                        break;
                    }
                }
                index += 1;
            }
        } else {
            index = 1;
        }
    } else if device {
        index = 4;
        while index < bytes.len() && !path_is_separator(style, bytes[index]) {
            index += 1;
        }
        if index < bytes.len() && index > 4 {
            index += 1;
        }
    } else if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        index = 2;
        if bytes
            .get(2)
            .is_some_and(|byte| path_is_separator(style, *byte))
        {
            index += 1;
        }
    }
    index
}

fn path_is_rooted(style: PathStyle, path: &str) -> bool {
    let bytes = path.as_bytes();
    match style {
        PathStyle::Unix => bytes.first() == Some(&b'/'),
        PathStyle::Windows => {
            bytes
                .first()
                .is_some_and(|byte| path_is_separator(style, *byte))
                || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        }
    }
}

fn path_is_fully_qualified(style: PathStyle, path: &str) -> bool {
    if style == PathStyle::Unix {
        return path_is_rooted(style, path);
    }
    let bytes = path.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    if path_is_separator(style, bytes[0]) {
        return bytes[1] == b'?' || path_is_separator(style, bytes[1]);
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && path_is_separator(style, bytes[2])
}

fn path_is_effectively_empty(style: PathStyle, path: &str) -> bool {
    path.is_empty() || (style == PathStyle::Windows && path.bytes().all(|byte| byte == b' '))
}

fn normalize_path_separators(style: PathStyle, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let separator = path_directory_separator(style);
    let preserve_two_leading = style == PathStyle::Windows
        && path
            .as_bytes()
            .get(0..2)
            .is_some_and(|prefix| prefix.iter().all(|byte| path_is_separator(style, *byte)));
    let mut output = String::with_capacity(path.len());
    for character in path.chars() {
        let is_separator = character.is_ascii() && path_is_separator(style, character as u8);
        if !is_separator {
            output.push(character);
            continue;
        }
        if output.ends_with(separator) {
            if preserve_two_leading && output.len() == 1 {
                output.push(separator);
            }
            continue;
        }
        output.push(separator);
    }
    output
}

fn path_get_directory_name(style: PathStyle, path: &str) -> Option<String> {
    if path_is_effectively_empty(style, path) {
        return None;
    }
    let root_length = path_root_length(style, path);
    let mut end = path.len();
    if end <= root_length {
        return None;
    }
    while end > root_length {
        let (position, character) = path[..end]
            .char_indices()
            .next_back()
            .expect("end is nonzero");
        end = position;
        if character.is_ascii() && path_is_separator(style, character as u8) {
            break;
        }
    }
    while end > root_length {
        let (position, character) = path[..end]
            .char_indices()
            .next_back()
            .expect("end is nonzero");
        if !character.is_ascii() || !path_is_separator(style, character as u8) {
            break;
        }
        end = position;
    }
    Some(normalize_path_separators(style, &path[..end]))
}

fn path_get_file_name(style: PathStyle, path: &str) -> &str {
    let root_length = path_root_length(style, path);
    let separator = path
        .bytes()
        .enumerate()
        .rev()
        .find(|(_, byte)| path_is_separator(style, *byte))
        .map(|(index, _)| index);
    let start = separator.map_or(root_length, |index| {
        if index < root_length {
            root_length
        } else {
            index + 1
        }
    });
    &path[start..]
}

fn path_get_extension(style: PathStyle, path: &str) -> &str {
    for (index, byte) in path.bytes().enumerate().rev() {
        if byte == b'.' {
            return if index == path.len() - 1 {
                ""
            } else {
                &path[index..]
            };
        }
        if path_is_separator(style, byte) {
            break;
        }
    }
    ""
}

fn path_change_extension(style: PathStyle, path: &str, extension: Option<&str>) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut sub_length = path.len();
    for (index, byte) in path.bytes().enumerate().rev() {
        if byte == b'.' {
            sub_length = index;
            break;
        }
        if path_is_separator(style, byte) {
            break;
        }
    }
    let Some(extension) = extension else {
        return path[..sub_length].to_string();
    };
    if extension.starts_with('.') {
        format!("{}{extension}", &path[..sub_length])
    } else {
        format!("{}.{extension}", &path[..sub_length])
    }
}

fn path_combine(style: PathStyle, paths: &[&str]) -> String {
    let first_component = paths
        .iter()
        .enumerate()
        .filter(|(_, path)| !path.is_empty() && path_is_rooted(style, path))
        .map(|(index, _)| index)
        .next_back()
        .unwrap_or(0);
    let mut output = String::new();
    for path in &paths[first_component..] {
        if path.is_empty() {
            continue;
        }
        if !output.is_empty()
            && !output
                .as_bytes()
                .last()
                .is_some_and(|byte| path_is_separator(style, *byte))
        {
            output.push(path_directory_separator(style));
        }
        output.push_str(path);
    }
    output
}

fn host_combine(paths: &[&str]) -> String {
    path_combine(host_path_style(), paths)
}

fn path_volume_name(style: PathStyle, path: &str) -> &str {
    let root_length = path_root_length(style, path);
    let root = &path[..root_length];
    root.strip_suffix(['/', '\\']).unwrap_or(root)
}

fn path_roots_equal(style: PathStyle, left: &str, right: &str) -> bool {
    let left = path_volume_name(style, left);
    let right = path_volume_name(style, right);
    if style == PathStyle::Windows {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn path_join(style: PathStyle, left: &str, right: &str) -> String {
    if left.is_empty() {
        return right.to_string();
    }
    if right.is_empty() {
        return left.to_string();
    }
    let mut output = left.to_string();
    if !left
        .as_bytes()
        .last()
        .is_some_and(|byte| path_is_separator(style, *byte))
        && !right
            .as_bytes()
            .first()
            .is_some_and(|byte| path_is_separator(style, *byte))
    {
        output.push(path_directory_separator(style));
    }
    output.push_str(right);
    output
}

fn resolve_path_against_base(
    style: PathStyle,
    path: &str,
    base: &str,
    use_drive_environment: bool,
) -> Result<String> {
    if style == PathStyle::Unix {
        return Ok(path_join(style, base, path));
    }
    let bytes = path.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| path_is_separator(style, *byte))
    {
        let root = &base[..path_root_length(style, base)];
        return Ok(path_join(style, root, &path[1..]));
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if path_roots_equal(style, path, base) {
            return Ok(path_join(style, base, &path[2..]));
        }
        if use_drive_environment {
            let variable = format!("={}:", bytes[0] as char);
            if let Some(directory) = std::env::var_os(variable) {
                let directory = directory.to_string_lossy();
                if path_is_fully_qualified(style, &directory)
                    && path_roots_equal(style, path, &directory)
                {
                    return Ok(path_join(style, &directory, &path[2..]));
                }
            }
        }
        return Ok(format!(
            "{}\\{}",
            &path[..2],
            path[2..].trim_start_matches(['/', '\\'])
        ));
    }
    Ok(path_join(style, base, path))
}

fn normalize_full_path(style: PathStyle, path: &str) -> Result<String> {
    if style == PathStyle::Windows
        && path
            .as_bytes()
            .get(0..4)
            .is_some_and(|prefix| prefix == br"\\?\")
    {
        return Ok(path.to_string());
    }
    if style == PathStyle::Windows && windows_is_device(path.as_bytes()) {
        bail!(
            "Device paths outside the canonical \\\\?\\ form are not supported by the native tier"
        );
    }
    #[cfg(windows)]
    if style == PathStyle::Windows {
        return windows_normalize_full_path(path);
    }
    let normalized = normalize_path_separators(style, path);
    let root_length = path_root_length(style, &normalized);
    if root_length == 0 {
        bail!("Path '{path}' is not fully qualified");
    }
    let separator = path_directory_separator(style);
    let trailing_separator = normalized.ends_with(separator);
    let mut components = Vec::new();
    for component in normalized[root_length..].split(separator) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    let mut output = normalized[..root_length].to_string();
    for component in components {
        if !output.ends_with(separator) {
            output.push(separator);
        }
        output.push_str(component);
    }
    if trailing_separator && !output.ends_with(separator) {
        output.push(separator);
    }
    if output.is_empty() {
        output.push(separator);
    }
    Ok(output)
}

#[cfg(windows)]
fn windows_normalize_full_path(path: &str) -> Result<String> {
    let input = path.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut output = vec![0u16; 260];
    loop {
        // GetFullPathNameW only performs lexical Win32 normalization; it does
        // not resolve links or require the path to exist.
        let length = unsafe {
            GetFullPathNameW(
                input.as_ptr(),
                output
                    .len()
                    .try_into()
                    .context("Path buffer is too large")?,
                output.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if length == 0 {
            return Err(std::io::Error::last_os_error()).context("Path.GetFullPath failed");
        }
        let length = usize::try_from(length).context("Path length does not fit usize")?;
        if length < output.len() {
            return String::from_utf16(&output[..length])
                .context("Path.GetFullPath returned invalid UTF-16");
        }
        output.resize(length.saturating_add(1), 0);
    }
}

fn get_full_path(
    style: PathStyle,
    path: &str,
    base: Option<&str>,
    current_directory: &str,
) -> Result<String> {
    if path.contains('\0') || base.is_some_and(|base| base.contains('\0')) {
        bail!("Path contains a null character");
    }
    let combined = if let Some(base) = base {
        if !path_is_fully_qualified(style, base) {
            bail!("Base path '{base}' is not fully qualified");
        }
        if style == PathStyle::Windows
            && base
                .as_bytes()
                .get(0..4)
                .is_some_and(|prefix| prefix == br"\\?\")
            && !path_is_fully_qualified(style, path)
            && !path_is_effectively_empty(style, path)
        {
            bail!("Relative resolution against a Windows device base is outside the native tier");
        }
        if path_is_fully_qualified(style, path) {
            path.to_string()
        } else if path_is_effectively_empty(style, path) {
            return Ok(base.to_string());
        } else {
            resolve_path_against_base(style, path, base, false)?
        }
    } else {
        if path_is_effectively_empty(style, path) {
            bail!("Path cannot be empty");
        }
        if path_is_fully_qualified(style, path) {
            path.to_string()
        } else {
            if !path_is_fully_qualified(style, current_directory) {
                bail!("Current directory '{current_directory}' is not fully qualified");
            }
            resolve_path_against_base(style, path, current_directory, true)?
        }
    };
    normalize_full_path(style, &combined)
}

fn host_get_full_path(path: &str, base: Option<&str>) -> Result<String> {
    let current_directory = std::env::current_dir()
        .context("Could not read the current directory for Path.GetFullPath")?;
    get_full_path(
        host_path_style(),
        path,
        base,
        &display_path(&current_directory),
    )
}

fn fix_file_path(value: &str) -> String {
    if host_path_style() == PathStyle::Unix {
        value.replace('\\', "/")
    } else {
        value.to_string()
    }
}

fn ensure_trailing_separator(mut value: String) -> String {
    if host_path_style() == PathStyle::Unix {
        value = value.replace('\\', "/");
    }
    if !value.is_empty()
        && !value
            .as_bytes()
            .last()
            .is_some_and(|byte| path_is_separator(host_path_style(), *byte))
    {
        value.push(path_directory_separator(host_path_style()));
    }
    value
}

fn handle_math(
    _: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    match &arguments[0].value {
        IntrinsicValue::Int16(value) => value
            .checked_abs()
            .map(IntrinsicValue::Int16)
            .ok_or_else(|| anyhow!("System.Math.Abs overflowed its Int16 argument")),
        IntrinsicValue::Int32(value) => value
            .checked_abs()
            .map(IntrinsicValue::Int32)
            .ok_or_else(|| anyhow!("System.Math.Abs overflowed its Int32 argument")),
        IntrinsicValue::Int64(value) => value
            .checked_abs()
            .map(IntrinsicValue::Int64)
            .ok_or_else(|| anyhow!("System.Math.Abs overflowed its Int64 argument")),
        _ => bail!("System.Math.Abs received a nonintegral coerced argument"),
    }
}

fn handle_environment(
    descriptor: &IntrinsicDescriptor,
    context: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("ExpandEnvironmentVariables") {
        let environment = environment_snapshot(context);
        Ok(IntrinsicValue::String(expand_environment_variables(
            argument_string(&arguments[0], descriptor.member)?,
            &environment,
        )))
    } else if operation == member_code("GetEnvironmentVariable") {
        let environment = environment_snapshot(context);
        Ok(environment
            .get(&environment_key(argument_string(
                &arguments[0],
                descriptor.member,
            )?))
            .cloned()
            .map(IntrinsicValue::String)
            .unwrap_or(IntrinsicValue::Null))
    } else if operation == member_code("NewLine") {
        Ok(IntrinsicValue::String(
            if cfg!(windows) { "\r\n" } else { "\n" }.to_string(),
        ))
    } else if operation == member_code("Is64BitProcess") {
        Ok(IntrinsicValue::Boolean(usize::BITS == 64))
    } else {
        Ok(IntrinsicValue::Int32(
            std::thread::available_parallelism()
                .map(|count| i32::try_from(count.get()).unwrap_or(i32::MAX))
                .unwrap_or(1),
        ))
    }
}

fn handle_tool_location(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let identifier = argument_optional_string(&arguments[0], descriptor.member)?;
    let version = argument_optional_string(&arguments[1], descriptor.member)?;
    if identifier.is_none_or(str::is_empty) && version.is_none_or(str::is_empty) {
        Ok(IntrinsicValue::String(String::new()))
    } else {
        bail!(
            "{} is retained only for the empty platform probe used by ordinary SDK-style projects",
            descriptor.member
        )
    }
}

fn environment_key_for_platform(name: &str, windows_case_insensitive: bool) -> Vec<u16> {
    if windows_case_insensitive {
        dotnet_ordinal_ignore_case_key(name)
    } else {
        name.encode_utf16().collect()
    }
}

fn environment_key(name: &str) -> Vec<u16> {
    environment_key_for_platform(name, cfg!(windows))
}

fn environment_snapshot(context: &IntrinsicContext<'_>) -> HashMap<Vec<u16>, String> {
    if let Some(environment) = context.environment {
        return environment
            .iter()
            .map(|(name, value)| (environment_key(name), value.to_string()))
            .collect();
    }
    std::env::vars_os()
        .map(|(name, value)| {
            (
                environment_key(&name.to_string_lossy()),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn expand_environment_variables(input: &str, environment: &HashMap<Vec<u16>, String>) -> String {
    expand_environment_variables_for_platform(input, environment, cfg!(windows))
}

fn expand_environment_variables_for_platform(
    input: &str,
    environment: &HashMap<Vec<u16>, String>,
    windows_case_insensitive: bool,
) -> String {
    let mut output = String::with_capacity(input.len());
    let mut last_position = 0;
    while last_position < input.len() {
        let next_character = input[last_position..]
            .chars()
            .next()
            .expect("last_position is in bounds");
        let search_start = last_position + next_character.len_utf8();
        let Some(relative_position) = input[search_start..].find('%') else {
            break;
        };
        let position = search_start + relative_position;
        if next_character == '%' {
            let name = &input[search_start..position];
            if let Some(value) = environment.get(&environment_key_for_platform(
                name,
                windows_case_insensitive,
            )) {
                output.push_str(value);
                last_position = position + 1;
                continue;
            }
        }
        output.push_str(&input[last_position..position]);
        last_position = position;
    }
    output.push_str(&input[last_position..]);
    output
}

fn handle_convert(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let member = descriptor.member;
    let operation = descriptor.dispatch_code;
    let radix = arguments
        .get(1)
        .map(|argument| argument_i32(argument, member))
        .transpose()?;
    if operation == member_code("ToInt32") {
        Ok(IntrinsicValue::Int32(if let Some(radix) = radix {
            parse_radix_i32(argument_string(&arguments[0], member)?, radix)?
        } else {
            convert_to_i32(&arguments[0].value)?
        }))
    } else if operation == member_code("ToInt64") {
        Ok(IntrinsicValue::Int64(if let Some(radix) = radix {
            parse_radix_i64(argument_string(&arguments[0], member)?, radix)?
        } else {
            convert_to_i64(&arguments[0].value)?
        }))
    } else if operation == member_code("ToUInt64") {
        Ok(IntrinsicValue::UInt64(if let Some(radix) = radix {
            parse_radix_u64(argument_string(&arguments[0], member)?, radix)?
        } else {
            convert_to_u64(&arguments[0].value)?
        }))
    } else if operation == member_code("ToBoolean") {
        Ok(IntrinsicValue::Boolean(convert_to_bool(
            &arguments[0].value,
            member,
        )?))
    } else if operation == member_code("ToString") {
        if let Some(radix) = radix {
            Ok(IntrinsicValue::String(format_radix(
                &arguments[0].value,
                radix,
            )?))
        } else {
            Ok(IntrinsicValue::String(match &arguments[0].value {
                IntrinsicValue::String(value) => value.clone(),
                IntrinsicValue::Boolean(value) => dotnet_bool(*value).to_string(),
                IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => String::new(),
                value => value.to_msbuild_string()?,
            }))
        }
    } else {
        unreachable!("all registered Convert members are handled")
    }
}

fn handle_version(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    if descriptor.kind == InvocationKind::StaticMethod {
        return Ok(IntrinsicValue::Version(NativeVersion::parse(
            argument_string(&arguments[0], descriptor.member)?,
        )?));
    }
    if descriptor.kind == InvocationKind::Constructor {
        return Ok(IntrinsicValue::Version(match arguments {
            [] => NativeVersion {
                parts: [0, 0, -1, -1],
                count: 2,
            },
            [argument] => NativeVersion::parse(argument_string(argument, descriptor.member)?)?,
            _ => NativeVersion::from_arguments(arguments)?,
        }));
    }
    let Some(IntrinsicValue::Version(version)) = receiver else {
        bail!("{} requires a System.Version receiver", descriptor.member);
    };
    let operation = descriptor.dispatch_code;
    if operation == member_code("Major") {
        Ok(IntrinsicValue::Int32(version.parts[0]))
    } else if operation == member_code("Minor") {
        Ok(IntrinsicValue::Int32(version.parts[1]))
    } else if operation == member_code("Build") {
        Ok(IntrinsicValue::Int32(version.parts[2]))
    } else if operation == member_code("Revision") {
        Ok(IntrinsicValue::Int32(version.parts[3]))
    } else if operation == member_code("ToString") {
        let fields = arguments
            .first()
            .map(|argument| argument_usize(argument, descriptor.member))
            .transpose()?
            .unwrap_or(version.count);
        Ok(IntrinsicValue::String(version.format_fields(fields)?))
    } else {
        let other = match &arguments[0].value {
            IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => {
                return Ok(if operation == member_code("CompareTo") {
                    IntrinsicValue::Int32(1)
                } else {
                    IntrinsicValue::Boolean(false)
                });
            }
            IntrinsicValue::Version(value) => value,
            _ => bail!("{} requires a Version argument", descriptor.member),
        };
        let ordering = version.parts.cmp(&other.parts);
        if operation == member_code("CompareTo") {
            Ok(IntrinsicValue::Int32(ordering_i32(ordering)))
        } else {
            Ok(IntrinsicValue::Boolean(ordering.is_eq()))
        }
    }
}

fn handle_guid(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("Parse") {
        return Ok(IntrinsicValue::Guid(parse_guid(argument_string(
            &arguments[0],
            descriptor.member,
        )?)?));
    }
    if operation == member_code("NewGuid") {
        return Ok(IntrinsicValue::Guid(Uuid::new_v4()));
    }
    if operation == member_code("Empty") {
        return Ok(IntrinsicValue::Guid(Uuid::nil()));
    }
    let Some(IntrinsicValue::Guid(guid)) = receiver else {
        bail!("{} requires a System.Guid receiver", descriptor.member);
    };
    let format = match arguments.first().map(|argument| &argument.value) {
        None | Some(IntrinsicValue::Null | IntrinsicValue::TypedNull(_)) => "D",
        Some(IntrinsicValue::String(value)) => value.as_str(),
        Some(_) => bail!("System.Guid.ToString format must be a String"),
    };
    let text = if format.eq_ignore_ascii_case("N") {
        guid.simple().to_string()
    } else if format.eq_ignore_ascii_case("D") {
        guid.hyphenated().to_string()
    } else if format.eq_ignore_ascii_case("B") {
        format!("{{{}}}", guid.hyphenated())
    } else if format.eq_ignore_ascii_case("P") {
        format!("({})", guid.hyphenated())
    } else if format.eq_ignore_ascii_case("X") {
        let (a, b, c, d) = guid.as_fields();
        format!(
            "{{0x{a:08x},0x{b:04x},0x{c:04x},{{0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x},0x{:02x}}}}}",
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    } else {
        bail!("MSB4184: Unsupported System.Guid format '{format}'")
    };
    Ok(IntrinsicValue::String(text))
}

fn handle_datetime(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    if descriptor.kind == InvocationKind::StaticMethod {
        return Ok(IntrinsicValue::DateTime(NativeDateTime::parse(
            argument_string(&arguments[0], descriptor.member)?,
        )?));
    }
    let Some(IntrinsicValue::DateTime(value)) = receiver else {
        bail!("{} requires a System.DateTime receiver", descriptor.member);
    };
    Ok(IntrinsicValue::String(value.format(argument_string(
        &arguments[0],
        descriptor.member,
    )?)?))
}

fn handle_integer_static(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let is_i32 = descriptor.type_name.eq_ignore_ascii_case("System.Int32");
    let operation = descriptor.dispatch_code;
    if operation == member_code("Parse") {
        let value = argument_string(&arguments[0], descriptor.member)?;
        if is_i32 {
            Ok(IntrinsicValue::Int32(parse_decimal_i32(value)?))
        } else {
            Ok(IntrinsicValue::Int64(parse_decimal_i64(value)?))
        }
    } else if operation == member_code("MaxValue") {
        Ok(if is_i32 {
            IntrinsicValue::Int32(i32::MAX)
        } else {
            IntrinsicValue::Int64(i64::MAX)
        })
    } else {
        Ok(if is_i32 {
            IntrinsicValue::Int32(i32::MIN)
        } else {
            IntrinsicValue::Int64(i64::MIN)
        })
    }
}

fn handle_numeric_instance(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let receiver = receiver.ok_or_else(|| anyhow!("numeric member requires a receiver"))?;
    let operation = descriptor.dispatch_code;
    if operation == member_code("ToString") {
        let format = arguments
            .first()
            .map(|argument| argument_string(argument, descriptor.member))
            .transpose()?;
        return Ok(IntrinsicValue::String(format_numeric(receiver, format)?));
    }
    if operation == member_code("CompareTo") {
        Ok(IntrinsicValue::Int32(ordering_i32(numeric_compare(
            receiver,
            &arguments[0].value,
        )?)))
    } else {
        Ok(IntrinsicValue::Boolean(numeric_equals(
            receiver,
            &arguments[0].value,
        )?))
    }
}

fn handle_boolean_instance(
    _: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    _: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let Some(IntrinsicValue::Boolean(value)) = receiver else {
        bail!("Boolean.ToString requires a System.Boolean receiver");
    };
    Ok(IntrinsicValue::String(dotnet_bool(*value).to_string()))
}

fn handle_msbuild(
    descriptor: &IntrinsicDescriptor,
    context: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let member = descriptor.member;
    let operation = descriptor.dispatch_code;
    if operation == member_code("AreFeaturesEnabled") {
        let IntrinsicValue::Version(wave) = &arguments[0].value else {
            unreachable!("AreFeaturesEnabled is coerced to System.Version");
        };
        let configured = context
            .disable_features_from_version
            .map(ToOwned::to_owned)
            .or_else(|| {
                environment_snapshot(context)
                    .get(&environment_key("MSBuildDisableFeaturesFromVersion"))
                    .cloned()
            });
        let disabled = NativeVersion::parse(&resolve_feature_wave(configured.as_deref()).version)?;
        Ok(IntrinsicValue::Boolean(wave < &disabled))
    } else if operation == member_code("IsRunningFromVisualStudio") {
        Ok(IntrinsicValue::Boolean(false))
    } else if operation == member_code("IsOSPlatform") {
        let requested = argument_string(&arguments[0], member)?;
        if requested.is_empty() {
            bail!("IsOSPlatform platform cannot be empty");
        }
        let platform = match std::env::consts::OS {
            "windows" => "Windows",
            "linux" => "Linux",
            "macos" => "OSX",
            _ => "",
        };
        Ok(IntrinsicValue::Boolean(
            requested.eq_ignore_ascii_case(platform),
        ))
    } else if operation == member_code("IsOSUnixLike") {
        Ok(IntrinsicValue::Boolean(!cfg!(windows)))
    } else if matches!(
        operation,
        value
            if value == member_code("VersionGreaterThan")
                || value == member_code("VersionGreaterThanOrEquals")
                || value == member_code("VersionLessThan")
                || value == member_code("VersionLessThanOrEquals")
                || value == member_code("VersionEquals")
    ) {
        let ordering = compare_sdk_versions(
            argument_string(&arguments[0], member)?,
            argument_string(&arguments[1], member)?,
        )?;
        let result = if operation == member_code("VersionGreaterThan") {
            ordering.is_gt()
        } else if operation == member_code("VersionGreaterThanOrEquals") {
            !ordering.is_lt()
        } else if operation == member_code("VersionLessThan") {
            ordering.is_lt()
        } else if operation == member_code("VersionLessThanOrEquals") {
            !ordering.is_gt()
        } else {
            ordering.is_eq()
        };
        Ok(IntrinsicValue::Boolean(result))
    } else if matches!(
        operation,
        value
            if value == member_code("GetTargetFrameworkIdentifier")
                || value == member_code("GetTargetFrameworkVersion")
                || value == member_code("GetTargetPlatformIdentifier")
                || value == member_code("GetTargetPlatformVersion")
    ) {
        let framework = parse_target_framework(argument_string(&arguments[0], member)?)?;
        let value = if operation == member_code("GetTargetFrameworkIdentifier") {
            framework.identifier
        } else if operation == member_code("GetTargetPlatformIdentifier") {
            framework.platform_identifier.unwrap_or_default()
        } else {
            let minimum_parts = arguments
                .get(1)
                .map(|argument| argument_i32(argument, member))
                .transpose()?
                .unwrap_or(2);
            let minimum_parts = usize::try_from(minimum_parts)
                .ok()
                .filter(|count| (1..=4).contains(count))
                .ok_or_else(|| anyhow!("{member} version part count must be between 1 and 4"))?;
            if operation == member_code("GetTargetFrameworkVersion") {
                format_short_version(framework.version, minimum_parts)
            } else {
                format_short_version(
                    framework.platform_version.unwrap_or_else(|| vec![0, 0]),
                    minimum_parts,
                )
            }
        };
        Ok(IntrinsicValue::String(value))
    } else if operation == member_code("GetDirectoryNameOfFileAbove") {
        Ok(IntrinsicValue::String(
            find_file_above(
                argument_string(&arguments[0], member)?,
                argument_string(&arguments[1], member)?,
            )?
            .and_then(|path| path.parent().map(display_path))
            .unwrap_or_default(),
        ))
    } else if operation == member_code("GetPathOfFileAbove") {
        let file_name = argument_string(&arguments[0], member)?;
        if file_name
            .bytes()
            .any(|byte| path_is_separator(host_path_style(), byte))
        {
            bail!(
                "GetPathOfFileAbove file name '{file_name}' cannot include a directory separator"
            );
        }
        Ok(IntrinsicValue::String(
            find_file_above(argument_string(&arguments[1], member)?, file_name)?
                .map(|path| display_path(&path))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("MakeRelative") {
        Ok(IntrinsicValue::String(make_relative(
            argument_string(&arguments[0], member)?,
            argument_string(&arguments[1], member)?,
        )?))
    } else if operation == member_code("NormalizePath")
        || operation == member_code("NormalizeDirectory")
    {
        let paths = arguments
            .iter()
            .map(|argument| argument_string(argument, member))
            .collect::<Result<Vec<_>>>()?;
        let value = fix_file_path(&host_get_full_path(&host_combine(&paths), None)?);
        Ok(IntrinsicValue::String(
            if operation == member_code("NormalizeDirectory") {
                ensure_trailing_separator(value)
            } else {
                value
            },
        ))
    } else if operation == member_code("EnsureTrailingSlash") {
        let value = argument_string(&arguments[0], member)?;
        Ok(IntrinsicValue::String(if value.is_empty() {
            String::new()
        } else {
            ensure_trailing_separator(fix_file_path(value))
        }))
    } else if matches!(
        operation,
        value
            if value == member_code("Add")
                || value == member_code("Subtract")
                || value == member_code("Multiply")
                || value == member_code("Divide")
                || value == member_code("Modulo")
    ) {
        arithmetic(descriptor, arguments)
    } else if matches!(
        operation,
        value
            if value == member_code("BitwiseOr")
                || value == member_code("BitwiseAnd")
                || value == member_code("BitwiseXor")
                || value == member_code("LeftShift")
                || value == member_code("RightShift")
    ) {
        let left = argument_i32(&arguments[0], member)?;
        let right = argument_i32(&arguments[1], member)?;
        Ok(IntrinsicValue::Int32(
            if operation == member_code("BitwiseOr") {
                left | right
            } else if operation == member_code("BitwiseAnd") {
                left & right
            } else if operation == member_code("BitwiseXor") {
                left ^ right
            } else if operation == member_code("LeftShift") {
                left.wrapping_shl((right & 0x1f) as u32)
            } else {
                left.wrapping_shr((right & 0x1f) as u32)
            },
        ))
    } else if operation == member_code("BitwiseNot") {
        Ok(IntrinsicValue::Int32(!argument_i32(&arguments[0], member)?))
    } else if operation == member_code("ValueOrDefault") {
        let first = argument_optional_string(&arguments[0], member)?;
        let fallback = argument_optional_string(&arguments[1], member)?.unwrap_or_default();
        Ok(IntrinsicValue::String(if first.is_none_or(str::is_empty) {
            fallback.to_string()
        } else {
            first.unwrap_or_default().to_string()
        }))
    } else if operation == member_code("Escape") {
        Ok(IntrinsicValue::String(escape_lower(argument_string(
            &arguments[0],
            member,
        )?)))
    } else if operation == member_code("Unescape") {
        Ok(IntrinsicValue::String(unescape_once(argument_string(
            &arguments[0],
            member,
        )?)))
    } else if operation == member_code("DoesTaskHostExist") {
        does_task_host_exist(context, arguments)
    } else if operation == member_code("GetToolsDirectory32") {
        Ok(IntrinsicValue::String(
            context.tools_directory.unwrap_or_default().to_string(),
        ))
    } else if operation == member_code("SubstringByAsciiChars") {
        substring_by_ascii_chars(arguments)
    } else if operation == member_code("StableStringHash") {
        stable_string_hash(arguments)
    } else if operation == member_code("GetRegistryValue") {
        registry_intrinsic(arguments, false)
    } else if operation == member_code("GetRegistryValueFromView") {
        registry_intrinsic(arguments, true)
    } else {
        unreachable!("all registered MSBuild members are handled")
    }
}

fn arithmetic(
    descriptor: &IntrinsicDescriptor,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let member = descriptor.member;
    let operation = descriptor.dispatch_code;
    let left = argument_i64(&arguments[0], member)?;
    let right = argument_i64(&arguments[1], member)?;
    let value = if operation == member_code("Add") {
        left.wrapping_add(right)
    } else if operation == member_code("Subtract") {
        left.wrapping_sub(right)
    } else if operation == member_code("Multiply") {
        left.wrapping_mul(right)
    } else {
        if right == 0 {
            bail!("{member} divided by zero");
        }
        if left == i64::MIN && right == -1 {
            bail!("{member} overflowed");
        }
        if operation == member_code("Divide") {
            left / right
        } else {
            left % right
        }
    };
    Ok(IntrinsicValue::Int64(value))
}

fn registry_intrinsic(
    arguments: &[IntrinsicArgument],
    views_supplied: bool,
) -> Result<IntrinsicValue> {
    registry_intrinsic_with_reader(
        arguments,
        views_supplied,
        cfg!(windows),
        read_registry_value,
    )
}

fn registry_intrinsic_with_reader(
    arguments: &[IntrinsicArgument],
    views_supplied: bool,
    is_windows: bool,
    mut reader: impl FnMut(&str, Option<&str>, RegistryView) -> Result<RegistryReadResult>,
) -> Result<IntrinsicValue> {
    let default = arguments
        .get(2)
        .map(|argument| argument.value.clone())
        .unwrap_or(IntrinsicValue::Null);

    // The .NET Core compatibility checks are the first statements in both
    // intrinsics. In particular, invalid hives/views and null key names are
    // not validated on non-Windows.
    if !is_windows {
        return Ok(default);
    }

    let member = if views_supplied {
        "GetRegistryValueFromView"
    } else {
        "GetRegistryValue"
    };

    if !views_supplied {
        let key = argument_optional_string(&arguments[0], member)?
            .ok_or_else(|| anyhow!("MSB4184: {member} registry key name cannot be null"))?;
        let value_name =
            argument_optional_string(&arguments[1], member)?.filter(|name| !name.is_empty());
        return Ok(registry_data_to_intrinsic(
            match reader(key, value_name, RegistryView::Default)? {
                RegistryReadResult::KeyMissing => return Ok(IntrinsicValue::Null),
                RegistryReadResult::ValueMissing => return Ok(default),
                RegistryReadResult::Value(value) => value,
            },
        ));
    }

    let mut result = default;
    for argument in &arguments[3..] {
        // IntrinsicFunctions only processes string objects in its params
        // object[]. Its synthesized default is a boxed RegistryView, so an
        // omitted view is ignored and the supplied default is returned.
        let IntrinsicValue::String(value) = &argument.value else {
            continue;
        };
        let view = RegistryView::parse(value)?;
        // Parsing is deliberately per-view and precedes key validation.
        // Finding a value stops the loop before later views are validated.
        let key = argument_optional_string(&arguments[0], member)?
            .ok_or_else(|| anyhow!("MSB4184: {member} registry key name cannot be null"))?;
        let value_name =
            argument_optional_string(&arguments[1], member)?.filter(|name| !name.is_empty());
        match reader(key, value_name, view)? {
            RegistryReadResult::KeyMissing => {}
            RegistryReadResult::ValueMissing => result = IntrinsicValue::Null,
            RegistryReadResult::Value(value) => return Ok(registry_data_to_intrinsic(value)),
        }
    }
    Ok(result)
}

fn registry_data_to_intrinsic(value: RegistryData) -> IntrinsicValue {
    match value {
        RegistryData::String(value) => IntrinsicValue::String(value),
        RegistryData::DWord(value) => IntrinsicValue::Int32(value),
        RegistryData::QWord(value) => IntrinsicValue::Int64(value),
        RegistryData::MultiString(values) => IntrinsicValue::Strings(values),
        RegistryData::Binary(values) => IntrinsicValue::Bytes(values),
    }
}

fn stable_string_hash(arguments: &[IntrinsicArgument]) -> Result<IntrinsicValue> {
    let algorithm = arguments
        .get(1)
        .map(|argument| argument_string(argument, "StableStringHash"))
        .transpose()?
        .unwrap_or("Legacy");
    let value = argument_string(&arguments[0], "StableStringHash")?;
    if algorithm.eq_ignore_ascii_case("Sha256") {
        let digest = Sha256::digest(value.as_bytes());
        Ok(IntrinsicValue::String(format!("{digest:x}")))
    } else if algorithm.eq_ignore_ascii_case("Legacy") {
        let mut hash1 = (5381i32 << 16) + 5381;
        let mut hash2 = hash1;
        let units = value.encode_utf16().collect::<Vec<_>>();
        let mut index = 0usize;
        let mut remaining = units.len() as isize;
        while remaining > 0 {
            hash1 = legacy_hash_mix(hash1, packed_utf16(&units, index));
            if remaining <= 2 {
                break;
            }
            hash2 = legacy_hash_mix(hash2, packed_utf16(&units, index + 2));
            index += 4;
            remaining -= 4;
        }
        Ok(IntrinsicValue::Int32(
            hash1.wrapping_add(hash2.wrapping_mul(1_566_083_941)),
        ))
    } else if algorithm.eq_ignore_ascii_case("Fnv1a32bit") {
        let mut hash = 2_166_136_261u32;
        for unit in value.encode_utf16() {
            hash ^= u32::from(unit & 0xff);
            hash = hash.wrapping_mul(16_777_619);
            hash ^= u32::from(unit >> 8);
            hash = hash.wrapping_mul(16_777_619);
        }
        Ok(IntrinsicValue::Int32(hash as i32))
    } else if algorithm.eq_ignore_ascii_case("Fnv1a32bitFast") {
        let mut hash = 2_166_136_261u32;
        for unit in value.encode_utf16() {
            hash = (hash ^ u32::from(unit)).wrapping_mul(16_777_619);
        }
        Ok(IntrinsicValue::Int32(hash as i32))
    } else {
        bail!("MSB4184: Unsupported StableStringHash algorithm '{algorithm}'")
    }
}

fn packed_utf16(units: &[u16], index: usize) -> i32 {
    i32::from(units.get(index).copied().unwrap_or_default())
        | (i32::from(units.get(index + 1).copied().unwrap_or_default()) << 16)
}

fn legacy_hash_mix(hash: i32, value: i32) -> i32 {
    hash.wrapping_shl(5)
        .wrapping_add(hash)
        .wrapping_add(hash >> 27)
        ^ value
}

fn does_task_host_exist(
    context: &IntrinsicContext<'_>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let runtime = argument_optional_string(&arguments[0], "DoesTaskHostExist")?
        .map(str::trim)
        .unwrap_or("*");
    let architecture = argument_optional_string(&arguments[1], "DoesTaskHostExist")?
        .map(str::trim)
        .unwrap_or("*");
    if !["CLR2", "CLR4", "CurrentRuntime", "NET", "*"]
        .iter()
        .any(|value| runtime.eq_ignore_ascii_case(value))
    {
        bail!("DoesTaskHostExist received invalid runtime '{runtime}'");
    }
    if !["x86", "x64", "arm64", "CurrentArchitecture", "*"]
        .iter()
        .any(|value| architecture.eq_ignore_ascii_case(value))
    {
        bail!("DoesTaskHostExist received invalid architecture '{architecture}'");
    }

    let current_runtime = match context.runtime_type {
        Some(value) if value.eq_ignore_ascii_case("Core") => "NET",
        Some(value) if value.eq_ignore_ascii_case("Full") => "CLR4",
        _ => {
            if runtime.eq_ignore_ascii_case("*") || runtime.eq_ignore_ascii_case("CurrentRuntime") {
                bail!("DoesTaskHostExist requires an active MSBuild runtime");
            }
            runtime
        }
    };
    let runtime =
        if runtime.eq_ignore_ascii_case("*") || runtime.eq_ignore_ascii_case("CurrentRuntime") {
            current_runtime
        } else {
            runtime
        };
    let current_architecture = match std::env::consts::ARCH {
        "x86" => "x86",
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ if usize::BITS == 64 => "x64",
        _ => "x86",
    };
    let architecture = if architecture.eq_ignore_ascii_case("*")
        || architecture.eq_ignore_ascii_case("CurrentArchitecture")
    {
        current_architecture
    } else {
        architecture
    };

    let tools_directory = context
        .tools_directory
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("DoesTaskHostExist requires an active MSBuild toolset"))?;
    let environment = environment_snapshot(context);
    if runtime.eq_ignore_ascii_case("CLR2") && architecture.eq_ignore_ascii_case("arm64") {
        return Ok(IntrinsicValue::Boolean(false));
    }
    let executable = if runtime.eq_ignore_ascii_case("CLR2") {
        environment
            .get(&environment_key("MSBUILDTASKHOST_EXE_NAME"))
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("MSBuildTaskHost.exe")
    } else {
        environment
            .get(&environment_key("MSBUILD_EXE_NAME"))
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(if cfg!(windows) {
                "MSBuild.exe"
            } else {
                "MSBuild"
            })
    };
    let override_directory = if runtime.eq_ignore_ascii_case("CLR2") {
        let variable = if architecture.eq_ignore_ascii_case("x64") {
            "MSBUILDTASKHOSTLOCATION64"
        } else {
            "MSBUILDTASKHOSTLOCATION"
        };
        environment
            .get(&environment_key(variable))
            .filter(|value| !value.is_empty())
            .map(String::as_str)
    } else {
        None
    };
    let candidate = override_directory
        .map(Path::new)
        .unwrap_or_else(|| Path::new(tools_directory))
        .join(executable);
    Ok(IntrinsicValue::Boolean(
        candidate.is_file()
            || (override_directory.is_some()
                && Path::new(tools_directory).join(executable).is_file()),
    ))
}

fn substring_by_ascii_chars(arguments: &[IntrinsicArgument]) -> Result<IntrinsicValue> {
    let input = argument_string(&arguments[0], "SubstringByAsciiChars")?;
    let start_i32 = argument_i32(&arguments[1], "SubstringByAsciiChars")?;
    let length_i32 = argument_i32(&arguments[2], "SubstringByAsciiChars")?;
    let start =
        usize::try_from(start_i32).context("SubstringByAsciiChars start must be nonnegative")?;
    usize::try_from(length_i32).context("SubstringByAsciiChars length must be nonnegative")?;
    let units = input.encode_utf16().collect::<Vec<_>>();
    if start > units.len() {
        return Ok(IntrinsicValue::String(String::new()));
    }
    let end = start_i32
        .checked_add(length_i32)
        .ok_or_else(|| anyhow!("SubstringByAsciiChars start plus length overflowed Int32"))?;
    let end = usize::try_from(end)
        .expect("nonnegative Int32 inputs have a nonnegative checked sum")
        .min(units.len());
    let mut output = String::with_capacity(end - start);
    for unit in &units[start..end] {
        let is_valid = (32..=126).contains(unit)
            && !matches!(
                *unit as u8 as char,
                '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/'
            );
        output.push(if is_valid {
            char::from_u32(u32::from(*unit)).expect("ASCII is valid Unicode")
        } else {
            '_'
        });
    }
    Ok(IntrinsicValue::String(output))
}

fn require_string<'a>(
    receiver: Option<&'a IntrinsicValue>,
    descriptor: &IntrinsicDescriptor,
) -> Result<&'a String> {
    let Some(IntrinsicValue::String(receiver)) = receiver else {
        bail!("{} requires a System.String receiver", descriptor.member);
    };
    Ok(receiver)
}

fn argument_string<'a>(argument: &'a IntrinsicArgument, member: &str) -> Result<&'a str> {
    let IntrinsicValue::String(value) = &argument.value else {
        bail!(
            "{member} requires a String argument, found {}",
            argument.value.type_name()
        );
    };
    Ok(value)
}

fn argument_optional_string<'a>(
    argument: &'a IntrinsicArgument,
    member: &str,
) -> Result<Option<&'a str>> {
    match &argument.value {
        IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => Ok(None),
        IntrinsicValue::String(value) => Ok(Some(value)),
        value => bail!(
            "{member} requires a String or null, found {}",
            value.type_name()
        ),
    }
}

fn argument_i32(argument: &IntrinsicArgument, member: &str) -> Result<i32> {
    match &argument.value {
        IntrinsicValue::Byte(value) => Ok(i32::from(*value)),
        IntrinsicValue::Int16(value) => Ok(i32::from(*value)),
        IntrinsicValue::Int32(value) => Ok(*value),
        IntrinsicValue::Int64(value) => (*value)
            .try_into()
            .with_context(|| format!("{member} argument is outside the Int32 range")),
        _ => bail!(
            "{member} requires an Int32 argument, found {}",
            argument.value.type_name()
        ),
    }
}

fn argument_i64(argument: &IntrinsicArgument, member: &str) -> Result<i64> {
    match &argument.value {
        IntrinsicValue::Byte(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int16(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int32(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value),
        _ => bail!(
            "{member} requires an Int64 argument, found {}",
            argument.value.type_name()
        ),
    }
}

fn argument_usize(argument: &IntrinsicArgument, member: &str) -> Result<usize> {
    let value = argument_i64(argument, member)?;
    value
        .try_into()
        .with_context(|| format!("{member} argument '{value}' is not a nonnegative index"))
}

fn parse_boolean(value: &str, member: &str) -> Result<bool> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        bail!("{member} argument '{value}' is not Boolean")
    }
}

fn parse_decimal_i16(value: &str) -> Result<i16> {
    parse_signed_decimal(value, "Int16")?
        .try_into()
        .context("value is outside the Int16 range")
}

fn parse_decimal_i32(value: &str) -> Result<i32> {
    parse_signed_decimal(value, "Int32")?
        .try_into()
        .context("value is outside the Int32 range")
}

fn parse_decimal_i64(value: &str) -> Result<i64> {
    parse_signed_decimal(value, "Int64")
}

fn parse_arithmetic_i64(value: &str) -> Result<i64> {
    let value = value.trim();
    let integral = if let Some((integral, fractional)) = value.split_once('.') {
        if fractional.is_empty() || !fractional.bytes().all(|byte| byte == b'0') {
            bail!("'{value}' is not an integral MSBuild arithmetic value");
        }
        integral
    } else {
        value
    };
    parse_signed_decimal(integral, "Int64")
}

fn parse_signed_decimal(value: &str, type_name: &str) -> Result<i64> {
    let value = value.trim();
    let digits = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("'{value}' is not a valid invariant {type_name}");
    }
    value
        .parse()
        .with_context(|| format!("'{value}' is outside the {type_name} range"))
}

fn parse_decimal_u64(value: &str) -> Result<u64> {
    let value = value.trim();
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("'{value}' is not a valid invariant UInt64");
    }
    value
        .parse()
        .with_context(|| format!("'{value}' is outside the UInt64 range"))
}

fn coerce_signed_radix_value(value: &IntrinsicValue, member: &str) -> Result<IntrinsicValue> {
    let value = match value {
        IntrinsicValue::Byte(value) => i64::from(*value),
        IntrinsicValue::Int16(value) => i64::from(*value),
        IntrinsicValue::Int32(value) => i64::from(*value),
        IntrinsicValue::Int64(value) => *value,
        IntrinsicValue::String(value) => parse_decimal_i64(value)?,
        _ => bail!(
            "{member} radix overload requires a signed integer, found {}",
            value.type_name()
        ),
    };
    Ok(if let Ok(value) = i16::try_from(value) {
        IntrinsicValue::Int16(value)
    } else if let Ok(value) = i32::try_from(value) {
        IntrinsicValue::Int32(value)
    } else {
        IntrinsicValue::Int64(value)
    })
}

fn convert_to_i32(value: &IntrinsicValue) -> Result<i32> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_i32(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Byte(value) => Ok(i32::from(*value)),
        IntrinsicValue::Int16(value) => Ok(i32::from(*value)),
        IntrinsicValue::Int32(value) => Ok(*value),
        IntrinsicValue::Int64(value) => Ok((*value).try_into()?),
        IntrinsicValue::UInt64(value) => Ok((*value).try_into()?),
        IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => {
            bail!("Convert.ToInt32(null) is ambiguous")
        }
        _ => bail!("Convert.ToInt32 does not support {}", value.type_name()),
    }
}

fn convert_to_i64(value: &IntrinsicValue) -> Result<i64> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_i64(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Byte(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int16(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int32(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value),
        IntrinsicValue::UInt64(value) => Ok((*value).try_into()?),
        IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => {
            bail!("Convert.ToInt64(null) is ambiguous")
        }
        _ => bail!("Convert.ToInt64 does not support {}", value.type_name()),
    }
}

fn convert_to_u64(value: &IntrinsicValue) -> Result<u64> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_u64(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Byte(value) => Ok(u64::from(*value)),
        IntrinsicValue::Int16(value) => Ok((*value).try_into()?),
        IntrinsicValue::Int32(value) => Ok((*value).try_into()?),
        IntrinsicValue::Int64(value) => Ok((*value).try_into()?),
        IntrinsicValue::UInt64(value) => Ok(*value),
        IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => {
            bail!("Convert.ToUInt64(null) is ambiguous")
        }
        _ => bail!("Convert.ToUInt64 does not support {}", value.type_name()),
    }
}

fn convert_to_bool(value: &IntrinsicValue, member: &str) -> Result<bool> {
    match value {
        IntrinsicValue::String(value) => parse_boolean(value, member),
        IntrinsicValue::Boolean(value) => Ok(*value),
        IntrinsicValue::Byte(value) => Ok(*value != 0),
        IntrinsicValue::Int16(value) => Ok(*value != 0),
        IntrinsicValue::Int32(value) => Ok(*value != 0),
        IntrinsicValue::Int64(value) => Ok(*value != 0),
        IntrinsicValue::UInt64(value) => Ok(*value != 0),
        IntrinsicValue::Null | IntrinsicValue::TypedNull(_) => {
            bail!("Convert.ToBoolean(null) is ambiguous")
        }
        _ => bail!("Convert.ToBoolean does not support {}", value.type_name()),
    }
}

fn radix_digits(value: &str, radix: i32) -> Result<&str> {
    if !matches!(radix, 2 | 8 | 10 | 16) {
        bail!("MSB4184: radix must be 2, 8, 10, or 16");
    }
    let digits = value.strip_prefix('+').unwrap_or(value);
    if radix != 10 && digits.starts_with('-') {
        bail!("A minus sign is only valid with radix 10");
    }
    let digits = if radix == 16 {
        digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits)
    } else {
        digits
    };
    if digits.is_empty() {
        bail!("The converted value has no digits");
    }
    Ok(digits)
}

fn parse_radix_i32(value: &str, radix: i32) -> Result<i32> {
    let digits = radix_digits(value, radix)?;
    if radix == 10 {
        parse_decimal_i32(digits)
    } else {
        Ok(u32::from_str_radix(digits, radix as u32)? as i32)
    }
}

fn parse_radix_i64(value: &str, radix: i32) -> Result<i64> {
    let digits = radix_digits(value, radix)?;
    if radix == 10 {
        parse_decimal_i64(digits)
    } else {
        Ok(u64::from_str_radix(digits, radix as u32)? as i64)
    }
}

fn parse_radix_u64(value: &str, radix: i32) -> Result<u64> {
    let digits = radix_digits(value, radix)?;
    if radix == 10 {
        parse_decimal_u64(digits)
    } else {
        Ok(u64::from_str_radix(digits, radix as u32)?)
    }
}

fn format_radix(value: &IntrinsicValue, radix: i32) -> Result<String> {
    if !matches!(radix, 2 | 8 | 10 | 16) {
        bail!("Convert.ToString radix must be 2, 8, 10, or 16");
    }
    if radix == 10 {
        return match value {
            IntrinsicValue::Int16(value) => Ok(value.to_string()),
            IntrinsicValue::Int32(value) => Ok(value.to_string()),
            IntrinsicValue::Int64(value) => Ok(value.to_string()),
            _ => bail!("Convert.ToString(value, radix) requires a signed integer"),
        };
    }
    let value = match value {
        IntrinsicValue::Int16(value) => u64::from(*value as u16),
        IntrinsicValue::Int32(value) => u64::from(*value as u32),
        IntrinsicValue::Int64(value) => *value as u64,
        _ => bail!("Convert.ToString(value, radix) requires a signed integer"),
    };
    Ok(match radix {
        2 => format!("{value:b}"),
        8 => format!("{value:o}"),
        16 => format!("{value:x}"),
        _ => unreachable!("radix was validated"),
    })
}

fn numeric_compare(left: &IntrinsicValue, right: &IntrinsicValue) -> Result<Ordering> {
    match (left, right) {
        (IntrinsicValue::Byte(left), IntrinsicValue::Byte(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::Int16(left), IntrinsicValue::Int16(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::Int32(left), IntrinsicValue::Int32(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::Int64(left), IntrinsicValue::Int64(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::UInt64(left), IntrinsicValue::UInt64(right)) => Ok(left.cmp(right)),
        _ => bail!("Numeric CompareTo received mismatched coerced types"),
    }
}

fn numeric_equals(left: &IntrinsicValue, right: &IntrinsicValue) -> Result<bool> {
    Ok(match (left, right) {
        (IntrinsicValue::Byte(left), IntrinsicValue::Byte(right)) => left == right,
        (IntrinsicValue::Int16(left), IntrinsicValue::Int16(right)) => left == right,
        (IntrinsicValue::Int32(left), IntrinsicValue::Int32(right)) => left == right,
        (IntrinsicValue::Int64(left), IntrinsicValue::Int64(right)) => left == right,
        (IntrinsicValue::UInt64(left), IntrinsicValue::UInt64(right)) => left == right,
        _ => bail!("Numeric Equals received mismatched coerced types"),
    })
}

fn format_numeric(value: &IntrinsicValue, format: Option<&str>) -> Result<String> {
    let Some(format) = format.filter(|format| !format.is_empty()) else {
        return value.to_msbuild_string();
    };
    let mut characters = format.chars();
    let specifier = characters
        .next()
        .ok_or_else(|| anyhow!("Numeric format cannot be empty"))?;
    let precision_text = characters.as_str();
    if !precision_text.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("Unsupported numeric format '{format}'");
    }
    let precision = if precision_text.is_empty() {
        0
    } else {
        precision_text
            .parse::<usize>()
            .with_context(|| format!("Invalid numeric precision in '{format}'"))?
    };
    if precision > 999 {
        bail!("Numeric format precision cannot exceed 999");
    }

    if matches!(specifier, 'D' | 'd') {
        let (negative, magnitude) = match value {
            IntrinsicValue::Byte(value) => (false, u64::from(*value)),
            IntrinsicValue::Int16(value) => (*value < 0, u64::from(value.unsigned_abs())),
            IntrinsicValue::Int32(value) => (*value < 0, u64::from(value.unsigned_abs())),
            IntrinsicValue::Int64(value) => (*value < 0, value.unsigned_abs()),
            IntrinsicValue::UInt64(value) => (false, *value),
            _ => bail!("decimal formatting requires an integer receiver"),
        };
        let digits = format!("{magnitude:0precision$}");
        return Ok(if negative {
            format!("-{digits}")
        } else {
            digits
        });
    }

    if matches!(specifier, 'X' | 'x') {
        let integer = match value {
            IntrinsicValue::Byte(value) => u64::from(*value),
            IntrinsicValue::Int16(value) => u64::from(*value as u16),
            IntrinsicValue::Int32(value) => u64::from(*value as u32),
            IntrinsicValue::Int64(value) => *value as u64,
            IntrinsicValue::UInt64(value) => *value,
            _ => bail!("hexadecimal formatting requires an integer receiver"),
        };
        return Ok(if specifier == 'X' {
            format!("{integer:0precision$X}")
        } else {
            format!("{integer:0precision$x}")
        });
    }
    bail!("MSB4184: Unsupported numeric format '{format}'")
}

pub(crate) fn dotnet_ordinal_ignore_case_key(value: &str) -> Vec<u16> {
    let mut key = Vec::with_capacity(value.len());
    for character in value.chars() {
        let folded = if matches!(character, '\u{131}' | '\u{17f}') {
            character
        } else {
            let mut uppercase = character.to_uppercase();
            match (uppercase.next(), uppercase.next()) {
                (Some(single), None) => single,
                _ => character,
            }
        };
        let mut units = [0; 2];
        key.extend_from_slice(folded.encode_utf16(&mut units));
    }
    key
}

fn compare_ordinal(left: &str, right: &str) -> i32 {
    let mut left = left.encode_utf16();
    let mut right = right.encode_utf16();
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) if left != right => {
                return i32::from(left) - i32::from(right);
            }
            (Some(_), Some(_)) => {}
            (Some(_), None) => {
                return i32::try_from(left.count().saturating_add(1)).unwrap_or(i32::MAX);
            }
            (None, Some(_)) => {
                return -i32::try_from(right.count().saturating_add(1)).unwrap_or(i32::MAX);
            }
            (None, None) => return 0,
        }
    }
}

fn parse_guid(value: &str) -> Result<Uuid> {
    let value = value.trim();
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.starts_with("{0x") || compact.starts_with("{0X") {
        bail!(
            "System.Guid X parsing is outside the native surface; X formatting remains supported"
        );
    }
    let valid_shape = (value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || (value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            }))
        || ((value.starts_with('{') && value.ends_with('}'))
            || (value.starts_with('(') && value.ends_with(')')))
            && value.len() == 38
            && value[1..value.len() - 1]
                .bytes()
                .enumerate()
                .all(|(index, byte)| match index {
                    8 | 13 | 18 | 23 => byte == b'-',
                    _ => byte.is_ascii_hexdigit(),
                });
    if !valid_shape {
        bail!("Value is not in the retained Guid N, D, B, or P grammar");
    }
    let value = if (value.starts_with('{') && value.ends_with('}'))
        || (value.starts_with('(') && value.ends_with(')'))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Uuid::parse_str(value).map_err(Into::into)
}

fn utf16_substring(value: &str, start: usize, length: Option<usize>) -> Result<String> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    let end = length.map_or(units.len(), |length| start.saturating_add(length));
    let range = units
        .get(start..end)
        .ok_or_else(|| anyhow!("Substring range {start}..{end} is out of bounds"))?;
    String::from_utf16(range).map_err(|_| {
        anyhow!("Substring split a UTF-16 surrogate pair, which Rust cannot represent")
    })
}

fn utf16_byte_index(value: &str, target: usize) -> Result<usize> {
    if target == value.encode_utf16().count() {
        return Ok(value.len());
    }
    let mut units = 0;
    for (byte_index, character) in value.char_indices() {
        if units == target {
            return Ok(byte_index);
        }
        units += character.len_utf16();
        if units > target {
            bail!("Index {target} splits a UTF-16 surrogate pair");
        }
    }
    bail!("Index {target} is out of bounds")
}

fn parse_simple_version(value: &str) -> Result<[u32; 4]> {
    let mut value = value.trim();
    if value.starts_with(['v', 'V']) {
        value = &value[1..];
    }
    if let Some(index) = value.find(['-', '+']) {
        value = &value[..index];
    }
    let components = value.split('.').collect::<Vec<_>>();
    if components.is_empty() || components.len() > 4 {
        bail!("MSB4184: '{value}' is not a valid MSBuild version");
    }
    let mut version = [0; 4];
    for (index, component) in components.iter().enumerate() {
        if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("MSB4184: '{value}' is not a valid MSBuild version");
        }
        version[index] = component
            .parse::<u32>()
            .ok()
            .filter(|component| *component <= i32::MAX as u32)
            .ok_or_else(|| anyhow!("MSB4184: '{value}' is not a valid MSBuild version"))?;
    }
    Ok(version)
}

fn compare_sdk_versions(left: &str, right: &str) -> Result<Ordering> {
    Ok(parse_simple_version(left)?.cmp(&parse_simple_version(right)?))
}

struct ParsedTargetFramework {
    identifier: String,
    version: Vec<u32>,
    platform_identifier: Option<String>,
    platform_version: Option<Vec<u32>>,
}

fn parse_target_framework(value: &str) -> Result<ParsedTargetFramework> {
    let value = value.trim();
    let (framework, platform) = value
        .split_once('-')
        .map_or((value, None), |(framework, platform)| {
            (framework, Some(platform))
        });
    let lower = framework.to_ascii_lowercase();
    let (identifier, version) = if let Some(version) = lower.strip_prefix("netstandard") {
        (".NETStandard", parse_tfm_version(version)?)
    } else if let Some(version) = lower.strip_prefix("netcoreapp") {
        (".NETCoreApp", parse_tfm_version(version)?)
    } else if let Some(version) = lower.strip_prefix("net") {
        if version.contains('.') {
            let parsed = parse_tfm_version(version)?;
            if parsed.first().copied().unwrap_or_default() < 5 {
                bail!("Unsupported abbreviated target framework '{value}'");
            }
            (".NETCoreApp", parsed)
        } else if version.len() >= 2 && version.bytes().all(|byte| byte.is_ascii_digit()) {
            let mut digits = version.bytes().map(|byte| u32::from(byte - b'0'));
            let major = digits.next().unwrap_or_default();
            let mut parsed = vec![major, digits.next().unwrap_or_default()];
            parsed.extend(digits);
            (".NETFramework", parsed)
        } else {
            bail!("Unsupported target framework '{value}'");
        }
    } else {
        bail!("Unsupported target framework '{value}'");
    };

    let (platform_identifier, platform_version) = if let Some(platform) = platform {
        let identifier_end = platform
            .find(|character: char| character.is_ascii_digit())
            .unwrap_or(platform.len());
        let identifier = platform[..identifier_end].to_ascii_lowercase();
        let version = &platform[identifier_end..];
        let version = (!version.is_empty())
            .then(|| parse_tfm_version(version))
            .transpose()?;
        ((!identifier.is_empty()).then_some(identifier), version)
    } else {
        (None, None)
    };
    if platform.is_some() && platform_identifier.is_none() {
        bail!("Unsupported target platform in '{value}'");
    }
    Ok(ParsedTargetFramework {
        identifier: identifier.to_string(),
        version,
        platform_identifier,
        platform_version,
    })
}

fn parse_tfm_version(value: &str) -> Result<Vec<u32>> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.is_empty()
        || parts.len() > 4
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("'{value}' is not a supported target-framework version");
    }
    parts
        .iter()
        .map(|part| {
            part.parse::<u32>()
                .with_context(|| format!("'{value}' is not a supported target-framework version"))
        })
        .collect()
}

fn format_short_version(mut version: Vec<u32>, minimum_parts: usize) -> String {
    version.resize(version.len().max(minimum_parts), 0);
    while version.len() > minimum_parts && version.last() == Some(&0) {
        version.pop();
    }
    version
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn fix_file_path_for_style(style: PathStyle, value: &str) -> String {
    if style == PathStyle::Unix {
        value.replace('\\', "/")
    } else {
        value.to_string()
    }
}

fn path_component_equals(style: PathStyle, left: &str, right: &str) -> bool {
    if style != PathStyle::Windows {
        return left == right;
    }

    #[cfg(windows)]
    {
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        let (Ok(left_length), Ok(right_length)) =
            (i32::try_from(left.len()), i32::try_from(right.len()))
        else {
            return false;
        };
        // SAFETY: both pointers remain valid for their explicit UTF-16 lengths.
        (unsafe {
            CompareStringOrdinal(left.as_ptr(), left_length, right.as_ptr(), right_length, 1)
        }) == CSTR_EQUAL
    }

    #[cfg(not(windows))]
    {
        // Windows-style paths are not host paths here. Keep only the portable
        // ASCII rule rather than approximating Windows' Unicode ordinal table.
        left.eq_ignore_ascii_case(right)
    }
}

fn make_relative_with_current(
    style: PathStyle,
    base: &str,
    path: &str,
    current_directory: &str,
) -> Result<String> {
    let full_base = get_full_path(style, base, None, current_directory)?;
    let full_path = get_full_path(style, path, None, current_directory)?;
    let separator = path_directory_separator(style);
    let base_components = full_base
        .split(separator)
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let path_components = full_path
        .split(separator)
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let Some(first_component) = path_components.first() else {
        return Ok(full_path);
    };
    let first_non_separator = path
        .bytes()
        .position(|byte| !path_is_separator(style, byte))
        .unwrap_or(path.len());
    let authored_prefix = &path[first_non_separator..];
    if !authored_prefix
        .get(..first_component.len())
        .is_some_and(|prefix| path_component_equals(style, prefix, first_component))
    {
        return Ok(fix_file_path_for_style(style, path));
    }

    let mut common = 0;
    while common < base_components.len()
        && common < path_components.len()
        && path_component_equals(style, base_components[common], path_components[common])
    {
        common += 1;
    }
    if common == base_components.len() && common == path_components.len() {
        return Ok(".".to_string());
    }
    if common == 0 {
        return Ok(full_path);
    }

    let mut relative = Vec::new();
    relative.extend(std::iter::repeat_n(
        "..",
        base_components.len().saturating_sub(common),
    ));
    relative.extend(path_components[common..].iter().copied());
    let mut result = relative.join(&separator.to_string());
    if full_path.ends_with(separator) && !result.ends_with(separator) {
        result.push(separator);
    }
    Ok(result)
}

fn make_relative(base: &str, path: &str) -> Result<String> {
    let current_directory = std::env::current_dir()
        .context("Could not read the current directory for MSBuild.MakeRelative")?;
    make_relative_with_current(
        host_path_style(),
        base,
        path,
        &display_path(&current_directory),
    )
}

fn find_file_above(start: &str, file_name: &str) -> Result<Option<PathBuf>> {
    let mut directory = PathBuf::from(host_get_full_path(&fix_file_path(start), None)?);
    if directory.is_file() {
        directory.pop();
    }
    loop {
        let candidate = directory.join(file_name);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        if !directory.pop() {
            return Ok(None);
        }
    }
}

fn escape_lower(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        let escaped = match character {
            '%' => Some("25"),
            '*' => Some("2a"),
            '?' => Some("3f"),
            '@' => Some("40"),
            '$' => Some("24"),
            '(' => Some("28"),
            ')' => Some("29"),
            ';' => Some("3b"),
            '\'' => Some("27"),
            _ => None,
        };
        if let Some(hex) = escaped {
            output.push('%');
            output.push_str(hex);
        } else {
            output.push(character);
        }
    }
    output
}

fn dotnet_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn ordering_i32(ordering: Ordering) -> i32 {
    match ordering {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_unique_indexed_and_inspectable() {
        let registry = index();
        assert_eq!(registry.members.len(), INTRINSICS.len());
        let mut dispatch_codes = HashMap::new();
        let mut type_codes = HashMap::new();
        for entry in allowed_intrinsics() {
            let normalized = entry.member.to_ascii_lowercase();
            if let Some(existing) = dispatch_codes.insert(entry.dispatch_code, normalized.clone()) {
                assert_eq!(existing, normalized, "native dispatch-code collision");
            }
            let normalized_type = entry.type_name.to_ascii_lowercase();
            if let Some(existing) =
                type_codes.insert(member_code(entry.type_name), normalized_type.clone())
            {
                assert_eq!(existing, normalized_type, "native type-code collision");
            }
        }
        assert!(allowed_intrinsics().iter().any(|entry| {
            entry.type_name == "System.Math"
                && entry.member == "Abs"
                && entry.kind == InvocationKind::StaticMethod
        }));
        assert!(allowed_intrinsics().iter().all(|entry| {
            !entry.overloads.is_empty()
                && entry.overloads.iter().all(|overload| {
                    !overload.coercions.is_empty() || overload.arity == Arity::Exact(0)
                })
        }));

        let convert =
            resolve("system.convert", "toint64", InvocationKind::StaticMethod, 2).unwrap();
        let radix = convert
            .overloads
            .iter()
            .find(|overload| overload.arity == Arity::Exact(2))
            .unwrap();
        assert_eq!(radix.coercions, &[Coercion::String, Coercion::Radix]);
        let unescape = resolve("MSBuild", "UnEscape", InvocationKind::StaticMethod, 1).unwrap();
        assert_eq!(unescape.argument_rule, ArgumentRule::Escaped);
        assert_eq!(unescape.result_rule, ResultRule::AlreadyEscaped);
        assert!(!is_allowed(
            "System.String",
            "CompareTo",
            InvocationKind::InstanceMethod
        ));
        assert!(is_allowed(
            "System.Char",
            "ToString",
            InvocationKind::InstanceMethod
        ));
        assert!(!is_allowed(
            "MSBuild",
            "IsTargetFrameworkCompatible",
            InvocationKind::StaticMethod
        ));
        assert!(!is_allowed(
            "System.Environment",
            "Is64BitOperatingSystem",
            InvocationKind::StaticProperty
        ));
    }

    #[test]
    fn registry_intrinsic_missing_defaults_views_and_platform_order_match_msbuild() {
        let arguments = |key: IntrinsicValue,
                         name: IntrinsicValue,
                         default: Option<IntrinsicValue>,
                         views: Vec<IntrinsicValue>| {
            let mut arguments = vec![
                IntrinsicArgument { value: key },
                IntrinsicArgument { value: name },
            ];
            if let Some(default) = default {
                arguments.push(IntrinsicArgument { value: default });
            }
            arguments.extend(views.into_iter().map(|value| IntrinsicArgument { value }));
            arguments
        };
        let key = || IntrinsicValue::String(r"HKEY_CURRENT_USER\Software\Test".into());
        let name = || IntrinsicValue::String("Value".into());
        let fallback = || IntrinsicValue::String("FALLBACK".into());

        let get = arguments(key(), name(), Some(fallback()), vec![]);
        assert_eq!(
            registry_intrinsic_with_reader(&get, false, true, |_, _, _| {
                Ok(RegistryReadResult::KeyMissing)
            })
            .unwrap(),
            IntrinsicValue::Null
        );
        assert_eq!(
            registry_intrinsic_with_reader(&get, false, true, |_, _, _| {
                Ok(RegistryReadResult::ValueMissing)
            })
            .unwrap(),
            fallback()
        );
        assert_eq!(
            registry_intrinsic_with_reader(&get, false, true, |_, _, _| {
                Ok(RegistryReadResult::Value(RegistryData::DWord(42)))
            })
            .unwrap(),
            IntrinsicValue::Int32(42)
        );

        let from_view = arguments(
            key(),
            name(),
            Some(fallback()),
            vec![
                IntrinsicValue::String("Registry64".into()),
                IntrinsicValue::String("Registry32".into()),
            ],
        );
        assert_eq!(
            registry_intrinsic_with_reader(&from_view, true, true, |_, _, _| {
                Ok(RegistryReadResult::KeyMissing)
            })
            .unwrap(),
            fallback()
        );
        let no_views = arguments(IntrinsicValue::Null, name(), Some(fallback()), vec![]);
        assert_eq!(
            registry_intrinsic_with_reader(
                &no_views,
                true,
                true,
                |_, _, _| -> Result<RegistryReadResult> {
                    panic!("MSBuild's boxed synthesized default view is ignored")
                },
            )
            .unwrap(),
            fallback()
        );
        assert_eq!(
            registry_intrinsic_with_reader(&from_view, true, true, |_, _, view| {
                Ok(if view == RegistryView::Registry64 {
                    RegistryReadResult::ValueMissing
                } else {
                    RegistryReadResult::KeyMissing
                })
            })
            .unwrap(),
            IntrinsicValue::Null
        );
        assert_eq!(
            registry_intrinsic_with_reader(&from_view, true, true, |_, _, view| {
                Ok(if view == RegistryView::Registry64 {
                    RegistryReadResult::ValueMissing
                } else {
                    RegistryReadResult::Value(RegistryData::MultiString(vec![
                        "A".into(),
                        "B".into(),
                    ]))
                })
            })
            .unwrap(),
            IntrinsicValue::Strings(vec!["A".into(), "B".into()])
        );
        let value_before_invalid_view = arguments(
            key(),
            name(),
            Some(fallback()),
            vec![
                IntrinsicValue::String("Default".into()),
                IntrinsicValue::String("not-a-view".into()),
            ],
        );
        assert_eq!(
            registry_intrinsic_with_reader(&value_before_invalid_view, true, true, |_, _, _| Ok(
                RegistryReadResult::Value(RegistryData::DWord(42))
            ),)
            .unwrap(),
            IntrinsicValue::Int32(42)
        );

        let null_name = arguments(key(), IntrinsicValue::Null, Some(fallback()), vec![]);
        assert_eq!(
            registry_intrinsic_with_reader(&null_name, false, true, |_, name, _| {
                assert_eq!(name, None);
                Ok(RegistryReadResult::Value(RegistryData::String(
                    "DEFAULT".into(),
                )))
            })
            .unwrap(),
            IntrinsicValue::String("DEFAULT".into())
        );

        let invalid_non_windows = arguments(
            IntrinsicValue::Null,
            IntrinsicValue::Null,
            Some(IntrinsicValue::Int32(7)),
            vec![IntrinsicValue::String("not-a-view".into())],
        );
        assert_eq!(
            registry_intrinsic_with_reader(
                &invalid_non_windows,
                true,
                false,
                |_, _, _| -> Result<RegistryReadResult> {
                    panic!("non-Windows must return before registry validation")
                },
            )
            .unwrap(),
            IntrinsicValue::Int32(7)
        );

        let invalid_view_first = arguments(
            IntrinsicValue::Null,
            name(),
            Some(fallback()),
            vec![IntrinsicValue::String("not-a-view".into())],
        );
        let error = registry_intrinsic_with_reader(
            &invalid_view_first,
            true,
            true,
            |_, _, _| -> Result<RegistryReadResult> {
                panic!("invalid view must fail before key validation")
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("RegistryView"));

        let ignored_typed_view = arguments(
            IntrinsicValue::Null,
            name(),
            Some(fallback()),
            vec![IntrinsicValue::Null, IntrinsicValue::Int32(256)],
        );
        assert_eq!(
            registry_intrinsic_with_reader(
                &ignored_typed_view,
                true,
                true,
                |_, _, _| -> Result<RegistryReadResult> {
                    panic!("non-string object views are ignored")
                },
            )
            .unwrap(),
            fallback()
        );
    }

    #[test]
    fn disallowed_types_and_members_are_rejected_before_invocation() {
        let type_error = resolve(
            "System.Diagnostics.Process",
            "Start",
            InvocationKind::StaticMethod,
            1,
        )
        .unwrap_err()
        .to_string();
        assert!(type_error.contains("MSB4212"));
        assert!(type_error.contains("not available"));

        let member_error = resolve(
            "System.Environment",
            "SetEnvironmentVariable",
            InvocationKind::StaticMethod,
            2,
        )
        .unwrap_err()
        .to_string();
        assert!(member_error.contains("MSB4185"));
    }

    #[test]
    fn unsupported_nuget_tfm_compatibility_is_pruned_instead_of_approximated() {
        for (member, inputs, direct_result) in [
            (
                "IsTargetFrameworkCompatible",
                &["net48", "netstandard2.0"][..],
                "True",
            ),
            (
                "IsTargetFrameworkCompatible",
                &["netcoreapp1.0", "netstandard2.1"][..],
                "False",
            ),
        ] {
            let error = resolve(
                "MSBuild",
                member,
                InvocationKind::StaticMethod,
                inputs.len(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("MSB4185"),
                "{member}({}) must remain pruned rather than approximate direct MSBuild result {direct_result}: {error}",
                inputs.join(", ")
            );
        }
    }

    #[test]
    fn common_target_framework_helpers_match_sdk_inputs() -> Result<()> {
        assert_eq!(
            call(
                "MSBuild",
                "GetTargetFrameworkIdentifier",
                vec![IntrinsicValue::String("net10.0".into())],
            )?,
            IntrinsicValue::String(".NETCoreApp".into())
        );
        assert_eq!(
            call(
                "MSBuild",
                "GetTargetFrameworkVersion",
                vec![
                    IntrinsicValue::String("net10.0".into()),
                    IntrinsicValue::String("4".into()),
                ],
            )?,
            IntrinsicValue::String("10.0.0.0".into())
        );
        assert_eq!(
            call(
                "MSBuild",
                "GetTargetPlatformIdentifier",
                vec![IntrinsicValue::String("net10.0-windows10.0.19041.0".into())],
            )?,
            IntrinsicValue::String("windows".into())
        );
        assert_eq!(
            call(
                "MSBuild",
                "GetTargetPlatformVersion",
                vec![
                    IntrinsicValue::String("net10.0-windows10.0.19041.0".into()),
                    IntrinsicValue::String("2".into()),
                ],
            )?,
            IntrinsicValue::String("10.0.19041".into())
        );
        assert_eq!(
            call(
                "MSBuild",
                "GetTargetPlatformVersion",
                vec![IntrinsicValue::String("net10.0".into())],
            )?,
            IntrinsicValue::String("0.0".into())
        );
        Ok(())
    }

    #[test]
    fn overload_arity_is_deterministic() {
        let error = resolve("System.Math", "Abs", InvocationKind::StaticMethod, 2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no allowlisted overload accepting 2"));
    }

    fn call(
        type_name: &str,
        member: &str,
        arguments: Vec<IntrinsicValue>,
    ) -> Result<IntrinsicValue> {
        call_with_context(
            type_name,
            member,
            arguments,
            &IntrinsicContext {
                tools_directory: None,
                environment: None,
                disable_features_from_version: None,
                runtime_type: None,
            },
        )
    }

    fn call_with_context(
        type_name: &str,
        member: &str,
        arguments: Vec<IntrinsicValue>,
        context: &IntrinsicContext<'_>,
    ) -> Result<IntrinsicValue> {
        let descriptor = resolve(
            type_name,
            member,
            InvocationKind::StaticMethod,
            arguments.len(),
        )?;
        invoke(
            descriptor,
            context,
            None,
            &arguments
                .into_iter()
                .map(|value| IntrinsicArgument { value })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn typed_numeric_dispatch_matches_clr_edge_cases() -> Result<()> {
        let integral_cases = [
            ("Add", i64::MAX, 1, i64::MIN),
            ("Subtract", i64::MIN, 1, i64::MAX),
            ("Multiply", i64::MAX, 2, -2),
        ];
        for (member, left, right, expected) in integral_cases {
            assert_eq!(
                call(
                    "MSBuild",
                    member,
                    vec![
                        IntrinsicValue::String(left.to_string()),
                        IntrinsicValue::String(right.to_string()),
                    ],
                )?,
                IntrinsicValue::Int64(expected)
            );
        }

        assert_eq!(
            call(
                "System.Math",
                "Abs",
                vec![IntrinsicValue::String("-32769".to_string())],
            )?,
            IntrinsicValue::Int32(32769)
        );
        assert!(
            call(
                "System.Math",
                "Abs",
                vec![IntrinsicValue::String("-32768".to_string())],
            )
            .is_err()
        );
        assert_eq!(
            call("System.Math", "Abs", vec![IntrinsicValue::Int64(-32768)],)?,
            IntrinsicValue::Int64(32768)
        );
        assert_eq!(
            call(
                "System.Convert",
                "ToInt32",
                vec![
                    IntrinsicValue::String("FFFFFFFF".to_string()),
                    IntrinsicValue::String("16".to_string()),
                ],
            )?,
            IntrinsicValue::Int32(-1)
        );
        assert_eq!(
            call(
                "System.Convert",
                "ToString",
                vec![
                    IntrinsicValue::String("-1".to_string()),
                    IntrinsicValue::String("16".to_string()),
                ],
            )?,
            IntrinsicValue::String("ffff".to_string())
        );

        for member in ["Divide", "Modulo"] {
            assert!(
                call(
                    "MSBuild",
                    member,
                    vec![
                        IntrinsicValue::String(i64::MIN.to_string()),
                        IntrinsicValue::String("-1".to_string()),
                    ],
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn null_overload_policy_is_explicit() {
        let error = call("System.Convert", "ToString", vec![IntrinsicValue::Null])
            .unwrap_err()
            .to_string();
        assert!(error.contains("Ambiguous native overload"));
        let error = call("System.Convert", "ToInt32", vec![IntrinsicValue::Null])
            .unwrap_err()
            .to_string();
        assert!(error.contains("No native overload"));
        let copy = call("System.String", "Copy", vec![IntrinsicValue::Null])
            .unwrap_err()
            .to_string();
        assert!(copy.contains("does not accept null"));
    }

    #[test]
    fn culture_sensitive_floating_and_convert_members_are_pruned() -> Result<()> {
        for member in [
            "Max", "Min", "Ceiling", "Floor", "Truncate", "Round", "Pow", "Sqrt",
        ] {
            assert!(
                !is_allowed("System.Math", member, InvocationKind::StaticMethod),
                "{member}"
            );
        }
        assert!(!is_allowed(
            "System.Convert",
            "ToDouble",
            InvocationKind::StaticMethod
        ));
        assert!(!is_allowed(
            "System.Double",
            "ToString",
            InvocationKind::InstanceMethod
        ));
        for value in ["inf", "1.5", "1,5"] {
            assert!(
                call(
                    "MSBuild",
                    "Add",
                    vec![
                        IntrinsicValue::String(value.into()),
                        IntrinsicValue::String("1".into()),
                    ],
                )
                .is_err(),
                "{value}"
            );
        }
        assert!(call("System.Math", "Abs", vec![IntrinsicValue::UInt64(1)],).is_err());

        assert!(
            call(
                "System.Convert",
                "ToInt32",
                vec![IntrinsicValue::String("42".into())],
            )
            .is_err()
        );
        assert_eq!(
            call("System.Convert", "ToInt32", vec![IntrinsicValue::Int64(42)],)?,
            IntrinsicValue::Int32(42)
        );
        assert_eq!(
            call(
                "System.Convert",
                "ToString",
                vec![
                    IntrinsicValue::Int64(-1),
                    IntrinsicValue::String("16".into())
                ],
            )?,
            IntrinsicValue::String("ffff".into())
        );
        Ok(())
    }

    #[test]
    fn invariant_casing_is_pruned_without_a_dotnet_versioned_unicode_table() {
        for member in ["ToUpperInvariant", "ToLowerInvariant"] {
            assert!(!is_allowed(
                "System.String",
                member,
                InvocationKind::InstanceMethod
            ));
            assert!(resolve_item_string_function(member).is_none());
        }
    }

    #[test]
    fn path_lexical_semantics_match_windows_and_unix_corelib() -> Result<()> {
        assert!(path_is_rooted(PathStyle::Windows, r"C:relative"));
        assert!(!path_is_fully_qualified(PathStyle::Windows, r"C:relative"));
        assert!(!path_is_rooted(PathStyle::Unix, r"C:relative"));
        assert_eq!(
            path_get_extension(PathStyle::Windows, ".gitignore"),
            ".gitignore"
        );
        assert_eq!(
            path_get_extension(PathStyle::Unix, ".gitignore"),
            ".gitignore"
        );
        assert_eq!(path_get_file_name(PathStyle::Windows, "dir\\"), "");
        assert_eq!(path_get_file_name(PathStyle::Unix, "dir/"), "");
        assert_eq!(
            path_get_directory_name(PathStyle::Windows, r"C:\a\b").as_deref(),
            Some(r"C:\a")
        );
        assert_eq!(
            path_get_directory_name(PathStyle::Unix, "/a/b").as_deref(),
            Some("/a")
        );
        assert_eq!(
            path_change_extension(PathStyle::Windows, "file.txt", None),
            "file"
        );
        assert_eq!(
            path_change_extension(PathStyle::Unix, ".gitignore", Some("txt")),
            ".txt"
        );
        assert_eq!(
            path_combine(PathStyle::Windows, &["a", "C:relative"]),
            "C:relative"
        );
        assert_eq!(
            path_combine(PathStyle::Unix, &["a", r"C:relative"]),
            "a/C:relative"
        );

        assert!(get_full_path(PathStyle::Windows, "x", Some("relative"), r"C:\cwd").is_err());
        assert!(get_full_path(PathStyle::Unix, "x", Some("relative"), "/cwd").is_err());
        assert_eq!(
            get_full_path(
                PathStyle::Windows,
                r"C:relative",
                Some(r"C:\base"),
                r"C:\cwd",
            )?,
            r"C:\base\relative"
        );
        assert_eq!(
            get_full_path(PathStyle::Windows, r"C:relative", None, r"D:\cwd")?,
            r"C:\relative"
        );
        assert_eq!(
            get_full_path(
                PathStyle::Windows,
                r"C:\x\..\y",
                Some(r"C:\base"),
                r"C:\cwd",
            )?,
            r"C:\y"
        );
        assert_eq!(
            get_full_path(PathStyle::Unix, "../c", Some("/a/b"), "/cwd")?,
            "/a/c"
        );
        assert_eq!(
            make_relative_with_current(PathStyle::Windows, r"C:\a\b", r"C:\a\c", r"C:\cwd",)?,
            r"..\c"
        );
        #[cfg(windows)]
        {
            assert!(path_component_equals(PathStyle::Windows, "Ä", "ä"));
            assert!(!path_component_equals(PathStyle::Windows, "ƛ", "Ƛ"));
        }
        #[cfg(windows)]
        assert_eq!(
            make_relative_with_current(
                PathStyle::Windows,
                r"C:\Ärea\base",
                r"c:\ärea\child",
                r"C:\cwd",
            )?,
            r"..\child"
        );
        #[cfg(windows)]
        assert_eq!(
            make_relative_with_current(
                PathStyle::Windows,
                "C:\\ƛ\\base",
                "c:\\Ƛ\\child",
                r"C:\cwd",
            )?,
            "..\\..\\Ƛ\\child"
        );
        assert_eq!(
            make_relative_with_current(PathStyle::Windows, r"C:\a\b", r"C:relative", r"D:\cwd",)?,
            r"..\..\relative"
        );
        assert_eq!(
            make_relative_with_current(PathStyle::Unix, "/a/b", "/a/c", "/cwd")?,
            "../c"
        );
        assert_eq!(
            make_relative_with_current(PathStyle::Unix, r"C:\a\b", r"C:\a\c", "/cwd",)?,
            "C:/a/c"
        );
        Ok(())
    }

    #[test]
    fn file_above_resolves_relative_starts_to_lexical_absolute_paths() -> Result<()> {
        let current = std::env::current_dir()?;
        let directory = tempfile::tempdir_in(&current)?;
        let nested = directory.path().join("a").join("b");
        std::fs::create_dir_all(&nested)?;
        let marker = directory.path().join("marker.props");
        std::fs::write(&marker, "<Project />")?;
        let relative = nested.strip_prefix(&current)?.to_string_lossy();

        assert_eq!(find_file_above(&relative, "marker.props")?, Some(marker));
        Ok(())
    }

    #[test]
    fn stable_hash_versions_and_feature_boundaries_match_msbuild() -> Result<()> {
        assert_eq!(
            call(
                "MSBuild",
                "StableStringHash",
                vec![
                    IntrinsicValue::String("abc".to_string()),
                    IntrinsicValue::String("Fnv1a32bit".to_string()),
                ],
            )?,
            IntrinsicValue::Int32(-1_373_726_339)
        );
        assert_eq!(
            call(
                "MSBuild",
                "StableStringHash",
                vec![
                    IntrinsicValue::String("abc".to_string()),
                    IntrinsicValue::String("Fnv1a32bitFast".to_string()),
                ],
            )?,
            IntrinsicValue::Int32(440_920_331)
        );
        assert!(
            call(
                "MSBuild",
                "StableStringHash",
                vec![
                    IntrinsicValue::String("abc".to_string()),
                    IntrinsicValue::String("Fnv1a64bit".to_string()),
                ],
            )
            .is_err()
        );
        assert_eq!(
            compare_sdk_versions("v1.2.3-preview+data", "1.2.3")?,
            Ordering::Equal
        );
        assert!(compare_sdk_versions("garbage", "garbage").is_err());

        assert_eq!(resolve_feature_wave(Some("18.5.1")).version, "18.6");
        assert_eq!(resolve_feature_wave(Some("garbage")).version, "999.999");
        assert_eq!(resolve_feature_wave(Some("1.0")).version, "17.10");
        let disabled = "18.6";
        let context = IntrinsicContext {
            tools_directory: None,
            environment: None,
            disable_features_from_version: Some(disabled),
            runtime_type: None,
        };
        for (wave, expected) in [("18.5", true), ("18.6", false), ("18.7", false)] {
            assert_eq!(
                call_with_context(
                    "MSBuild",
                    "AreFeaturesEnabled",
                    vec![IntrinsicValue::String(wave.to_string())],
                    &context,
                )?,
                IntrinsicValue::Boolean(expected)
            );
        }
        assert!(
            call(
                "MSBuild",
                "VersionEquals",
                vec![
                    IntrinsicValue::String("garbage".to_string()),
                    IntrinsicValue::String("garbage".to_string()),
                ],
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn environment_expansion_is_single_pass_and_host_case_aware() {
        let environment = HashMap::from([
            (environment_key("MixedCase"), "VALUE".to_string()),
            (environment_key("Nested"), "%MixedCase%".to_string()),
        ]);
        assert_eq!(
            expand_environment_variables("%MixedCase%-%Missing%", &environment),
            "VALUE-%Missing%"
        );
        assert_eq!(
            expand_environment_variables("%Nested%", &environment),
            "%MixedCase%"
        );
        assert_eq!(
            expand_environment_variables("prefix%Missing", &environment),
            "prefix%Missing"
        );
        assert_eq!(expand_environment_variables("%%", &environment), "%%");
        assert_eq!(
            expand_environment_variables("%mixedcase%", &environment),
            if cfg!(windows) {
                "VALUE"
            } else {
                "%mixedcase%"
            }
        );

        let windows_environment = HashMap::from([(
            environment_key_for_platform("Σ", true),
            "UNICODE".to_string(),
        )]);
        assert_eq!(
            expand_environment_variables_for_platform("%ς%", &windows_environment, true,),
            "UNICODE"
        );
        assert_ne!(
            environment_key_for_platform("K", true),
            environment_key_for_platform("K", true)
        );
        assert_ne!(
            environment_key_for_platform("i", true),
            environment_key_for_platform("ı", true)
        );
    }

    #[test]
    fn task_host_detection_inspects_the_active_toolset() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = if cfg!(windows) {
            "MSBuild.exe"
        } else {
            "MSBuild"
        };
        std::fs::write(directory.path().join(executable), "")?;
        let tools = display_path(directory.path());
        let context = IntrinsicContext {
            tools_directory: Some(&tools),
            environment: None,
            disable_features_from_version: None,
            runtime_type: Some("Core"),
        };
        for architecture in ["x86", "x64", "arm64", "CurrentArchitecture"] {
            assert_eq!(
                call_with_context(
                    "MSBuild",
                    "DoesTaskHostExist",
                    vec![
                        IntrinsicValue::String("CurrentRuntime".to_string()),
                        IntrinsicValue::String(architecture.to_string()),
                    ],
                    &context,
                )?,
                IntrinsicValue::Boolean(true),
                "{architecture}"
            );
        }
        assert!(
            call_with_context(
                "MSBuild",
                "DoesTaskHostExist",
                vec![
                    IntrinsicValue::String("invalid".to_string()),
                    IntrinsicValue::String("CurrentArchitecture".to_string()),
                ],
                &context,
            )
            .is_err()
        );
        Ok(())
    }
}
