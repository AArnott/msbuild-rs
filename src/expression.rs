use anyhow::{Context, Result, anyhow, bail};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::object_model::ProjectModel;
use crate::properties::this_file_property;

const MAX_EXPRESSION_NESTING: usize = 128;

pub struct ExpressionEvaluator<'a> {
    model: &'a ProjectModel,
    base_directory: PathBuf,
    current_file: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConditionToken {
    Value(String),
    Equal,
    NotEqual,
    And,
    Or,
    Not,
    LeftParen,
    RightParen,
}

struct ConditionParser {
    tokens: Vec<ConditionToken>,
    position: usize,
}

impl ConditionParser {
    fn new(input: &str) -> Result<Self> {
        Ok(Self {
            tokens: tokenize_condition(input)?,
            position: 0,
        })
    }

    fn parse(mut self) -> Result<bool> {
        if self.tokens.is_empty() {
            return Ok(false);
        }

        let result = self.parse_or()?;
        if let Some(token) = self.peek() {
            bail!("Unexpected token in condition: {token:?}");
        }
        Ok(result)
    }

    fn parse_or(&mut self) -> Result<bool> {
        let mut result = self.parse_and()?;
        while self.consume(&ConditionToken::Or) {
            let right = self.parse_and()?;
            result = result || right;
        }
        Ok(result)
    }

    fn parse_and(&mut self) -> Result<bool> {
        let mut result = self.parse_unary()?;
        while self.consume(&ConditionToken::And) {
            let right = self.parse_unary()?;
            result = result && right;
        }
        Ok(result)
    }

    fn parse_unary(&mut self) -> Result<bool> {
        if self.consume(&ConditionToken::Not) {
            return Ok(!self.parse_unary()?);
        }

        if self.consume(&ConditionToken::LeftParen) {
            let result = self.parse_or()?;
            if !self.consume(&ConditionToken::RightParen) {
                bail!("Missing closing parenthesis in condition");
            }
            return Ok(result);
        }

        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<bool> {
        let left = if matches!(
            self.peek(),
            Some(ConditionToken::Equal | ConditionToken::NotEqual)
        ) {
            String::new()
        } else {
            self.take_value()?
        };
        if self.consume(&ConditionToken::Equal) {
            let right = self.take_value()?;
            return Ok(left.eq_ignore_ascii_case(&right));
        }
        if self.consume(&ConditionToken::NotEqual) {
            let right = self.take_value()?;
            return Ok(!left.eq_ignore_ascii_case(&right));
        }

        match left.trim().to_ascii_lowercase().as_str() {
            "" | "false" => Ok(false),
            "true" => Ok(true),
            _ => bail!("Expected a boolean value or comparison, found '{left}'"),
        }
    }

    fn take_value(&mut self) -> Result<String> {
        match self.tokens.get(self.position).cloned() {
            Some(ConditionToken::Value(value)) => {
                self.position += 1;
                Ok(value)
            }
            Some(token) => bail!("Expected a value in condition, found {token:?}"),
            None => bail!("Expected a value at the end of the condition"),
        }
    }

    fn consume(&mut self, expected: &ConditionToken) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<&ConditionToken> {
        self.tokens.get(self.position)
    }
}

fn tokenize_condition(input: &str) -> Result<Vec<ConditionToken>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut position = 0;

    while position < chars.len() {
        match chars[position] {
            character if character.is_whitespace() => position += 1,
            '(' => {
                tokens.push(ConditionToken::LeftParen);
                position += 1;
            }
            ')' => {
                tokens.push(ConditionToken::RightParen);
                position += 1;
            }
            '=' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(ConditionToken::Equal);
                position += 2;
            }
            '!' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(ConditionToken::NotEqual);
                position += 2;
            }
            '!' => {
                tokens.push(ConditionToken::Not);
                position += 1;
            }
            quote @ ('\'' | '"') => {
                position += 1;
                let start = position;
                while position < chars.len() && chars[position] != quote {
                    position += 1;
                }
                if position == chars.len() {
                    bail!("Unterminated quoted value in condition");
                }
                tokens.push(ConditionToken::Value(
                    chars[start..position].iter().collect(),
                ));
                position += 1;
            }
            _ => {
                let start = position;
                while position < chars.len()
                    && !chars[position].is_whitespace()
                    && !matches!(chars[position], '(' | ')' | '=' | '!')
                {
                    position += 1;
                }
                if start == position {
                    return Err(anyhow!(
                        "Unexpected character '{}' in condition",
                        chars[position]
                    ));
                }
                let value: String = chars[start..position].iter().collect();
                if value.eq_ignore_ascii_case("and") {
                    tokens.push(ConditionToken::And);
                } else if value.eq_ignore_ascii_case("or") {
                    tokens.push(ConditionToken::Or);
                } else {
                    tokens.push(ConditionToken::Value(value));
                }
            }
        }
    }

    Ok(tokens)
}

impl<'a> ExpressionEvaluator<'a> {
    #[allow(dead_code)] // Used by direct object-model consumers and tests.
    pub fn new(model: &'a ProjectModel) -> Self {
        Self {
            model,
            base_directory: model
                .get_project_directory()
                .unwrap_or_else(|| PathBuf::from(".")),
            current_file: None,
        }
    }

    pub fn with_current_file(model: &'a ProjectModel, path: &'a Path) -> Self {
        Self {
            model,
            base_directory: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
            current_file: Some(path),
        }
    }

    /// Evaluate a string that may contain property and item references
    pub fn evaluate(&self, input: &str) -> Result<String> {
        self.evaluate_with_depth(input, 0)
    }

    fn evaluate_with_depth(&self, input: &str, depth: usize) -> Result<String> {
        if depth > MAX_EXPRESSION_NESTING {
            bail!("Expression nesting exceeds the supported limit of {MAX_EXPRESSION_NESTING}");
        }
        self.expand(input, depth)
    }

    /// Evaluate a condition expression
    pub fn evaluate_condition(&self, condition: &str) -> Result<bool> {
        let evaluated = self.evaluate(condition)?;
        let evaluated = self.expand_condition_functions(&evaluated)?;
        ConditionParser::new(&evaluated)?.parse()
    }

    fn expand(&self, input: &str, depth: usize) -> Result<String> {
        let mut output = String::with_capacity(input.len());
        let mut position = 0;

        while let Some(relative_start) = input[position..].find(['$', '@']) {
            let start = position + relative_start;
            output.push_str(&input[position..start]);
            if input.as_bytes().get(start + 1) != Some(&b'(') {
                output.push(input.as_bytes()[start] as char);
                position = start + 1;
                continue;
            }

            let end = find_matching_parenthesis(input, start + 1)?;
            let body = &input[start + 2..end];
            let replacement = if input.as_bytes()[start] == b'$' {
                self.evaluate_property_expression(body, depth)?
            } else {
                self.evaluate_item_expression(body)?
            };
            output.push_str(&replacement);
            position = end + 1;
        }
        output.push_str(&input[position..]);
        Ok(output)
    }

    fn evaluate_property_expression(&self, expression: &str, depth: usize) -> Result<String> {
        if let Some(function) = expression.strip_prefix('[') {
            return self.evaluate_static_function(function, depth);
        }
        if let Some((property, invocation)) = expression.split_once('.')
            && let Some((method, arguments)) = parse_invocation(invocation)
        {
            let value = self.property_value(property).unwrap_or(Cow::Borrowed(""));
            let arguments = split_arguments(arguments)?
                .into_iter()
                .map(|argument| self.evaluate_with_depth(&argument, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            let argument = arguments.first().map_or("", |value| value.as_str());
            if let Some(matched) =
                match_ignore_ascii_case(method, &["Contains", "StartsWith", "EndsWith"])
            {
                return Ok(match matched {
                    "Contains" => value.contains(argument),
                    "StartsWith" => value.starts_with(argument),
                    _ => value.ends_with(argument),
                }
                .to_string());
            }
            if method.eq_ignore_ascii_case("Substring") {
                if !(1..=2).contains(&arguments.len()) {
                    bail!("Substring expects one or two arguments");
                }
                let start = arguments[0].parse::<usize>()?;
                let characters = value.chars().collect::<Vec<_>>();
                let end = if arguments.len() == 2 {
                    start + arguments[1].parse::<usize>()?
                } else {
                    characters.len()
                };
                return characters
                    .get(start..end)
                    .map(|characters| characters.iter().collect())
                    .ok_or_else(|| anyhow!("Substring range {start}..{end} is out of bounds"));
            }
            if method.eq_ignore_ascii_case("ToLower")
                || method.eq_ignore_ascii_case("ToUpper")
                || method.eq_ignore_ascii_case("Trim")
            {
                require_arguments(method, &arguments, 0)?;
                return Ok(if method.eq_ignore_ascii_case("ToLower") {
                    value.to_lowercase()
                } else if method.eq_ignore_ascii_case("ToUpper") {
                    value.to_uppercase()
                } else {
                    value.trim().to_string()
                });
            }
            bail!("Unsupported property method: {method}")
        }
        Ok(self
            .property_value(expression)
            .map(Cow::into_owned)
            .unwrap_or_default())
    }

    fn evaluate_static_function(&self, expression: &str, depth: usize) -> Result<String> {
        let (type_name, invocation) = expression
            .split_once("]::")
            .ok_or_else(|| anyhow!("Malformed property function: $([{expression})"))?;
        let (method, arguments) = parse_invocation(invocation)
            .ok_or_else(|| anyhow!("Malformed property function invocation: {invocation}"))?;
        let arguments = split_arguments(arguments)?
            .into_iter()
            .map(|argument| self.evaluate_with_depth(&argument, depth + 1))
            .collect::<Result<Vec<_>>>()?;

        if type_name.eq_ignore_ascii_case("MSBuild") {
            match method.to_ascii_lowercase().as_str() {
                "arefeaturesenabled" => Ok("true".to_string()),
                "isrunningfromvisualstudio" => {
                    require_arguments(method, &arguments, 0)?;
                    Ok("false".to_string())
                }
                "versiongreaterthan"
                | "versiongreaterthanorequals"
                | "versionlessthan"
                | "versionlessthanorequals"
                | "versionequals" => {
                    require_arguments(method, &arguments, 2)?;
                    let ordering = compare_versions(&arguments[0], &arguments[1]);
                    let result = match method.to_ascii_lowercase().as_str() {
                        "versiongreaterthan" => ordering.is_gt(),
                        "versiongreaterthanorequals" => !ordering.is_lt(),
                        "versionlessthan" => ordering.is_lt(),
                        "versionlessthanorequals" => !ordering.is_gt(),
                        _ => ordering.is_eq(),
                    };
                    Ok(result.to_string())
                }
                "getdirectorynameoffileabove" => {
                    require_arguments(method, &arguments, 2)?;
                    Ok(find_file_above(&arguments[0], &arguments[1])
                        .and_then(|path| path.parent().map(display_path))
                        .unwrap_or_default())
                }
                "getpathoffileabove" => {
                    require_arguments(method, &arguments, 2)?;
                    let file_name = Path::new(&arguments[0])
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    Ok(find_file_above(&arguments[1], &file_name)
                        .map(|path| display_path(&path))
                        .unwrap_or_default())
                }
                "makerelative" => {
                    require_arguments(method, &arguments, 2)?;
                    Ok(make_relative(&arguments[0], &arguments[1]))
                }
                "normalizepath" => {
                    if arguments.is_empty() {
                        bail!("NormalizePath expects at least one argument");
                    }
                    let mut path = PathBuf::from(&arguments[0]);
                    for part in &arguments[1..] {
                        path.push(part);
                    }
                    Ok(display_path(&path.canonicalize().unwrap_or(path)))
                }
                "ensuretrailingslash" => {
                    require_arguments(method, &arguments, 1)?;
                    if arguments[0].ends_with(['/', '\\']) || arguments[0].is_empty() {
                        Ok(arguments[0].clone())
                    } else {
                        Ok(format!("{}{}", arguments[0], std::path::MAIN_SEPARATOR))
                    }
                }
                "add" | "subtract" | "multiply" | "divide" | "modulo" => {
                    require_arguments(method, &arguments, 2)?;
                    let left = arguments[0].parse::<f64>().with_context(|| {
                        format!("{method} left operand '{}' is not numeric", arguments[0])
                    })?;
                    let right = arguments[1].parse::<f64>().with_context(|| {
                        format!("{method} right operand '{}' is not numeric", arguments[1])
                    })?;
                    let value = match method.to_ascii_lowercase().as_str() {
                        "add" => left + right,
                        "subtract" => left - right,
                        "multiply" => left * right,
                        "divide" if right != 0.0 => left / right,
                        "modulo" if right != 0.0 => left % right,
                        "divide" => bail!("Cannot divide by zero"),
                        _ => bail!("Cannot calculate modulo zero"),
                    };
                    Ok(if value.fract() == 0.0 {
                        format!("{value:.0}")
                    } else {
                        value.to_string()
                    })
                }
                _ => bail!("Unsupported MSBuild property function: {method}"),
            }
        } else if type_name.eq_ignore_ascii_case("System.IO.Path") {
            if method.eq_ignore_ascii_case("Combine") {
                require_arguments(method, &arguments, 2)?;
                Ok(display_path(&Path::new(&arguments[0]).join(&arguments[1])))
            } else if method.eq_ignore_ascii_case("IsPathRooted") {
                require_arguments(method, &arguments, 1)?;
                Ok(Path::new(&arguments[0]).is_absolute().to_string())
            } else if method.eq_ignore_ascii_case("GetDirectoryName") {
                require_arguments(method, &arguments, 1)?;
                Ok(Path::new(&arguments[0])
                    .parent()
                    .map(display_path)
                    .unwrap_or_default())
            } else if method.eq_ignore_ascii_case("GetFileName") {
                require_arguments(method, &arguments, 1)?;
                Ok(Path::new(&arguments[0])
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned())
            } else if method.eq_ignore_ascii_case("GetFileNameWithoutExtension") {
                require_arguments(method, &arguments, 1)?;
                Ok(Path::new(&arguments[0])
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned())
            } else if method.eq_ignore_ascii_case("GetExtension") {
                require_arguments(method, &arguments, 1)?;
                Ok(Path::new(&arguments[0])
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
                    .unwrap_or_default())
            } else if method.eq_ignore_ascii_case("GetFullPath") {
                require_arguments(method, &arguments, 1)?;
                let path = PathBuf::from(&arguments[0]);
                Ok(display_path(&path.canonicalize().unwrap_or(path)))
            } else {
                bail!("Unsupported System.IO.Path property function: {method}")
            }
        } else if type_name.eq_ignore_ascii_case("System.Version")
            && method.eq_ignore_ascii_case("Parse")
        {
            require_arguments(method, &arguments, 1)?;
            Ok(arguments[0].clone())
        } else {
            bail!("Unsupported property function: [{type_name}]::{method}")
        }
    }

    fn evaluate_item_expression(&self, expression: &str) -> Result<String> {
        if let Some((item_type, invocation)) = expression.split_once("->") {
            let (method, arguments) = parse_invocation(invocation)
                .ok_or_else(|| anyhow!("Malformed item function: {invocation}"))?;
            let arguments = split_arguments(arguments)?;
            if method.eq_ignore_ascii_case("AnyHaveMetadataValue") {
                require_arguments(method, &arguments, 2)?;
                return Ok(self
                    .model
                    .get_items(item_type)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.metadata.iter().any(|(name, value)| {
                                name.eq_ignore_ascii_case(&arguments[0])
                                    && value.eq_ignore_ascii_case(&arguments[1])
                            })
                        })
                    })
                    .to_string());
            }
            if method.eq_ignore_ascii_case("Distinct") {
                require_arguments(method, &arguments, 0)?;
                let mut seen = std::collections::HashSet::new();
                return Ok(self
                    .model
                    .get_items(item_type)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        seen.insert(item.name.to_ascii_lowercase())
                            .then_some(item.name.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(";"));
            }
            bail!("Unsupported item function: {method}")
        }
        Ok(self.model.get_all_item_names(expression))
    }

    fn expand_condition_functions(&self, input: &str) -> Result<String> {
        let mut output = String::with_capacity(input.len());
        let mut position = 0;
        while position < input.len() {
            let Some((start, name, arguments_start)) = find_condition_function(input, position)
            else {
                output.push_str(&input[position..]);
                break;
            };
            output.push_str(&input[position..start]);
            let end = find_matching_parenthesis(input, arguments_start)?;
            let arguments = split_arguments(&input[arguments_start + 1..end])?;
            let value = if name.eq_ignore_ascii_case("Exists") {
                require_arguments(name, &arguments, 1)?;
                let path = Path::new(&arguments[0]);
                if path.is_absolute() {
                    path.exists()
                } else {
                    self.base_directory.join(path).exists()
                }
            } else if name.eq_ignore_ascii_case("HasTrailingSlash") {
                require_arguments(name, &arguments, 1)?;
                arguments[0].ends_with(['/', '\\'])
            } else {
                bail!("Unsupported condition function: {name}")
            };
            output.push_str(&value.to_string());
            position = end + 1;
        }
        Ok(output)
    }

    fn property_value(&self, name: &str) -> Option<Cow<'a, str>> {
        if let Some(path) = self.current_file
            && let Some(value) = this_file_property(name, path)
        {
            return Some(Cow::Owned(value));
        }
        self.model
            .get_property(name)
            .map(|value| Cow::Borrowed(value.as_str()))
    }
}

fn find_matching_parenthesis(input: &str, opening: usize) -> Result<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, character) in input[opening..].char_indices() {
        let position = opening + offset;
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => {
                depth += 1;
                if depth > MAX_EXPRESSION_NESTING {
                    bail!(
                        "Expression nesting exceeds the supported limit of {MAX_EXPRESSION_NESTING}"
                    );
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(position);
                }
            }
            _ => {}
        }
    }
    bail!("Unterminated expression in '{input}'")
}

fn parse_invocation(input: &str) -> Option<(&str, &str)> {
    let opening = input.find('(')?;
    input
        .ends_with(')')
        .then(|| (&input[..opening], &input[opening + 1..input.len() - 1]))
}

fn split_arguments(input: &str) -> Result<Vec<String>> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut arguments = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut quote = None;
    for (position, character) in input.char_indices() {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => {
                depth += 1;
                if depth > MAX_EXPRESSION_NESTING {
                    bail!(
                        "Expression nesting exceeds the supported limit of {MAX_EXPRESSION_NESTING}"
                    );
                }
            }
            ')' => depth -= 1,
            ',' if depth == 0 => {
                arguments.push(unquote(input[start..position].trim()));
                start = position + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        bail!("Malformed function arguments: {input}")
    }
    arguments.push(unquote(input[start..].trim()));
    Ok(arguments)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn find_condition_function(input: &str, from: usize) -> Option<(usize, &str, usize)> {
    let bytes = input.as_bytes();
    let mut position = from;
    while position < bytes.len() {
        if bytes[position].is_ascii_alphabetic() {
            let start = position;
            while position < bytes.len()
                && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
            {
                position += 1;
            }
            let name = &input[start..position];
            while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            if bytes.get(position) == Some(&b'(')
                && (name.eq_ignore_ascii_case("Exists")
                    || name.eq_ignore_ascii_case("HasTrailingSlash"))
            {
                return Some((start, name, position));
            }
        } else {
            position += 1;
        }
    }
    None
}

fn require_arguments(method: &str, arguments: &[String], count: usize) -> Result<()> {
    if arguments.len() == count {
        Ok(())
    } else {
        bail!(
            "{method} expects {count} argument(s), found {}",
            arguments.len()
        )
    }
}

fn match_ignore_ascii_case<'a>(value: &str, options: &'a [&str]) -> Option<&'a str> {
    options
        .iter()
        .copied()
        .find(|option| value.eq_ignore_ascii_case(option))
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let parse = |value: &str| {
        value
            .split(['.', '-'])
            .take_while(|part| part.chars().all(|character| character.is_ascii_digit()))
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

fn display_path(path: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_model::{Item, ProjectModel};
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_property_substitution() {
        let mut model = ProjectModel::new();
        model.set_property("Configuration".to_string(), "Debug".to_string());
        model.set_property("Platform".to_string(), "x64".to_string());

        let evaluator = ExpressionEvaluator::new(&model);

        let result = evaluator
            .evaluate("bin/$(Configuration)/$(Platform)")
            .unwrap();
        assert_eq!(result, "bin/Debug/x64");
    }

    #[test]
    fn externally_supplied_property_values_are_not_fixed_point_expanded() {
        let mut model = ProjectModel::new();
        model.set_property("Configuration".to_string(), "Debug".to_string());
        model.set_property("OutputPath".to_string(), "bin/$(Configuration)".to_string());

        assert_eq!(
            ExpressionEvaluator::new(&model)
                .evaluate("$(OutputPath)")
                .unwrap(),
            "bin/$(Configuration)"
        );
    }

    #[test]
    fn literal_parentheses_are_opaque_to_expression_depth_guard() {
        let model = ProjectModel::new();
        let input = format!(
            "{}value{}",
            "(".repeat(MAX_EXPRESSION_NESTING * 4),
            ")".repeat(MAX_EXPRESSION_NESTING * 4)
        );
        assert_eq!(
            ExpressionEvaluator::new(&model).evaluate(&input).unwrap(),
            input
        );
    }

    #[test]
    fn rejects_deeply_recursive_function_arguments_even_when_quoted() {
        let model = ProjectModel::new();
        let mut input = "value".to_string();
        for _ in 0..=MAX_EXPRESSION_NESTING {
            input = format!("$([System.IO.Path]::GetFileName('{input}'))");
        }
        let error = ExpressionEvaluator::new(&model)
            .evaluate(&input)
            .unwrap_err();
        assert!(error.to_string().contains("nesting"));
    }

    #[test]
    fn test_item_substitution() {
        let mut model = ProjectModel::new();

        let item1 = Item {
            item_type: "Compile".to_string(),
            name: "file1.cs".to_string(),
            metadata: HashMap::new(),
        };

        let item2 = Item {
            item_type: "Compile".to_string(),
            name: "file2.cs".to_string(),
            metadata: HashMap::new(),
        };

        model.add_item(item1);
        model.add_item(item2);

        let evaluator = ExpressionEvaluator::new(&model);

        let result = evaluator.evaluate("Files: @(Compile)").unwrap();
        assert_eq!(result, "Files: file1.cs;file2.cs");
    }

    #[test]
    fn test_condition_evaluation() {
        let mut model = ProjectModel::new();
        model.set_property("Configuration".to_string(), "Debug".to_string());

        let evaluator = ExpressionEvaluator::new(&model);

        assert!(
            evaluator
                .evaluate_condition("'$(Configuration)' == 'Debug'")
                .unwrap()
        );
        assert!(
            !evaluator
                .evaluate_condition("'$(Configuration)' == 'Release'")
                .unwrap()
        );
        assert!(!evaluator.evaluate_condition("").unwrap());
        assert!(evaluator.evaluate_condition("true").unwrap());
    }

    #[test]
    fn test_complex_condition_precedence_and_parentheses() {
        let mut model = ProjectModel::new();
        model.set_property("Configuration".to_string(), "debug".to_string());
        model.set_property("Platform".to_string(), "x64".to_string());

        let evaluator = ExpressionEvaluator::new(&model);

        assert!(
            evaluator
                .evaluate_condition(
                    "'$(Configuration)' == 'Debug' And ('$(Platform)' == 'AnyCPU' Or '$(Platform)' == 'x64')"
                )
                .unwrap()
        );
        assert!(
            evaluator
                .evaluate_condition("false Or true And true")
                .unwrap()
        );
        assert!(
            !evaluator
                .evaluate_condition("(false Or true) And !true")
                .unwrap()
        );
    }

    #[test]
    fn test_malformed_conditions_are_errors() {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        assert!(evaluator.evaluate_condition("'Debug' ==").is_err());
        assert!(evaluator.evaluate_condition("(true Or false").is_err());
        assert!(evaluator.evaluate_condition("arbitrary text").is_err());
        assert!(evaluator.evaluate_condition("'unterminated").is_err());
    }

    #[test]
    fn test_empty_unquoted_property_in_comparison() {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        assert!(!evaluator.evaluate_condition("$(Missing) != ''").unwrap());
        assert!(evaluator.evaluate_condition("$(Missing) == ''").unwrap());
    }

    #[test]
    fn evaluates_nested_property_functions_without_regex() {
        let mut model = ProjectModel::new();
        model.set_property("SdkVersion".to_string(), "10.0.400-preview.1".to_string());
        let evaluator = ExpressionEvaluator::new(&model);

        assert_eq!(
            evaluator
                .evaluate("$([System.IO.Path]::Combine('root', '$([MSBuild]::MakeRelative(root, root/sub))'))")
                .unwrap(),
            format!("root{}sub", std::path::MAIN_SEPARATOR)
        );
        assert!(
            evaluator
                .evaluate_condition("$([MSBuild]::VersionGreaterThan($(SdkVersion), 8.0)) And $(SdkVersion.Contains('-preview'))")
                .unwrap()
        );
        assert_eq!(
            evaluator
                .evaluate("$([System.IO.Path]::IsPathRooted('C:\\root'))")
                .unwrap(),
            cfg!(windows).to_string()
        );
        assert_eq!(
            evaluator.evaluate("$(SdkVersion.Substring(0, 4))").unwrap(),
            "10.0"
        );
    }

    #[test]
    fn evaluates_directory_build_props_path_functions() -> Result<()> {
        let directory = TempDir::new()?;
        let nested = directory.path().join("src").join("project");
        fs::create_dir_all(&nested)?;
        fs::write(
            directory.path().join("Directory.Build.props"),
            "<Project />",
        )?;
        let mut model = ProjectModel::new();
        model.set_property("ProjectDirectory".to_string(), display_path(&nested));
        model.set_property("PropsFile".to_string(), "Directory.Build.props".to_string());

        let base = ExpressionEvaluator::new(&model).evaluate(
            "$([MSBuild]::GetDirectoryNameOfFileAbove($(ProjectDirectory), '$(PropsFile)'))",
        )?;
        model.set_property("PropsBase".to_string(), base);
        let path = ExpressionEvaluator::new(&model)
            .evaluate("$([System.IO.Path]::Combine('$(PropsBase)', '$(PropsFile)'))")?;
        assert_eq!(
            PathBuf::from(path).canonicalize()?,
            directory
                .path()
                .join("Directory.Build.props")
                .canonicalize()?
        );

        let props_directory = directory.path().join("src");
        let child_props = props_directory.join("Directory.Build.props");
        fs::write(&child_props, "<Project />")?;
        model.set_property("MSBuildThisFile".to_string(), display_path(&child_props));
        model.set_property(
            "MSBuildThisFileDirectory".to_string(),
            format!(
                "{}{}",
                display_path(&props_directory),
                std::path::MAIN_SEPARATOR
            ),
        );
        let parent_path = ExpressionEvaluator::new(&model).evaluate(
            "$([MSBuild]::GetPathOfFileAbove($(MSBuildThisFile), $(MSBuildThisFileDirectory)..))",
        )?;
        assert_eq!(
            PathBuf::from(parent_path).canonicalize()?,
            directory
                .path()
                .join("Directory.Build.props")
                .canonicalize()?
        );
        assert_eq!(
            ExpressionEvaluator::new(&model)
                .evaluate("$([MSBuild]::MakeRelative('C:\\repo\\', 'C:\\repo\\src\\project'))")?,
            r"src\project"
        );
        assert_eq!(
            ExpressionEvaluator::new(&model)
                .evaluate("$([MSBuild]::MakeRelative('C:\\REPO\\', 'c:\\repo\\src\\project'))")?,
            r"src\project"
        );
        Ok(())
    }

    #[test]
    fn evaluates_condition_intrinsics() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("Restore".to_string(), "true".to_string());
        model.set_property(
            "NETCoreSdkVersion".to_string(),
            "10.0.400-preview.1".to_string(),
        );
        let evaluator = ExpressionEvaluator::new(&model);

        assert!(
            evaluator
                .evaluate_condition("HasTrailingSlash('obj\\') And HasTrailingSlash('obj/')")?
        );
        assert!(!evaluator.evaluate_condition("HasTrailingSlash('obj')")?);
        assert!(evaluator.evaluate_condition(
            "$([MSBuild]::AreFeaturesEnabled('17.10')) And '$(Restore)' == 'true'"
        )?);
        assert!(evaluator.evaluate_condition(
            "$([MSBuild]::VersionGreaterThan($(NETCoreSdkVersion), 7.0.100)) And $(NETCoreSdkVersion.Contains('-preview'))"
        )?);
        assert!(
            evaluator.evaluate_condition("$([MSBuild]::VersionGreaterThanOrEquals(8.0, 8.0))")?
        );
        Ok(())
    }
}
