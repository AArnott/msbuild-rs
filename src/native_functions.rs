use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
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
    Integer,
    Number,
    Version,
    Path,
    Radix,
    RegistryView,
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
    pub value: String,
    pub is_null: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IntrinsicValue {
    String(String),
    Strings(Vec<String>),
    Boolean(bool),
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
            Self::String(_) => "System.String",
            Self::Strings(_) => "System.String[]",
            Self::Boolean(_) => "System.Boolean",
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
            Self::String(value) => Ok(value.clone()),
            Self::Strings(values) => Ok(values.join(";")),
            Self::Boolean(value) => Ok(dotnet_bool(*value).to_string()),
            Self::Int32(value) => Ok(value.to_string()),
            Self::Int64(value) => Ok(value.to_string()),
            Self::UInt64(value) => Ok(value.to_string()),
            Self::Double(value) => format_double(*value),
            Self::Version(value) => Ok(value.to_string()),
            Self::Guid(value) => Ok(value.to_string()),
            Self::DateTime(value) => Ok(value.default_string()),
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
        let (date, time) = value.trim().split_once(' ').unwrap_or((value.trim(), ""));
        let (year, month, day) = if date.contains('-') {
            let parts = date.split('-').collect::<Vec<_>>();
            if parts.len() != 3 {
                bail!("MSB4184: '{value}' is not a supported System.DateTime");
            }
            (parts[0].parse()?, parts[1].parse()?, parts[2].parse()?)
        } else {
            let parts = date.split('/').collect::<Vec<_>>();
            if parts.len() != 3 {
                bail!("MSB4184: '{value}' is not a supported System.DateTime");
            }
            (parts[2].parse()?, parts[0].parse()?, parts[1].parse()?)
        };
        let (hour, minute, second) = if time.is_empty() {
            (0, 0, 0)
        } else {
            let parts = time.split(':').collect::<Vec<_>>();
            if !(2..=3).contains(&parts.len()) {
                bail!("MSB4184: '{value}' is not a supported System.DateTime");
            }
            (
                parts[0].parse()?,
                parts[1].parse()?,
                parts.get(2).map_or(Ok(0), |part| part.parse())?,
            )
        };
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            bail!("MSB4184: '{value}' is not a supported System.DateTime");
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

    fn default_string(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
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
                    .expect("position is within the string");
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

const O0: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(0),
    coercions: &[],
}];
const O1_ANY: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::Any],
}];
const O1_STRING: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::String],
}];
const O1_INTEGER: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::Integer],
}];
const O1_NUMBER: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::Number],
}];
const O1_BOOLEAN: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::Boolean],
}];
const O1_VERSION: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(1),
    coercions: &[Coercion::Version],
}];
const O2_STRING: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(2),
    coercions: &[Coercion::String, Coercion::String],
}];
const O2_NUMBER: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(2),
    coercions: &[Coercion::Number, Coercion::Number],
}];
const O2_INTEGER: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(2),
    coercions: &[Coercion::Integer, Coercion::Integer],
}];
const O_INTEGER_STRING: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(2),
    coercions: &[Coercion::Integer, Coercion::String],
}];
const O3_STRING_INTEGER: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Exact(3),
    coercions: &[Coercion::String, Coercion::Integer, Coercion::Integer],
}];
const O_STRING_1_2: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::String],
    },
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::String, Coercion::String],
    },
];
const O_STRING_OR_STRING_INTEGER: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::String],
    },
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::String, Coercion::Integer],
    },
];
const O_INTEGER_1_2: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::Integer],
    },
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::Integer, Coercion::Integer],
    },
];
const O0_1_STRING: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(0),
        coercions: &[],
    },
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::String],
    },
];
const O0_1_INTEGER: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(0),
        coercions: &[],
    },
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::Integer],
    },
];
const O_CONVERT_INTEGER: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::Any],
    },
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::String, Coercion::Radix],
    },
];
const O_NUMBER_1_2: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(1),
        coercions: &[Coercion::Number],
    },
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::Number, Coercion::Integer],
    },
];
const O_PATH_1_16: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Range(1, 16),
    coercions: &[Coercion::Path],
}];
const O_PATH_2_4: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Range(2, 4),
    coercions: &[Coercion::Path],
}];
const O_VERSION_NEW: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Range(2, 4),
    coercions: &[Coercion::Integer],
}];
const O_JOIN: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::AtLeast(2),
    coercions: &[Coercion::String],
}];
const O_REGISTRY_VALUE: &[OverloadDescriptor] = &[
    OverloadDescriptor {
        arity: Arity::Exact(2),
        coercions: &[Coercion::String, Coercion::String],
    },
    OverloadDescriptor {
        arity: Arity::Exact(3),
        coercions: &[Coercion::String, Coercion::String, Coercion::Any],
    },
];
const O_REGISTRY_VIEWS: &[OverloadDescriptor] = &[OverloadDescriptor {
    arity: Arity::Range(3, 12),
    coercions: &[
        Coercion::String,
        Coercion::String,
        Coercion::Any,
        Coercion::RegistryView,
    ],
}];

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
        O_PATH_1_16,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "NormalizeDirectory",
        StaticMethod,
        Decoded,
        Escape,
        O_PATH_1_16,
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
        O2_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseAnd",
        StaticMethod,
        Decoded,
        Escape,
        O2_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseXor",
        StaticMethod,
        Decoded,
        Escape,
        O2_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "BitwiseNot",
        StaticMethod,
        Decoded,
        Escape,
        O1_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "LeftShift",
        StaticMethod,
        Decoded,
        Escape,
        O2_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "RightShift",
        StaticMethod,
        Decoded,
        Escape,
        O2_INTEGER,
        "System.Int32",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "ValueOrDefault",
        StaticMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.String",
        handle_msbuild
    ),
    intrinsic!(
        "MSBuild",
        "Escape",
        StaticMethod,
        Decoded,
        AlreadyEscaped,
        O1_STRING,
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
        O_STRING_OR_STRING_INTEGER,
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
        O_STRING_OR_STRING_INTEGER,
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
        O3_STRING_INTEGER,
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
        O1_ANY,
        "System.Boolean",
        handle_string_static
    ),
    intrinsic!(
        "System.String",
        "IsNullOrWhiteSpace",
        StaticMethod,
        Decoded,
        Escape,
        O1_ANY,
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
        O_INTEGER_1_2,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "ToLower",
        InstanceMethod,
        Decoded,
        Escape,
        O0,
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
        "ToUpper",
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
        O0_1_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimStart",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "TrimEnd",
        InstanceMethod,
        Decoded,
        Escape,
        O0_1_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Replace",
        InstanceMethod,
        Decoded,
        Escape,
        O2_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Split",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.String[]",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
        "System.Boolean",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
        "System.Int32",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "IndexOf",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Int32",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "LastIndexOf",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Int32",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Insert",
        InstanceMethod,
        Decoded,
        Escape,
        O_INTEGER_STRING,
        "System.String",
        handle_string_instance
    ),
    intrinsic!(
        "System.String",
        "Remove",
        InstanceMethod,
        Decoded,
        Escape,
        O_INTEGER_1_2,
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
        O1_INTEGER,
        "System.String",
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
        O1_INTEGER,
        "System.String",
        handle_string_array
    ),
    intrinsic!(
        "System.IO.Path",
        "Combine",
        StaticMethod,
        Decoded,
        Escape,
        O_PATH_2_4,
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
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Min",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Int64/System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Abs",
        StaticMethod,
        Decoded,
        Escape,
        O1_NUMBER,
        "System.Int64/System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Ceiling",
        StaticMethod,
        Decoded,
        Escape,
        O1_NUMBER,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Floor",
        StaticMethod,
        Decoded,
        Escape,
        O1_NUMBER,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Truncate",
        StaticMethod,
        Decoded,
        Escape,
        O1_NUMBER,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Round",
        StaticMethod,
        Decoded,
        Escape,
        O_NUMBER_1_2,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Pow",
        StaticMethod,
        Decoded,
        Escape,
        O2_NUMBER,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Math",
        "Sqrt",
        StaticMethod,
        Decoded,
        Escape,
        O1_NUMBER,
        "System.Double",
        handle_math
    ),
    intrinsic!(
        "System.Convert",
        "ToInt32",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INTEGER,
        "System.Int32",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToInt64",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INTEGER,
        "System.Int64",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToUInt64",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INTEGER,
        "System.UInt64",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToDouble",
        StaticMethod,
        Decoded,
        Escape,
        O1_ANY,
        "System.Double",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToBoolean",
        StaticMethod,
        Decoded,
        Escape,
        O1_BOOLEAN,
        "System.Boolean",
        handle_convert
    ),
    intrinsic!(
        "System.Convert",
        "ToString",
        StaticMethod,
        Decoded,
        Escape,
        O_CONVERT_INTEGER,
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
        O0_1_INTEGER,
        "System.String",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "CompareTo",
        InstanceMethod,
        Decoded,
        Escape,
        O1_VERSION,
        "System.Int32",
        handle_version
    ),
    intrinsic!(
        "System.Version",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_VERSION,
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
        O0_1_STRING,
        "System.String",
        handle_guid
    ),
    intrinsic!(
        "System.Guid",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_STRING,
        "System.Boolean",
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
        O0_1_STRING,
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
        O1_ANY,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int32",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
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
        O1_ANY,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Int64",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
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
        O1_ANY,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.UInt64",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
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
        O1_ANY,
        "System.Int32",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Double",
        "Equals",
        InstanceMethod,
        Decoded,
        Escape,
        O1_ANY,
        "System.Boolean",
        handle_numeric_instance
    ),
    intrinsic!(
        "System.Double",
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
    (descriptor.handler)(descriptor, context, receiver, arguments).with_context(|| {
        format!(
            "MSB4184: The expression invoking [{}]::{} could not be evaluated",
            descriptor.type_name, descriptor.member
        )
    })
}

fn handle_string_static(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("IsNullOrEmpty") {
        Ok(IntrinsicValue::Boolean(
            arguments[0].is_null || arguments[0].value.is_empty(),
        ))
    } else if operation == member_code("IsNullOrWhiteSpace") {
        Ok(IntrinsicValue::Boolean(
            arguments[0].is_null || arguments[0].value.trim().is_empty(),
        ))
    } else if operation == member_code("Join") {
        Ok(IntrinsicValue::String(
            arguments[1..]
                .iter()
                .map(|argument| argument.value.as_str())
                .collect::<Vec<_>>()
                .join(&arguments[0].value),
        ))
    } else {
        Ok(IntrinsicValue::String(arguments[0].value.clone()))
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
            receiver.contains(&arguments[0].value),
        ))
    } else if operation == member_code("StartsWith") {
        Ok(IntrinsicValue::Boolean(
            receiver.starts_with(&arguments[0].value),
        ))
    } else if operation == member_code("EndsWith") {
        Ok(IntrinsicValue::Boolean(
            receiver.ends_with(&arguments[0].value),
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
    } else if operation == member_code("ToLower") || operation == member_code("ToLowerInvariant") {
        Ok(IntrinsicValue::String(receiver.to_lowercase()))
    } else if operation == member_code("ToUpper") || operation == member_code("ToUpperInvariant") {
        Ok(IntrinsicValue::String(receiver.to_uppercase()))
    } else if matches!(
        operation,
        value
            if value == member_code("Trim")
                || value == member_code("TrimStart")
                || value == member_code("TrimEnd")
    ) {
        let characters = arguments
            .first()
            .map_or_else(|| " \t\r\n".to_string(), |argument| argument.value.clone());
        let matches = |character| characters.contains(character);
        let value = if operation == member_code("TrimStart") {
            receiver.trim_start_matches(matches)
        } else if operation == member_code("TrimEnd") {
            receiver.trim_end_matches(matches)
        } else {
            receiver.trim_matches(matches)
        };
        Ok(IntrinsicValue::String(value.to_string()))
    } else if operation == member_code("Replace") {
        Ok(IntrinsicValue::String(
            receiver.replace(&arguments[0].value, &arguments[1].value),
        ))
    } else if operation == member_code("Split") {
        let separators = &arguments[0].value;
        let values = if separators.is_empty() {
            receiver
                .split_whitespace()
                .map(ToString::to_string)
                .collect()
        } else {
            receiver
                .split(|character| separators.contains(character))
                .map(ToString::to_string)
                .collect()
        };
        Ok(IntrinsicValue::Strings(values))
    } else if operation == member_code("Equals") {
        Ok(IntrinsicValue::Boolean(
            receiver.as_str() == arguments[0].value,
        ))
    } else if operation == member_code("CompareTo") {
        Ok(IntrinsicValue::Int32(ordering_i32(
            receiver.as_str().cmp(&arguments[0].value),
        )))
    } else if operation == member_code("IndexOf") {
        Ok(IntrinsicValue::Int32(
            receiver.find(&arguments[0].value).map_or(-1, |position| {
                receiver[..position].encode_utf16().count() as i32
            }),
        ))
    } else if operation == member_code("LastIndexOf") {
        Ok(IntrinsicValue::Int32(
            receiver.rfind(&arguments[0].value).map_or(-1, |position| {
                receiver[..position].encode_utf16().count() as i32
            }),
        ))
    } else if operation == member_code("Insert") {
        let index = argument_usize(&arguments[0], member)?;
        let byte_index = utf16_byte_index(receiver, index)?;
        let mut result = receiver.clone();
        result.insert_str(byte_index, &arguments[1].value);
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
        Ok(IntrinsicValue::Int32(receiver.encode_utf16().count() as i32))
    } else if operation == member_code("Item") {
        let index = argument_usize(&arguments[0], member)?;
        Ok(IntrinsicValue::String(utf16_substring(
            receiver,
            index,
            Some(1),
        )?))
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
        Ok(IntrinsicValue::Int32(receiver.len() as i32))
    } else {
        let index = argument_usize(&arguments[0], descriptor.member)?;
        receiver
            .get(index)
            .cloned()
            .map(IntrinsicValue::String)
            .ok_or_else(|| anyhow!("String array index {index} is out of range"))
    }
}

fn handle_path(
    descriptor: &IntrinsicDescriptor,
    context: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("Combine") {
        let mut path = PathBuf::from(&arguments[0].value);
        for argument in &arguments[1..] {
            path.push(&argument.value);
        }
        Ok(IntrinsicValue::String(display_path(&path)))
    } else if operation == member_code("IsPathRooted") {
        Ok(IntrinsicValue::Boolean(
            Path::new(&arguments[0].value).has_root(),
        ))
    } else if operation == member_code("GetDirectoryName") {
        Ok(IntrinsicValue::String(
            Path::new(&arguments[0].value)
                .parent()
                .map(display_path)
                .unwrap_or_default(),
        ))
    } else if operation == member_code("GetFileName") {
        Ok(IntrinsicValue::String(
            Path::new(&arguments[0].value)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ))
    } else if operation == member_code("GetFileNameWithoutExtension") {
        Ok(IntrinsicValue::String(
            Path::new(&arguments[0].value)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ))
    } else if operation == member_code("GetExtension") {
        Ok(IntrinsicValue::String(
            Path::new(&arguments[0].value)
                .extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("GetFullPath") {
        let path = if arguments.len() == 2 {
            Path::new(&arguments[1].value).join(&arguments[0].value)
        } else {
            PathBuf::from(&arguments[0].value)
        };
        Ok(IntrinsicValue::String(display_path(
            &lexical_absolute(&path).unwrap_or_else(|_| context.base_directory.join(path)),
        )))
    } else if operation == member_code("GetPathRoot") {
        let root = Path::new(&arguments[0].value)
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
            Path::new(&arguments[0].value).extension().is_some(),
        ))
    } else if operation == member_code("ChangeExtension") {
        let mut path = PathBuf::from(&arguments[0].value);
        let extension = arguments[1].value.trim_start_matches('.');
        path.set_extension(extension);
        Ok(IntrinsicValue::String(display_path(&path)))
    } else if operation == member_code("GetTempPath") {
        Ok(IntrinsicValue::String(with_trailing_separator(
            display_path(&std::env::temp_dir()),
        )))
    } else if operation == member_code("DirectorySeparatorChar") {
        Ok(IntrinsicValue::String(
            std::path::MAIN_SEPARATOR.to_string(),
        ))
    } else if operation == member_code("AltDirectorySeparatorChar") {
        Ok(IntrinsicValue::String("/".to_string()))
    } else if operation == member_code("PathSeparator") {
        Ok(IntrinsicValue::String(
            if cfg!(windows) { ";" } else { ":" }.to_string(),
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
    if matches!(
        operation,
        value if value == member_code("Max") || value == member_code("Min")
    ) && arguments.iter().all(is_integer_argument)
    {
        let left = argument_i64(&arguments[0], member)?;
        let right = argument_i64(&arguments[1], member)?;
        return Ok(IntrinsicValue::Int64(if operation == member_code("Max") {
            left.max(right)
        } else {
            left.min(right)
        }));
    }
    if operation == member_code("Abs") && is_integer_argument(&arguments[0]) {
        return Ok(IntrinsicValue::Int64(
            argument_i64(&arguments[0], member)?
                .checked_abs()
                .ok_or_else(|| {
                    anyhow!("MSB4184: System.Math.Abs overflowed its integer argument")
                })?,
        ));
    }
    let left = argument_f64(&arguments[0], member)?;
    let value = if operation == member_code("Max") {
        left.max(argument_f64(&arguments[1], member)?)
    } else if operation == member_code("Min") {
        left.min(argument_f64(&arguments[1], member)?)
    } else if operation == member_code("Abs") {
        left.abs()
    } else if operation == member_code("Ceiling") {
        left.ceil()
    } else if operation == member_code("Floor") {
        left.floor()
    } else if operation == member_code("Truncate") {
        left.trunc()
    } else if operation == member_code("Round") {
        if let Some(digits) = arguments.get(1) {
            let factor = 10f64.powi(argument_i32(digits, member)?);
            (left * factor).round_ties_even() / factor
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
    finite_double(value, member).map(IntrinsicValue::Double)
}

fn handle_environment(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    _: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    let operation = descriptor.dispatch_code;
    if operation == member_code("ExpandEnvironmentVariables") {
        let mut output = arguments[0].value.clone();
        for (name, value) in std::env::vars() {
            output = output.replace(&format!("%{name}%"), &value);
        }
        Ok(IntrinsicValue::String(output))
    } else if operation == member_code("GetEnvironmentVariable") {
        Ok(IntrinsicValue::String(
            std::env::var(&arguments[0].value).unwrap_or_default(),
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
                .map(|count| count.get() as i32)
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
            parse_radix_i64(&arguments[0].value, radix)?.try_into()?
        } else {
            argument_i32(&arguments[0], member)?
        }))
    } else if operation == member_code("ToInt64") {
        Ok(IntrinsicValue::Int64(if let Some(radix) = radix {
            parse_radix_i64(&arguments[0].value, radix)?
        } else {
            argument_i64(&arguments[0], member)?
        }))
    } else if operation == member_code("ToUInt64") {
        Ok(IntrinsicValue::UInt64(if let Some(radix) = radix {
            u64::from_str_radix(&arguments[0].value, radix as u32)?
        } else {
            arguments[0].value.parse()?
        }))
    } else if operation == member_code("ToDouble") {
        Ok(IntrinsicValue::Double(argument_f64(&arguments[0], member)?))
    } else if operation == member_code("ToBoolean") {
        Ok(IntrinsicValue::Boolean(argument_bool(
            &arguments[0],
            member,
        )?))
    } else if operation == member_code("ToString") {
        if let Some(radix) = radix {
            let value = argument_i64(&arguments[0], member)?;
            let text = match radix {
                2 => format!("{value:b}"),
                8 => format!("{value:o}"),
                10 => value.to_string(),
                16 => format!("{value:x}"),
                _ => bail!("MSB4184: Convert.ToString radix must be 2, 8, 10, or 16"),
            };
            Ok(IntrinsicValue::String(text))
        } else {
            Ok(IntrinsicValue::String(arguments[0].value.clone()))
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
            &arguments[0].value,
        )?));
    }
    if descriptor.kind == InvocationKind::Constructor {
        return Ok(IntrinsicValue::Version(NativeVersion::from_arguments(
            arguments,
        )?));
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
        let other = NativeVersion::parse(&arguments[0].value)?;
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
        return Ok(IntrinsicValue::Guid(Uuid::parse_str(&arguments[0].value)?));
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
    if operation == member_code("Equals") {
        Ok(IntrinsicValue::Boolean(
            *guid == Uuid::parse_str(&arguments[0].value)?,
        ))
    } else {
        let format = arguments
            .first()
            .map_or("D", |argument| argument.value.as_str());
        let text = if format.eq_ignore_ascii_case("N") {
            guid.simple().to_string()
        } else if format.eq_ignore_ascii_case("D") {
            guid.hyphenated().to_string()
        } else if format.eq_ignore_ascii_case("B") {
            format!("{{{}}}", guid.hyphenated())
        } else if format.eq_ignore_ascii_case("P") {
            format!("({})", guid.hyphenated())
        } else {
            bail!("MSB4184: Unsupported System.Guid format '{format}'")
        };
        Ok(IntrinsicValue::String(text))
    }
}

fn handle_datetime(
    descriptor: &IntrinsicDescriptor,
    _: &IntrinsicContext<'_>,
    receiver: Option<&IntrinsicValue>,
    arguments: &[IntrinsicArgument],
) -> Result<IntrinsicValue> {
    if descriptor.kind == InvocationKind::StaticMethod {
        return Ok(IntrinsicValue::DateTime(NativeDateTime::parse(
            &arguments[0].value,
        )?));
    }
    let Some(IntrinsicValue::DateTime(value)) = receiver else {
        bail!("{} requires a System.DateTime receiver", descriptor.member);
    };
    Ok(IntrinsicValue::String(arguments.first().map_or_else(
        || Ok(value.default_string()),
        |format| value.format(&format.value),
    )?))
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
        if is_i32 {
            Ok(IntrinsicValue::Int32(arguments[0].value.parse()?))
        } else {
            Ok(IntrinsicValue::Int64(arguments[0].value.parse()?))
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
        let format = arguments.first().map(|argument| argument.value.as_str());
        return Ok(IntrinsicValue::String(format_numeric(receiver, format)?));
    }
    let left = numeric_value(receiver)?;
    let right = argument_f64(&arguments[0], descriptor.member)?;
    if operation == member_code("CompareTo") {
        Ok(IntrinsicValue::Int32(ordering_i32(
            left.partial_cmp(&right)
                .ok_or_else(|| anyhow!("NaN cannot be compared"))?,
        )))
    } else {
        Ok(IntrinsicValue::Boolean(left == right))
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
        if arguments[0].value.is_empty() {
            bail!("IsOSPlatform platform cannot be empty");
        }
        let platform = match std::env::consts::OS {
            "windows" => "Windows",
            "linux" => "Linux",
            "macos" => "OSX",
            _ => "",
        };
        Ok(IntrinsicValue::Boolean(
            arguments[0].value.eq_ignore_ascii_case(platform),
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
        let ordering = compare_sdk_versions(&arguments[0].value, &arguments[1].value);
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
            find_file_above(&arguments[0].value, &arguments[1].value)
                .and_then(|path| path.parent().map(display_path))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("GetPathOfFileAbove") {
        let file_name = Path::new(&arguments[0].value)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        Ok(IntrinsicValue::String(
            find_file_above(&arguments[1].value, &file_name)
                .map(|path| display_path(&path))
                .unwrap_or_default(),
        ))
    } else if operation == member_code("MakeRelative") {
        Ok(IntrinsicValue::String(make_relative(
            &arguments[0].value,
            &arguments[1].value,
        )))
    } else if operation == member_code("NormalizePath")
        || operation == member_code("NormalizeDirectory")
    {
        let mut path = PathBuf::from(&arguments[0].value);
        for argument in &arguments[1..] {
            path.push(&argument.value);
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
        Ok(IntrinsicValue::String(if arguments[0].value.is_empty() {
            String::new()
        } else {
            with_trailing_separator(arguments[0].value.clone())
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
                left << right
            } else {
                left >> right
            },
        ))
    } else if operation == member_code("BitwiseNot") {
        Ok(IntrinsicValue::Int32(!argument_i32(&arguments[0], member)?))
    } else if operation == member_code("ValueOrDefault") {
        Ok(IntrinsicValue::String(if arguments[0].value.is_empty() {
            arguments[1].value.clone()
        } else {
            arguments[0].value.clone()
        }))
    } else if operation == member_code("Escape") {
        Ok(IntrinsicValue::String(escape_lower(&arguments[0].value)))
    } else if operation == member_code("Unescape") {
        Ok(IntrinsicValue::String(unescape_once(&arguments[0].value)))
    } else if operation == member_code("GetTargetFrameworkIdentifier") {
        Ok(IntrinsicValue::String(
            target_framework(&arguments[0].value).0,
        ))
    } else if operation == member_code("GetTargetFrameworkVersion") {
        let (_, version) = target_framework(&arguments[0].value);
        Ok(IntrinsicValue::String(format_version_parts(
            &version,
            optional_part_count(arguments)?,
        )))
    } else if operation == member_code("GetTargetPlatformIdentifier") {
        Ok(IntrinsicValue::String(
            target_platform(&arguments[0].value).0,
        ))
    } else if operation == member_code("GetTargetPlatformVersion") {
        let (_, version) = target_platform(&arguments[0].value);
        Ok(IntrinsicValue::String(format_version_parts(
            &version,
            optional_part_count(arguments)?,
        )))
    } else if operation == member_code("IsTargetFrameworkCompatible") {
        Ok(IntrinsicValue::Boolean(target_framework_compatible(
            &arguments[0].value,
            &arguments[1].value,
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
            arguments[0]
                .value
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
            left.checked_add(right)
        } else if operation == member_code("Subtract") {
            left.checked_sub(right)
        } else if operation == member_code("Multiply") {
            left.checked_mul(right)
        } else if operation == member_code("Divide") {
            (right != 0).then(|| left / right)
        } else {
            (right != 0).then(|| left % right)
        }
        .ok_or_else(|| anyhow!("{member} overflowed or divided by zero"))?;
        return Ok(IntrinsicValue::Int64(value));
    }
    let left = argument_f64(&arguments[0], member)?;
    let right = argument_f64(&arguments[1], member)?;
    if matches!(
        operation,
        value if value == member_code("Divide") || value == member_code("Modulo")
    ) && right == 0.0
    {
        bail!("Cannot calculate {member} with a zero divisor");
    }
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
    finite_double(value, member).map(IntrinsicValue::Double)
}

fn registry_intrinsic(
    arguments: &[IntrinsicArgument],
    views_supplied: bool,
) -> Result<IntrinsicValue> {
    let default = arguments
        .get(2)
        .filter(|argument| !argument.is_null)
        .map(|argument| argument.value.clone());

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
                .map(|argument| RegistryView::parse(&argument.value))
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![RegistryView::Default]
        };
        Ok(IntrinsicValue::String(
            read_registry_value(
                &arguments[0].value,
                (!arguments[1].value.is_empty()).then_some(arguments[1].value.as_str()),
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
        .map_or("Legacy", |argument| argument.value.as_str());
    let value = &arguments[0].value;
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
    if value.contains('.') {
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
    arguments
        .get(1)
        .map(|argument| argument_usize(argument, "version part count"))
        .transpose()
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

fn is_integer_argument(argument: &IntrinsicArgument) -> bool {
    argument.is_null
        || (!argument.value.contains(['.', 'e', 'E']) && argument.value.parse::<i64>().is_ok())
}

fn argument_i32(argument: &IntrinsicArgument, member: &str) -> Result<i32> {
    if argument.is_null {
        return Ok(0);
    }
    argument
        .value
        .parse()
        .with_context(|| format!("{member} argument '{}' is not an Int32", argument.value))
}

fn argument_i64(argument: &IntrinsicArgument, member: &str) -> Result<i64> {
    if argument.is_null {
        return Ok(0);
    }
    argument
        .value
        .parse()
        .with_context(|| format!("{member} argument '{}' is not an Int64", argument.value))
}

fn argument_usize(argument: &IntrinsicArgument, member: &str) -> Result<usize> {
    if argument.is_null {
        return Ok(0);
    }
    argument.value.parse().with_context(|| {
        format!(
            "{member} argument '{}' is not a nonnegative index",
            argument.value
        )
    })
}

fn argument_f64(argument: &IntrinsicArgument, member: &str) -> Result<f64> {
    if argument.is_null {
        return Ok(0.0);
    }
    let value = argument
        .value
        .parse::<f64>()
        .with_context(|| format!("{member} argument '{}' is not numeric", argument.value))?;
    finite_double(value, member)
}

fn argument_bool(argument: &IntrinsicArgument, member: &str) -> Result<bool> {
    if argument.is_null {
        return Ok(false);
    }
    if argument.value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if argument.value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        bail!("{member} argument '{}' is not Boolean", argument.value)
    }
}

fn parse_radix_i64(value: &str, radix: i32) -> Result<i64> {
    if !matches!(radix, 2 | 8 | 10 | 16) {
        bail!("MSB4184: radix must be 2, 8, 10, or 16");
    }
    let (negative, digits) = value
        .strip_prefix('-')
        .map_or((false, value), |digits| (true, digits));
    let parsed = i64::from_str_radix(digits, radix as u32)?;
    Ok(if negative { -parsed } else { parsed })
}

fn finite_double(value: f64, member: &str) -> Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        bail!("MSB4184: {member} produced a non-finite numeric value")
    }
}

fn numeric_value(value: &IntrinsicValue) -> Result<f64> {
    match value {
        IntrinsicValue::Int32(value) => Ok(f64::from(*value)),
        IntrinsicValue::Int64(value) => Ok(*value as f64),
        IntrinsicValue::UInt64(value) => Ok(*value as f64),
        IntrinsicValue::Double(value) => Ok(*value),
        _ => bail!("numeric instance member requires a numeric receiver"),
    }
}

fn format_numeric(value: &IntrinsicValue, format: Option<&str>) -> Result<String> {
    let Some(format) = format.filter(|format| !format.is_empty()) else {
        return value.to_msbuild_string();
    };
    let upper_hex = format.starts_with('X');
    let lower_hex = format.starts_with('x');
    if upper_hex || lower_hex {
        let width = format[1..].parse::<usize>().unwrap_or(0);
        let integer = match value {
            IntrinsicValue::Int32(value) => i64::from(*value),
            IntrinsicValue::Int64(value) => *value,
            IntrinsicValue::UInt64(value) => *value as i64,
            _ => bail!("hexadecimal formatting requires an integer receiver"),
        };
        return Ok(if upper_hex {
            format!("{integer:0width$X}")
        } else {
            format!("{integer:0width$x}")
        });
    }
    bail!("MSB4184: Unsupported numeric format '{format}'")
}

fn format_double(value: f64) -> Result<String> {
    finite_double(value, "numeric formatting")?;
    if value.fract() == 0.0 {
        Ok(format!("{value:.0}"))
    } else {
        Ok(value.to_string())
    }
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
        assert_eq!(
            convert.overloads[1].coercions,
            &[Coercion::String, Coercion::Radix]
        );
        let unescape = resolve("MSBuild", "UnEscape", InvocationKind::StaticMethod, 1).unwrap();
        assert_eq!(unescape.argument_rule, ArgumentRule::Escaped);
        assert_eq!(unescape.result_rule, ResultRule::AlreadyEscaped);
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
}
