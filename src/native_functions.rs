use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use icu_casemap::CaseMapper;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::escaping::unescape_once;
use crate::properties::{display_path, lexical_absolute};
#[cfg(windows)]
use crate::registry::{RegistryView, read_registry_value};

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
    Boolean,
    Int16,
    Int32,
    Int64,
    UInt64,
    Double,
    Number,
    Version,
    Path,
    Radix,
    RegistryView,
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
    pub base_directory: &'a Path,
    pub tools_directory: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) struct IntrinsicArgument {
    pub value: IntrinsicValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IntrinsicValue {
    Null,
    String(String),
    Strings(Vec<String>),
    Char(u16),
    Boolean(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    UInt64(u64),
    Double(f64),
    Version(NativeVersion),
    Guid(Uuid),
    DateTime(NativeDateTime),
}

impl IntrinsicValue {
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "System.Object",
            Self::String(_) => "System.String",
            Self::Strings(_) => "System.String[]",
            Self::Char(_) => "System.Char",
            Self::Boolean(_) => "System.Boolean",
            Self::Int16(_) => "System.Int16",
            Self::Int32(_) => "System.Int32",
            Self::Int64(_) => "System.Int64",
            Self::UInt64(_) => "System.UInt64",
            Self::Double(_) => "System.Double",
            Self::Version(_) => "System.Version",
            Self::Guid(_) => "System.Guid",
            Self::DateTime(_) => "System.DateTime",
        }
    }

    pub(crate) fn to_msbuild_string(&self) -> Result<String> {
        match self {
            Self::Null => Ok(String::new()),
            Self::String(value) => Ok(value.clone()),
            Self::Strings(values) => Ok(values.join(";")),
            Self::Char(value) => String::from_utf16(&[*value])
                .map_err(|_| anyhow!("A lone UTF-16 surrogate cannot be rendered as UTF-8")),
            Self::Boolean(value) => Ok(dotnet_bool(*value).to_string()),
            Self::Int16(value) => Ok(value.to_string()),
            Self::Int32(value) => Ok(value.to_string()),
            Self::Int64(value) => Ok(value.to_string()),
            Self::UInt64(value) => Ok(value.to_string()),
            Self::Double(value) => format_double(*value),
            Self::Version(value) => Ok(value.to_string()),
            Self::Guid(value) => Ok(value.to_string()),
            Self::DateTime(_) => bail!(
                "Direct System.DateTime rendering is outside the native surface; use an allowlisted invariant ToString format"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
const O1_INT32: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Int32])];
const O1_INT64: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Int64])];
const O1_UINT64: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::UInt64])];
const O1_DOUBLE: &[OverloadDescriptor] = &[overload!(Arity::Exact(1), [Coercion::Double])];
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
const O2_REPLACE: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::String, Coercion::String],
    [NullPolicy::Reject, NullPolicy::EmptyString]
)];
const O2_NUMBER: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Number, Coercion::Number]
)];
const O2_INT32: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Int32, Coercion::Int32]
)];
const O2_DOUBLE: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Double, Coercion::Double]
)];
const O_INT32_STRING: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(2),
    [Coercion::Int32, Coercion::String]
)];
const O3_STRING_INT32: &[OverloadDescriptor] = &[overload!(
    Arity::Exact(3),
    [Coercion::String, Coercion::Int32, Coercion::Int32]
)];
const O_STRING_1_2: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::String]),
];
const O_STRING_OR_STRING_INT32: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::Int32]),
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
    overload!(Arity::Exact(1), [Coercion::Int32]),
    overload!(Arity::Exact(1), [Coercion::Int64]),
    overload!(Arity::Exact(1), [Coercion::Double]),
];
const O_ROUND: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::Double]),
    overload!(Arity::Exact(2), [Coercion::Double, Coercion::Int32]),
];
const O_CONVERT_INT32: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Boolean], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int32], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::UInt64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Double], [NullPolicy::Preserve]),
    overload!(Arity::Exact(2), [Coercion::String, Coercion::Radix]),
];
const O_CONVERT_INT64: &[OverloadDescriptor] = O_CONVERT_INT32;
const O_CONVERT_UINT64: &[OverloadDescriptor] = O_CONVERT_INT32;
const O_CONVERT_DOUBLE: &[OverloadDescriptor] = O_CONVERT_BOOLEAN;
const O_CONVERT_BOOLEAN: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Boolean], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int32], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::UInt64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Double], [NullPolicy::Preserve]),
];
const O_CONVERT_STRING: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(1), [Coercion::String], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Boolean], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int32], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Int64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::UInt64], [NullPolicy::Preserve]),
    overload!(Arity::Exact(1), [Coercion::Double], [NullPolicy::Preserve]),
    overload!(Arity::Exact(2), [Coercion::Int16, Coercion::Radix]),
    overload!(Arity::Exact(2), [Coercion::Int32, Coercion::Radix]),
    overload!(Arity::Exact(2), [Coercion::Int64, Coercion::Radix]),
];
const O_PATH_0_PLUS: &[OverloadDescriptor] = &[overload!(Arity::AtLeast(0), [Coercion::Path])];
const O_PATH_1_PLUS: &[OverloadDescriptor] = &[overload!(Arity::AtLeast(1), [Coercion::Path])];
const O_VERSION_NEW: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(0), []),
    overload!(Arity::Exact(1), [Coercion::String]),
    overload!(Arity::Range(2, 4), [Coercion::Int32]),
];
const O_JOIN: &[OverloadDescriptor] = &[overload!(
    Arity::AtLeast(2),
    [Coercion::String],
    [NullPolicy::EmptyString]
)];
const O_REGISTRY_VALUE: &[OverloadDescriptor] = &[
    overload!(Arity::Exact(2), [Coercion::String, Coercion::String]),
    overload!(
        Arity::Exact(3),
        [Coercion::String, Coercion::String, Coercion::Any],
        [NullPolicy::Reject, NullPolicy::Reject, NullPolicy::Preserve]
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
        NullPolicy::Reject,
        NullPolicy::Reject,
        NullPolicy::Preserve,
        NullPolicy::Reject,
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
        O1_STRING,
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
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Subtract",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Multiply",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Divide",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Modulo",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Int64/System.Double",
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
        O1_STRING,
        "System.String",
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
        O_STRING_OR_STRING_INT32,
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
        O_STRING_OR_STRING_INT32,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "IsTargetFrameworkCompatible",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.Boolean",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "DoesTaskHostExist",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
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
        "ToLowerInvariant",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "ToUpperInvariant",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Trim",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimStart",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimEnd",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
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
        O1_STRING_NULL,
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
        O2_STRING,
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
        "Is64BitOperatingSystem",
        StaticProperty,
        Decoded,
        Escape,
        O0,
        "System.Boolean",
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
        "System.Math",
        "Max",
        StaticMethod,
        Decoded,
        Escape,
        O2_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Min",
        StaticMethod,
        Decoded,
        Escape,
        O2_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Abs",
        StaticMethod,
        Decoded,
        Escape,
        O_MATH_ABS,
        "System.Int32/System.Int64/System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Ceiling",
        StaticMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Floor",
        StaticMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Truncate",
        StaticMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Round",
        StaticMethod,
        Decoded,
        Escape,
        O_ROUND,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Pow",
        StaticMethod,
        Decoded,
        Escape,
        O2_DOUBLE,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Sqrt",
        StaticMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Double",
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
        "ToDouble",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_DOUBLE,
        "System.Double",
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
        "System.Double",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Double",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_DOUBLE,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Double",
        "ToString",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
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
    (descriptor.handler)(descriptor, context, receiver, &arguments).with_context(|| {
        format!(
            "MSB4184: The expression invoking [{}]::{} could not be evaluated",
            descriptor.type_name, descriptor.member
        )
    })
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
    if matches!(value, IntrinsicValue::Null) {
        return match null_policy {
            NullPolicy::Reject => bail!("{member} does not accept null for this overload"),
            NullPolicy::Preserve => Ok((IntrinsicValue::Null, 0)),
            NullPolicy::EmptyString => Ok((IntrinsicValue::String(String::new()), 0)),
        };
    }

    let result = match coercion {
        Coercion::Any => (value.clone(), 0),
        Coercion::String | Coercion::Path | Coercion::RegistryView => match value {
            IntrinsicValue::String(value) => (IntrinsicValue::String(value.clone()), 0),
            IntrinsicValue::Char(value) => (
                IntrinsicValue::String(String::from_utf16(&[*value]).map_err(|_| {
                    anyhow!("{member} cannot convert a lone UTF-16 surrogate to String")
                })?),
                1,
            ),
            _ => (IntrinsicValue::String(value.to_msbuild_string()?), 30),
        },
        Coercion::Boolean => match value {
            IntrinsicValue::Boolean(value) => (IntrinsicValue::Boolean(*value), 0),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::Boolean(parse_boolean(value, member)?), 20)
            }
            IntrinsicValue::Int16(value) => (IntrinsicValue::Boolean(*value != 0), 10),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Boolean(*value != 0), 10),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Boolean(*value != 0), 10),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Boolean(*value != 0), 10),
            IntrinsicValue::Double(value) => (IntrinsicValue::Boolean(*value != 0.0), 10),
            _ => bail!("{member} cannot coerce {} to Boolean", value.type_name()),
        },
        Coercion::Int16 => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int16(*value), 0),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int16((*value).try_into()?), 5),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int16((*value).try_into()?), 6),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Int16((*value).try_into()?), 6),
            IntrinsicValue::Double(value) => {
                (IntrinsicValue::Int16(checked_f64_to_i16(*value)?), 8)
            }
            IntrinsicValue::String(value) => (IntrinsicValue::Int16(parse_decimal_i16(value)?), 20),
            _ => bail!("{member} cannot coerce {} to Int16", value.type_name()),
        },
        Coercion::Int32 | Coercion::Radix => {
            let (value, score) = match value {
                IntrinsicValue::Int16(value) => (i32::from(*value), 1),
                IntrinsicValue::Int32(value) => (*value, 0),
                IntrinsicValue::Int64(value) => ((*value).try_into()?, 5),
                IntrinsicValue::UInt64(value) => ((*value).try_into()?, 6),
                IntrinsicValue::Double(value) => (checked_f64_to_i32(*value)?, 8),
                IntrinsicValue::String(value) => (parse_decimal_i32(value)?, 21),
                _ => bail!("{member} cannot coerce {} to Int32", value.type_name()),
            };
            if coercion == Coercion::Radix && !matches!(value, 2 | 8 | 10 | 16) {
                bail!("{member} radix must be 2, 8, 10, or 16");
            }
            (IntrinsicValue::Int32(value), score)
        }
        Coercion::Int64 => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int64(i64::from(*value)), 1),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int64(i64::from(*value)), 1),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int64(*value), 0),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Int64((*value).try_into()?), 6),
            IntrinsicValue::Double(value) => {
                (IntrinsicValue::Int64(checked_f64_to_i64(*value)?), 8)
            }
            IntrinsicValue::String(value) => (IntrinsicValue::Int64(parse_decimal_i64(value)?), 22),
            _ => bail!("{member} cannot coerce {} to Int64", value.type_name()),
        },
        Coercion::UInt64 => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::Int32(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::Int64(value) => (IntrinsicValue::UInt64((*value).try_into()?), 5),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::UInt64(*value), 0),
            IntrinsicValue::Double(value) => {
                (IntrinsicValue::UInt64(checked_f64_to_u64(*value)?), 8)
            }
            IntrinsicValue::String(value) => {
                (IntrinsicValue::UInt64(parse_decimal_u64(value)?), 23)
            }
            _ => bail!("{member} cannot coerce {} to UInt64", value.type_name()),
        },
        Coercion::Double => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::Double(f64::from(*value)), 2),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Double(f64::from(*value)), 2),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Double(*value as f64), 2),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Double(*value as f64), 2),
            IntrinsicValue::Double(value) => (IntrinsicValue::Double(*value), 0),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::Double(parse_invariant_f64(value)?), 24)
            }
            _ => bail!("{member} cannot coerce {} to Double", value.type_name()),
        },
        Coercion::Number => match value {
            IntrinsicValue::Int16(value) => (IntrinsicValue::Int64(i64::from(*value)), 0),
            IntrinsicValue::Int32(value) => (IntrinsicValue::Int64(i64::from(*value)), 0),
            IntrinsicValue::Int64(value) => (IntrinsicValue::Int64(*value), 0),
            IntrinsicValue::UInt64(value) => (IntrinsicValue::Int64((*value).try_into()?), 1),
            IntrinsicValue::Double(value) => (IntrinsicValue::Double(*value), 0),
            IntrinsicValue::String(value) => {
                if let Ok(value) = parse_decimal_i64(value) {
                    (IntrinsicValue::Int64(value), 20)
                } else {
                    (IntrinsicValue::Double(parse_invariant_f64(value)?), 20)
                }
            }
            _ => bail!("{member} cannot coerce {} to Number", value.type_name()),
        },
        Coercion::Version => match value {
            IntrinsicValue::Version(value) => (IntrinsicValue::Version(value.clone()), 0),
            IntrinsicValue::String(value) => {
                (IntrinsicValue::Version(NativeVersion::parse(value)?), 20)
            }
            _ => bail!("{member} cannot coerce {} to Version", value.type_name()),
        },
    };
    Ok(result)
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
            IntrinsicValue::Null => true,
            IntrinsicValue::String(value) => value.is_empty(),
            _ => false,
        }))
    } else if operation == member_code("IsNullOrWhiteSpace") {
        Ok(IntrinsicValue::Boolean(match &arguments[0].value {
            IntrinsicValue::Null => true,
            IntrinsicValue::String(value) => value.chars().all(char::is_whitespace),
            _ => false,
        }))
    } else if operation == member_code("Join") {
        let separator = argument_string(&arguments[0], descriptor.member)?;
        Ok(IntrinsicValue::String(
            arguments[1..]
                .iter()
                .map(|argument| argument_string(argument, descriptor.member))
                .collect::<Result<Vec<_>>>()?
                .join(separator),
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
    } else if operation == member_code("Substring") {
        let start = argument_usize(&arguments[0], member)?;
        let length = arguments
            .get(1)
            .map(|argument| argument_usize(argument, member))
            .transpose()?;
        Ok(IntrinsicValue::String(utf16_substring(
            receiver, start, length,
        )?))
    } else if operation == member_code("ToLowerInvariant") {
        Ok(IntrinsicValue::String(invariant_case(receiver, false)))
    } else if operation == member_code("ToUpperInvariant") {
        Ok(IntrinsicValue::String(invariant_case(receiver, true)))
    } else if matches!(
        operation,
        value
            if value == member_code("Trim")
                || value == member_code("TrimStart")
                || value == member_code("TrimEnd")
    ) {
        let characters = arguments
            .first()
            .map(|argument| {
                argument_optional_string(argument, member)
                    .map(|value| value.map(ToString::to_string))
            })
            .transpose()?;
        let characters = characters.flatten();
        let matches = |character| {
            characters
                .as_ref()
                .is_some_and(|value| value.contains(character))
        };
        let value = if operation == member_code("TrimStart") {
            characters.as_ref().map_or_else(
                || receiver.trim_start_matches(char::is_whitespace),
                |_| receiver.trim_start_matches(matches),
            )
        } else if operation == member_code("TrimEnd") {
            characters.as_ref().map_or_else(
                || receiver.trim_end_matches(char::is_whitespace),
                |_| receiver.trim_end_matches(matches),
            )
        } else {
            characters.as_ref().map_or_else(
                || receiver.trim_matches(char::is_whitespace),
                |_| receiver.trim_matches(matches),
            )
        };
        Ok(IntrinsicValue::String(value.to_string()))
    } else if operation == member_code("Replace") {
        let old = argument_string(&arguments[0], member)?;
        if old.is_empty() {
            bail!("String.Replace oldValue cannot be empty");
        }
        Ok(IntrinsicValue::String(
            receiver.replace(old, argument_string(&arguments[1], member)?),
        ))
    } else if operation == member_code("Split") {
        let separators = match &arguments[0].value {
            IntrinsicValue::Null => None,
            IntrinsicValue::String(value) if value.is_empty() => None,
            IntrinsicValue::String(value) => Some(value.as_str()),
            _ => bail!("String.Split requires a string separator"),
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
            IntrinsicValue::Null => false,
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
    context: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("Combine") {
        let Some(first) = arguments.first() else {
            return Ok(IntrinsicValue::String(String::new()));
        };
        let mut path = PathBuf::from(argument_string(first, descriptor.member)?);
        for argument in arguments.iter().skip(1) {
            path.push(argument_string(argument, descriptor.member)?);
        }
        Ok(IntrinsicValue::String(display_path(&path)))
    } else if operation == member_code("IsPathRooted") {
        Ok(IntrinsicValue::Boolean(
            Path::new(argument_string(&arguments[0], descriptor.member)?).has_root(),
        ))
    } else if operation == member_code("GetDirectoryName") {
        Ok(IntrinsicValue::String(
            Path::new(argument_string(&arguments[0], descriptor.member)?)
                .parent()
                .map(display_path)
                .unwrap_or_default(),
        ))
    } else if operation == member_code("GetFileName") {
        Ok(IntrinsicValue::String(
            Path::new(argument_string(&arguments[0], descriptor.member)?)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ))
    } else if operation == member_code("GetFileNameWithoutExtension") {
        Ok(IntrinsicValue::String(
            Path::new(argument_string(&arguments[0], descriptor.member)?)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ))
    } else if operation == member_code("GetExtension") {
        Ok(IntrinsicValue::String(
            Path::new(argument_string(&arguments[0], descriptor.member)?)
                .extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("GetFullPath") {
        let path = if arguments.len() == 2 {
            Path::new(argument_string(&arguments[1], descriptor.member)?)
                .join(argument_string(&arguments[0], descriptor.member)?)
        } else {
            PathBuf::from(argument_string(&arguments[0], descriptor.member)?)
        };
        Ok(IntrinsicValue::String(display_path(
            &lexical_absolute(&path).unwrap_or_else(|_| context.base_directory.join(path)),
        )))
    } else if operation == member_code("GetPathRoot") {
        let root = Path::new(argument_string(&arguments[0], descriptor.member)?)
            .components()
            .take_while(|component| {
                matches!(
                    component,
                    std::path::Component::Prefix(_) | std::path::Component::RootDir
                )
            })
            .collect::<PathBuf>();
        Ok(IntrinsicValue::String(display_path(&root)))
    } else if operation == member_code("HasExtension") {
        Ok(IntrinsicValue::Boolean(
            Path::new(argument_string(&arguments[0], descriptor.member)?)
                .extension()
                .is_some(),
        ))
    } else if operation == member_code("ChangeExtension") {
        let mut path = PathBuf::from(argument_string(&arguments[0], descriptor.member)?);
        let extension = argument_string(&arguments[1], descriptor.member)?.trim_start_matches('.');
        path.set_extension(extension);
        Ok(IntrinsicValue::String(display_path(&path)))
    } else if operation == member_code("GetTempPath") {
        Ok(IntrinsicValue::String(with_trailing_separator(
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

fn handle_math(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let member = descriptor.member;
    let operation = descriptor.dispatch_code;
    if operation == member_code("Abs") {
        return match &arguments[0].value {
            IntrinsicValue::Int32(value) => value
                .checked_abs()
                .map(IntrinsicValue::Int32)
                .ok_or_else(|| anyhow!("System.Math.Abs overflowed its Int32 argument")),
            IntrinsicValue::Int64(value) => value
                .checked_abs()
                .map(IntrinsicValue::Int64)
                .ok_or_else(|| anyhow!("System.Math.Abs overflowed its Int64 argument")),
            IntrinsicValue::Double(value) => Ok(IntrinsicValue::Double(value.abs())),
            _ => bail!("System.Math.Abs received a nonnumeric coerced argument"),
        };
    }
    let left = argument_f64(&arguments[0], member)?;
    let value = if operation == member_code("Max") {
        let right = argument_f64(&arguments[1], member)?;
        if left.is_nan() {
            left
        } else if right.is_nan() {
            right
        } else {
            left.max(right)
        }
    } else if operation == member_code("Min") {
        let right = argument_f64(&arguments[1], member)?;
        if left.is_nan() {
            left
        } else if right.is_nan() {
            right
        } else {
            left.min(right)
        }
    } else if operation == member_code("Ceiling") {
        left.ceil()
    } else if operation == member_code("Floor") {
        left.floor()
    } else if operation == member_code("Truncate") {
        left.trunc()
    } else if operation == member_code("Round") {
        if let Some(digits) = arguments.get(1) {
            let digits = argument_i32(digits, member)?;
            if !(0..=15).contains(&digits) {
                bail!("System.Math.Round digits must be between 0 and 15");
            }
            if left.abs() < 1e16 {
                let factor = 10f64.powi(digits);
                (left * factor).round_ties_even() / factor
            } else {
                left
            }
        } else {
            left.round_ties_even()
        }
    } else if operation == member_code("Pow") {
        left.powf(argument_f64(&arguments[1], member)?)
    } else if operation == member_code("Sqrt") {
        left.sqrt()
    } else {
        unreachable!("all registered math members are handled")
    };
    Ok(IntrinsicValue::Double(value))
}

fn handle_environment(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("ExpandEnvironmentVariables") {
        let mut output = argument_string(&arguments[0], descriptor.member)?.to_string();
        for (name, value) in std::env::vars() {
            output = output.replace(&format!("%{name}%"), &value);
        }
        Ok(IntrinsicValue::String(output))
    } else if operation == member_code("GetEnvironmentVariable") {
        Ok(IntrinsicValue::String(
            std::env::var(argument_string(&arguments[0], descriptor.member)?).unwrap_or_default(),
        ))
    } else if operation == member_code("NewLine") {
        Ok(IntrinsicValue::String(
            if cfg!(windows) { "\r\n" } else { "\n" }.to_string(),
        ))
    } else if operation == member_code("Is64BitOperatingSystem")
        || operation == member_code("Is64BitProcess")
    {
        Ok(IntrinsicValue::Boolean(usize::BITS == 64))
    } else {
        Ok(IntrinsicValue::Int32(
            std::thread::available_parallelism()
                .map(|count| i32::try_from(count.get()).unwrap_or(i32::MAX))
                .unwrap_or(1),
        ))
    }
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
    } else if operation == member_code("ToDouble") {
        Ok(IntrinsicValue::Double(convert_to_f64(&arguments[0].value)?))
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
                IntrinsicValue::Null => String::new(),
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
            IntrinsicValue::Null => {
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
        None | Some(IntrinsicValue::Null) => "D",
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
        Ok(IntrinsicValue::Boolean(true))
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
        );
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
    } else if operation == member_code("GetDirectoryNameOfFileAbove") {
        Ok(IntrinsicValue::String(
            find_file_above(
                argument_string(&arguments[0], member)?,
                argument_string(&arguments[1], member)?,
            )
            .and_then(|path| path.parent().map(display_path))
            .unwrap_or_default(),
        ))
    } else if operation == member_code("GetPathOfFileAbove") {
        let file_name = Path::new(argument_string(&arguments[0], member)?)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        Ok(IntrinsicValue::String(
            find_file_above(argument_string(&arguments[1], member)?, &file_name)
                .map(|path| display_path(&path))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("MakeRelative") {
        Ok(IntrinsicValue::String(make_relative(
            argument_string(&arguments[0], member)?,
            argument_string(&arguments[1], member)?,
        )))
    } else if operation == member_code("NormalizePath")
        || operation == member_code("NormalizeDirectory")
    {
        let mut path = PathBuf::from(argument_string(&arguments[0], member)?);
        for argument in arguments.iter().skip(1) {
            path.push(argument_string(argument, member)?);
        }
        let path = lexical_absolute(&path).unwrap_or_else(|_| context.base_directory.join(path));
        let value = display_path(&path);
        Ok(IntrinsicValue::String(
            if operation == member_code("NormalizeDirectory") {
                with_trailing_separator(value)
            } else {
                value
            },
        ))
    } else if operation == member_code("EnsureTrailingSlash") {
        let value = argument_string(&arguments[0], member)?;
        Ok(IntrinsicValue::String(if value.is_empty() {
            String::new()
        } else {
            with_trailing_separator(value.to_string())
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
    } else if operation == member_code("GetTargetFrameworkIdentifier") {
        Ok(IntrinsicValue::String(
            target_framework(argument_string(&arguments[0], member)?).0,
        ))
    } else if operation == member_code("GetTargetFrameworkVersion") {
        let (_, version) = target_framework(argument_string(&arguments[0], member)?);
        Ok(IntrinsicValue::String(format_version_parts(
            &version,
            optional_part_count(arguments)?,
        )))
    } else if operation == member_code("GetTargetPlatformIdentifier") {
        Ok(IntrinsicValue::String(
            target_platform(argument_string(&arguments[0], member)?).0,
        ))
    } else if operation == member_code("GetTargetPlatformVersion") {
        let (_, version) = target_platform(argument_string(&arguments[0], member)?);
        Ok(IntrinsicValue::String(format_version_parts(
            &version,
            optional_part_count(arguments)?,
        )))
    } else if operation == member_code("IsTargetFrameworkCompatible") {
        Ok(IntrinsicValue::Boolean(target_framework_compatible(
            argument_string(&arguments[0], member)?,
            argument_string(&arguments[1], member)?,
        )))
    } else if operation == member_code("DoesTaskHostExist") {
        Ok(IntrinsicValue::Boolean(false))
    } else if operation == member_code("GetToolsDirectory32") {
        Ok(IntrinsicValue::String(
            context.tools_directory.unwrap_or_default().to_string(),
        ))
    } else if operation == member_code("SubstringByAsciiChars") {
        let start = argument_usize(&arguments[1], member)?;
        let length = argument_usize(&arguments[2], member)?;
        Ok(IntrinsicValue::String(
            argument_string(&arguments[0], member)?
                .get(start..start.saturating_add(length))
                .ok_or_else(|| anyhow!("ASCII substring range is out of bounds"))?
                .to_string(),
        ))
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
    if arguments.iter().all(is_integer_argument) {
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
        return Ok(IntrinsicValue::Int64(value));
    }
    let left = argument_f64(&arguments[0], member)?;
    let right = argument_f64(&arguments[1], member)?;
    let value = if operation == member_code("Add") {
        left + right
    } else if operation == member_code("Subtract") {
        left - right
    } else if operation == member_code("Multiply") {
        left * right
    } else if operation == member_code("Divide") {
        left / right
    } else {
        left % right
    };
    Ok(IntrinsicValue::Double(value))
}

fn registry_intrinsic(
    arguments: &[IntrinsicArgument],
    views_supplied: bool,
) -> Result<IntrinsicValue> {
    let default = match arguments.get(2).map(|argument| &argument.value) {
        None | Some(IntrinsicValue::Null) => None,
        Some(value) => Some(value.to_msbuild_string()?),
    };

    #[cfg(not(windows))]
    {
        let _ = views_supplied;
        return Ok(IntrinsicValue::String(default.unwrap_or_default()));
    }

    #[cfg(windows)]
    {
        let views = if views_supplied {
            arguments[3..]
                .iter()
                .map(|argument| {
                    RegistryView::parse(argument_string(argument, "GetRegistryValueFromView")?)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![RegistryView::Default]
        };
        Ok(IntrinsicValue::String(
            read_registry_value(
                argument_string(&arguments[0], "GetRegistryValue")?,
                (!argument_string(&arguments[1], "GetRegistryValue")?.is_empty())
                    .then_some(argument_string(&arguments[1], "GetRegistryValue")?),
                &views,
            )?
            .or(default)
            .unwrap_or_default(),
        ))
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
    } else if algorithm.eq_ignore_ascii_case("Fnv1a32bit")
        || algorithm.eq_ignore_ascii_case("Fnv1a32bitFast")
    {
        let mut hash = 2_166_136_261u32;
        for byte in value.as_bytes() {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        Ok(IntrinsicValue::UInt64(u64::from(hash)))
    } else {
        bail!("MSB4184: Unsupported StableStringHash algorithm '{algorithm}'")
    }
}

fn target_framework(value: &str) -> (String, String) {
    let framework = value
        .split('-')
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    if let Some(version) = framework.strip_prefix("netstandard") {
        (".NETStandard".to_string(), normalize_short_version(version))
    } else if let Some(version) = framework.strip_prefix("netcoreapp") {
        (".NETCoreApp".to_string(), normalize_short_version(version))
    } else if let Some(version) = framework.strip_prefix("net") {
        let normalized = normalize_short_version(version);
        let major = normalized
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .unwrap_or_default();
        (
            if major >= 5 {
                ".NETCoreApp"
            } else {
                ".NETFramework"
            }
            .to_string(),
            normalized,
        )
    } else {
        ("Unsupported".to_string(), String::new())
    }
}

fn target_platform(value: &str) -> (String, String) {
    let Some(platform) = value.split_once('-').map(|(_, platform)| platform) else {
        return (String::new(), String::new());
    };
    let name_end = platform
        .find(|character: char| character.is_ascii_digit())
        .unwrap_or(platform.len());
    let name = &platform[..name_end];
    (
        name.to_ascii_lowercase(),
        normalize_short_version(&platform[name_end..]),
    )
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

fn target_framework_compatible(target: &str, candidate: &str) -> bool {
    let (target_id, target_version) = target_framework(target);
    let (candidate_id, candidate_version) = target_framework(candidate);
    if target_id == "Unsupported" || candidate_id == "Unsupported" {
        return false;
    }
    if target_id == candidate_id {
        return compare_sdk_versions(&target_version, &candidate_version).is_ge();
    }
    target_id == ".NETCoreApp" && candidate_id == ".NETStandard"
}

fn normalize_short_version(value: &str) -> String {
    if value.contains('.') || !value.is_ascii() {
        return value.to_string();
    }
    match value.len() {
        2 => format!("{}.{}", &value[..1], &value[1..]),
        3 => format!("{}.{}.{}", &value[..1], &value[1..2], &value[2..]),
        4 => format!(
            "{}.{}.{}.{}",
            &value[..1],
            &value[1..2],
            &value[2..3],
            &value[3..]
        ),
        _ => value.to_string(),
    }
}

fn optional_part_count(arguments: &[IntrinsicArgument]) -> Result<Option<usize>> {
    let count = arguments
        .get(1)
        .map(|argument| argument_usize(argument, "version part count"))
        .transpose()?;
    if count.is_some_and(|count| count > 4) {
        bail!("version part count cannot exceed 4");
    }
    Ok(count)
}

fn format_version_parts(version: &str, count: Option<usize>) -> String {
    let Some(count) = count else {
        return version.to_string();
    };
    let mut parts = version
        .split('.')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    parts.resize(count, "0".to_string());
    parts.truncate(count);
    parts.join(".")
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
        IntrinsicValue::Null => Ok(None),
        IntrinsicValue::String(value) => Ok(Some(value)),
        value => bail!(
            "{member} requires a String or null, found {}",
            value.type_name()
        ),
    }
}

fn is_integer_argument(argument: &IntrinsicArgument) -> bool {
    matches!(
        &argument.value,
        IntrinsicValue::Int16(_) | IntrinsicValue::Int32(_) | IntrinsicValue::Int64(_)
    )
}

fn argument_i32(argument: &IntrinsicArgument, member: &str) -> Result<i32> {
    match &argument.value {
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

fn argument_f64(argument: &IntrinsicArgument, member: &str) -> Result<f64> {
    match &argument.value {
        IntrinsicValue::Int16(value) => Ok(f64::from(*value)),
        IntrinsicValue::Int32(value) => Ok(f64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value as f64),
        IntrinsicValue::UInt64(value) => Ok(*value as f64),
        IntrinsicValue::Double(value) => Ok(*value),
        _ => bail!(
            "{member} requires a numeric argument, found {}",
            argument.value.type_name()
        ),
    }
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

fn parse_invariant_f64(value: &str) -> Result<f64> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("nan") {
        return Ok(f64::NAN);
    }
    if value.eq_ignore_ascii_case("infinity") || value.eq_ignore_ascii_case("+infinity") {
        return Ok(f64::INFINITY);
    }
    if value.eq_ignore_ascii_case("-infinity") {
        return Ok(f64::NEG_INFINITY);
    }
    value
        .parse()
        .with_context(|| format!("'{value}' is not an invariant Double"))
}

fn checked_f64_to_i16(value: f64) -> Result<i16> {
    let value = checked_rounded_f64(value, f64::from(i16::MIN), f64::from(i16::MAX) + 1.0)?;
    Ok(value as i16)
}

fn checked_f64_to_i32(value: f64) -> Result<i32> {
    let value = checked_rounded_f64(value, f64::from(i32::MIN), f64::from(i32::MAX) + 1.0)?;
    Ok(value as i32)
}

fn checked_f64_to_i64(value: f64) -> Result<i64> {
    let value = checked_rounded_f64(value, -(2f64.powi(63)), 2f64.powi(63))?;
    Ok(value as i64)
}

fn checked_f64_to_u64(value: f64) -> Result<u64> {
    let value = checked_rounded_f64(value, 0.0, 2f64.powi(64))?;
    Ok(value as u64)
}

fn checked_rounded_f64(value: f64, minimum: f64, maximum_exclusive: f64) -> Result<f64> {
    let value = value.round_ties_even();
    if !value.is_finite() || value < minimum || value >= maximum_exclusive {
        bail!("Double value is outside the requested integral range");
    }
    Ok(value)
}

fn convert_to_i32(value: &IntrinsicValue) -> Result<i32> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_i32(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Int16(value) => Ok(i32::from(*value)),
        IntrinsicValue::Int32(value) => Ok(*value),
        IntrinsicValue::Int64(value) => Ok((*value).try_into()?),
        IntrinsicValue::UInt64(value) => Ok((*value).try_into()?),
        IntrinsicValue::Double(value) => checked_f64_to_i32(*value),
        IntrinsicValue::Null => bail!("Convert.ToInt32(null) is ambiguous"),
        _ => bail!("Convert.ToInt32 does not support {}", value.type_name()),
    }
}

fn convert_to_i64(value: &IntrinsicValue) -> Result<i64> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_i64(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Int16(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int32(value) => Ok(i64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value),
        IntrinsicValue::UInt64(value) => Ok((*value).try_into()?),
        IntrinsicValue::Double(value) => checked_f64_to_i64(*value),
        IntrinsicValue::Null => bail!("Convert.ToInt64(null) is ambiguous"),
        _ => bail!("Convert.ToInt64 does not support {}", value.type_name()),
    }
}

fn convert_to_u64(value: &IntrinsicValue) -> Result<u64> {
    match value {
        IntrinsicValue::String(value) => parse_decimal_u64(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1 } else { 0 }),
        IntrinsicValue::Int16(value) => Ok((*value).try_into()?),
        IntrinsicValue::Int32(value) => Ok((*value).try_into()?),
        IntrinsicValue::Int64(value) => Ok((*value).try_into()?),
        IntrinsicValue::UInt64(value) => Ok(*value),
        IntrinsicValue::Double(value) => checked_f64_to_u64(*value),
        IntrinsicValue::Null => bail!("Convert.ToUInt64(null) is ambiguous"),
        _ => bail!("Convert.ToUInt64 does not support {}", value.type_name()),
    }
}

fn convert_to_f64(value: &IntrinsicValue) -> Result<f64> {
    match value {
        IntrinsicValue::String(value) => parse_invariant_f64(value),
        IntrinsicValue::Boolean(value) => Ok(if *value { 1.0 } else { 0.0 }),
        IntrinsicValue::Int16(value) => Ok(f64::from(*value)),
        IntrinsicValue::Int32(value) => Ok(f64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value as f64),
        IntrinsicValue::UInt64(value) => Ok(*value as f64),
        IntrinsicValue::Double(value) => Ok(*value),
        IntrinsicValue::Null => bail!("Convert.ToDouble(null) is ambiguous"),
        _ => bail!("Convert.ToDouble does not support {}", value.type_name()),
    }
}

fn convert_to_bool(value: &IntrinsicValue, member: &str) -> Result<bool> {
    match value {
        IntrinsicValue::String(value) => parse_boolean(value, member),
        IntrinsicValue::Boolean(value) => Ok(*value),
        IntrinsicValue::Int16(value) => Ok(*value != 0),
        IntrinsicValue::Int32(value) => Ok(*value != 0),
        IntrinsicValue::Int64(value) => Ok(*value != 0),
        IntrinsicValue::UInt64(value) => Ok(*value != 0),
        IntrinsicValue::Double(value) => Ok(*value != 0.0),
        IntrinsicValue::Null => bail!("Convert.ToBoolean(null) is ambiguous"),
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
        (IntrinsicValue::Int32(left), IntrinsicValue::Int32(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::Int64(left), IntrinsicValue::Int64(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::UInt64(left), IntrinsicValue::UInt64(right)) => Ok(left.cmp(right)),
        (IntrinsicValue::Double(left), IntrinsicValue::Double(right)) => {
            Ok(if left.is_nan() && right.is_nan() {
                Ordering::Equal
            } else if left.is_nan() {
                Ordering::Less
            } else if right.is_nan() {
                Ordering::Greater
            } else {
                left.partial_cmp(right)
                    .ok_or_else(|| anyhow!("Double values could not be compared"))?
            })
        }
        _ => bail!("Numeric CompareTo received mismatched coerced types"),
    }
}

fn numeric_equals(left: &IntrinsicValue, right: &IntrinsicValue) -> Result<bool> {
    Ok(match (left, right) {
        (IntrinsicValue::Int32(left), IntrinsicValue::Int32(right)) => left == right,
        (IntrinsicValue::Int64(left), IntrinsicValue::Int64(right)) => left == right,
        (IntrinsicValue::UInt64(left), IntrinsicValue::UInt64(right)) => left == right,
        (IntrinsicValue::Double(left), IntrinsicValue::Double(right)) => {
            left == right || (left.is_nan() && right.is_nan())
        }
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

fn format_double(value: f64) -> Result<String> {
    if value.is_nan() {
        return Ok("NaN".to_string());
    }
    if value == f64::INFINITY {
        return Ok("Infinity".to_string());
    }
    if value == f64::NEG_INFINITY {
        return Ok("-Infinity".to_string());
    }
    if value == 0.0 {
        return Ok(if value.is_sign_negative() { "-0" } else { "0" }.to_string());
    }

    let exponent = value.abs().log10().floor() as i32;
    if !(-4..17).contains(&exponent) {
        let scientific = format!("{value:e}");
        let (mantissa, exponent) = scientific
            .split_once('e')
            .ok_or_else(|| anyhow!("Rust produced an invalid scientific Double"))?;
        let exponent: i32 = exponent.parse()?;
        return Ok(format!("{mantissa}E{exponent:+03}"));
    }

    let text = value.to_string();
    if let Some((mantissa, exponent)) = text.split_once('e') {
        let exponent: i32 = exponent.parse()?;
        return expand_scientific(mantissa, exponent);
    }
    Ok(text)
}

fn expand_scientific(mantissa: &str, exponent: i32) -> Result<String> {
    let (sign, mantissa) = mantissa
        .strip_prefix('-')
        .map_or(("", mantissa), |value| ("-", value));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len());
    let digits = mantissa.replace('.', "");
    let decimal = i32::try_from(decimal)? + exponent;
    let result = if decimal <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat(usize::try_from(-decimal)?))
    } else {
        let decimal = usize::try_from(decimal)?;
        if decimal >= digits.len() {
            format!("{sign}{digits}{}", "0".repeat(decimal - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..decimal], &digits[decimal..])
        }
    };
    Ok(result)
}

fn invariant_case(value: &str, uppercase: bool) -> String {
    let mapper = CaseMapper::new();
    value
        .chars()
        .map(|character| {
            if uppercase {
                mapper.simple_uppercase(character)
            } else {
                mapper.simple_lowercase(character)
            }
        })
        .collect()
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
    if !value.starts_with("{0x") && !value.starts_with("{0X") {
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
            bail!("Value is not in a supported Guid N, D, B, P, or X grammar");
        }
        let value = if (value.starts_with('{') && value.ends_with('}'))
            || (value.starts_with('(') && value.ends_with(')'))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        return Uuid::parse_str(value).map_err(Into::into);
    }
    let inner = value
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
        .ok_or_else(|| anyhow!("Invalid Guid X format"))?;
    let (head, bytes) = inner
        .split_once(",{")
        .ok_or_else(|| anyhow!("Invalid Guid X format"))?;
    let bytes = bytes
        .strip_suffix('}')
        .ok_or_else(|| anyhow!("Invalid Guid X byte group"))?;
    let head = head.split(',').collect::<Vec<_>>();
    let bytes = bytes.split(',').collect::<Vec<_>>();
    if head.len() != 3 || bytes.len() != 8 {
        bail!("Invalid Guid X component count");
    }
    let a = parse_prefixed_hex(head[0], 8)? as u32;
    let b = parse_prefixed_hex(head[1], 4)? as u16;
    let c = parse_prefixed_hex(head[2], 4)? as u16;
    let mut d = [0u8; 8];
    for (target, source) in d.iter_mut().zip(bytes) {
        *target = parse_prefixed_hex(source, 2)? as u8;
    }
    Ok(Uuid::from_fields(a, b, c, &d))
}

fn parse_prefixed_hex(value: &str, digits: usize) -> Result<u64> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or_else(|| anyhow!("Guid X component is missing its 0x prefix"))?;
    if value.len() != digits || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Guid X component must contain exactly {digits} hexadecimal digits");
    }
    Ok(u64::from_str_radix(value, 16)?)
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

fn compare_sdk_versions(left: &str, right: &str) -> Ordering {
    let parse = |value: &str| {
        value
            .trim_start_matches(['v', 'V'])
            .split(['.', '-'])
            .take_while(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut left = parse(left);
    let mut right = parse(right);
    let length = left.len().max(right.len());
    left.resize(length, 0);
    right.resize(length, 0);
    left.cmp(&right)
}

fn make_relative(base: &str, path: &str) -> String {
    if let Ok(relative) = Path::new(path).strip_prefix(base) {
        return display_path(relative);
    }
    let is_windows_path = |value: &str| {
        value.contains('\\')
            || value
                .as_bytes()
                .get(1)
                .is_some_and(|character| *character == b':')
    };
    if is_windows_path(base) || is_windows_path(path) {
        let base_components = base
            .split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let path_components = path
            .split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        if path_components.len() >= base_components.len()
            && path_components
                .iter()
                .zip(&base_components)
                .all(|(path, base)| path.eq_ignore_ascii_case(base))
        {
            let separator = if path.contains('\\') { "\\" } else { "/" };
            return path_components[base_components.len()..].join(separator);
        }
    }
    path.to_string()
}

fn find_file_above(start: &str, file_name: &str) -> Option<PathBuf> {
    let mut directory = PathBuf::from(start.replace('/', std::path::MAIN_SEPARATOR_STR));
    if directory.is_file() {
        directory.pop();
    }
    loop {
        let candidate = directory.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !directory.pop() {
            return None;
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

fn with_trailing_separator(mut value: String) -> String {
    if !value.ends_with(['/', '\\']) {
        value.push(std::path::MAIN_SEPARATOR);
    }
    value
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
                && entry.member == "Max"
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
    fn overload_arity_is_deterministic() {
        let error = resolve("System.Math", "Max", InvocationKind::StaticMethod, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no allowlisted overload accepting 1"));
    }

    fn call(
        type_name: &str,
        member: &str,
        arguments: Vec<IntrinsicValue>,
    ) -> Result<IntrinsicValue> {
        let descriptor = resolve(
            type_name,
            member,
            InvocationKind::StaticMethod,
            arguments.len(),
        )?;
        invoke(
            descriptor,
            &IntrinsicContext {
                base_directory: Path::new("."),
                tools_directory: None,
            },
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
                "Max",
                vec![
                    IntrinsicValue::String("9007199254740992".to_string()),
                    IntrinsicValue::String("9007199254740993".to_string()),
                ],
            )?,
            IntrinsicValue::Double(9_007_199_254_740_992.0)
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
        for member in ["ToInt32", "ToString"] {
            let error = call("System.Convert", member, vec![IntrinsicValue::Null])
                .unwrap_err()
                .to_string();
            assert!(error.contains("Ambiguous native overload"));
        }
        let copy = call("System.String", "Copy", vec![IntrinsicValue::Null])
            .unwrap_err()
            .to_string();
        assert!(copy.contains("does not accept null"));
    }
}
