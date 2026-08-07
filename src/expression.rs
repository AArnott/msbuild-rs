use anyhow::{Context, Result, anyhow, bail};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::escaping::{DecodedString, EscapedString, escape, unescape_once};
use crate::object_model::{Item, ProjectModel};
use crate::properties::this_file_property;

const MAX_EXPRESSION_NESTING: usize = 128;

pub struct ExpressionEvaluator<'a> {
    model: &'a ProjectModel,
    base_directory: PathBuf,
    current_file: Option<&'a Path>,
    current_item: Option<&'a Item>,
    current_item_type: Option<&'a str>,
    current_item_identity: Option<Cow<'a, str>>,
    current_item_metadata_cleared: bool,
}

pub(crate) struct EvaluatedItemExpression<'a> {
    pub source: Option<&'a Item>,
    pub escaped_identity: String,
    pub preserve_metadata: bool,
    pub preserve_recursive_dir: bool,
}

enum ItemExpressionEvaluation<'a> {
    Items {
        values: Vec<EvaluatedItemExpression<'a>>,
        separator: String,
        preserves_items: bool,
    },
    Scalar {
        escaped_value: String,
        source: Option<&'a Item>,
        preserve_metadata: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConditionToken {
    Value(String),
    Function(String, Vec<String>),
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    And,
    Or,
    Not,
    LeftParen,
    RightParen,
}

#[derive(Debug)]
enum ConditionExpression {
    Value(String),
    Function(String, Vec<String>),
    Not(Box<Self>),
    Comparison {
        left: Box<Self>,
        operator: ComparisonOperator,
        right: Box<Self>,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, Copy)]
enum ComparisonOperator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
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

    fn parse(mut self) -> Result<ConditionExpression> {
        if self.tokens.is_empty() {
            bail!("A condition must evaluate to a boolean value");
        }

        let result = self.parse_or()?;
        if let Some(token) = self.peek() {
            bail!("Unexpected token in condition: {token:?}");
        }
        Ok(result)
    }

    fn parse_or(&mut self) -> Result<ConditionExpression> {
        let mut result = self.parse_and()?;
        while self.consume(&ConditionToken::Or) {
            result = ConditionExpression::Or(Box::new(result), Box::new(self.parse_and()?));
        }
        Ok(result)
    }

    fn parse_and(&mut self) -> Result<ConditionExpression> {
        let mut result = self.parse_comparison()?;
        while self.consume(&ConditionToken::And) {
            result = ConditionExpression::And(Box::new(result), Box::new(self.parse_comparison()?));
        }
        Ok(result)
    }

    fn parse_factor(&mut self) -> Result<ConditionExpression> {
        if self.consume(&ConditionToken::Not) {
            return Ok(ConditionExpression::Not(Box::new(self.parse_factor()?)));
        }

        if self.consume(&ConditionToken::LeftParen) {
            let result = self.parse_or()?;
            if !self.consume(&ConditionToken::RightParen) {
                bail!("Missing closing parenthesis in condition");
            }
            return Ok(result);
        }

        match self.tokens.get(self.position).cloned() {
            Some(ConditionToken::Value(value)) => {
                self.position += 1;
                Ok(ConditionExpression::Value(value))
            }
            Some(ConditionToken::Function(name, arguments)) => {
                self.position += 1;
                Ok(ConditionExpression::Function(name, arguments))
            }
            Some(token) => bail!("Expected a value in condition, found {token:?}"),
            None => bail!("Expected a value at the end of the condition"),
        }
    }

    fn parse_comparison(&mut self) -> Result<ConditionExpression> {
        let left = self.parse_factor()?;
        let operator = if self.consume(&ConditionToken::Equal) {
            Some(ComparisonOperator::Equal)
        } else if self.consume(&ConditionToken::NotEqual) {
            Some(ComparisonOperator::NotEqual)
        } else if self.consume(&ConditionToken::Less) {
            Some(ComparisonOperator::Less)
        } else if self.consume(&ConditionToken::LessOrEqual) {
            Some(ComparisonOperator::LessOrEqual)
        } else if self.consume(&ConditionToken::Greater) {
            Some(ComparisonOperator::Greater)
        } else if self.consume(&ConditionToken::GreaterOrEqual) {
            Some(ComparisonOperator::GreaterOrEqual)
        } else {
            None
        };
        if let Some(operator) = operator {
            Ok(ConditionExpression::Comparison {
                left: Box::new(left),
                operator,
                right: Box::new(self.parse_factor()?),
            })
        } else {
            Ok(left)
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
            '<' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(ConditionToken::LessOrEqual);
                position += 2;
            }
            '>' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(ConditionToken::GreaterOrEqual);
                position += 2;
            }
            '<' => {
                tokens.push(ConditionToken::Less);
                position += 1;
            }
            '>' => {
                tokens.push(ConditionToken::Greater);
                position += 1;
            }
            '!' => {
                tokens.push(ConditionToken::Not);
                position += 1;
            }
            quote @ ('\'' | '"') => {
                position += 1;
                let mut value = String::new();
                while position < chars.len() {
                    if chars[position] == quote {
                        break;
                    }
                    if matches!(chars[position], '$' | '@') && chars.get(position + 1) == Some(&'(')
                    {
                        let end = find_matching_condition_parenthesis(&chars, position + 1)?;
                        value.extend(chars[position..=end].iter());
                        position = end + 1;
                    } else {
                        value.push(chars[position]);
                        position += 1;
                    }
                }
                if position == chars.len() {
                    bail!("Unterminated quoted value in condition");
                }
                tokens.push(ConditionToken::Value(value));
                position += 1;
            }
            '$' | '@' if chars.get(position + 1) == Some(&'(') => {
                let end = find_matching_condition_parenthesis(&chars, position + 1)?;
                tokens.push(ConditionToken::Value(
                    chars[position..=end].iter().collect(),
                ));
                position = end + 1;
            }
            _ => {
                let start = position;
                while position < chars.len()
                    && !chars[position].is_whitespace()
                    && !matches!(chars[position], '(' | ')' | '=' | '!' | '<' | '>')
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
                let mut invocation_start = position;
                while chars
                    .get(invocation_start)
                    .is_some_and(|character| character.is_whitespace())
                {
                    invocation_start += 1;
                }
                if value.eq_ignore_ascii_case("and") {
                    tokens.push(ConditionToken::And);
                } else if value.eq_ignore_ascii_case("or") {
                    tokens.push(ConditionToken::Or);
                } else if value
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphabetic())
                    && value
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    && chars.get(invocation_start) == Some(&'(')
                {
                    let end = find_matching_condition_parenthesis(&chars, invocation_start)?;
                    let arguments: String = chars[invocation_start + 1..end].iter().collect();
                    tokens.push(ConditionToken::Function(
                        value,
                        split_arguments(&arguments)?,
                    ));
                    position = end + 1;
                } else {
                    tokens.push(ConditionToken::Value(value));
                }
            }
        }
    }

    Ok(tokens)
}

fn find_matching_condition_parenthesis(chars: &[char], opening: usize) -> Result<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    for (position, &character) in chars.iter().enumerate().skip(opening) {
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
    bail!("Unterminated expression in condition")
}

#[derive(Debug)]
struct VersionValue {
    parts: [i32; 4],
}

fn compare_equality(left: &str, right: &str) -> Result<bool> {
    if let (Some(left), Some(right)) = (parse_numeric(left), parse_numeric(right)) {
        return Ok(left == right);
    }
    if let (Some(left), Some(right)) = (parse_condition_bool(left), parse_condition_bool(right)) {
        return Ok(left == right);
    }
    Ok(left.eq_ignore_ascii_case(right))
}

fn compare_relational(left: &str, right: &str) -> Result<Ordering> {
    let left_numeric = parse_numeric(left);
    let right_numeric = parse_numeric(right);
    let left_version = parse_version(left);
    let right_version = parse_version(right);

    match (left_numeric, left_version, right_numeric, right_version) {
        (Some(left), _, Some(right), _) => Ok(left.partial_cmp(&right).unwrap()),
        (_, Some(left), _, Some(right)) => Ok(compare_versions_exact(&left, &right)),
        (Some(left), _, _, Some(right)) => Ok(compare_number_and_version(&left, &right)),
        (_, Some(left), Some(right), _) => Ok(compare_number_and_version(&right, &left).reverse()),
        _ => bail!(
            "Relational comparison requires decimal, hexadecimal, or version operands: '{left}' and '{right}'"
        ),
    }
}

fn parse_condition_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("!false")
        || value.eq_ignore_ascii_case("!off")
        || value.eq_ignore_ascii_case("!no")
    {
        Some(true)
    } else if value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("!true")
        || value.eq_ignore_ascii_case("!on")
        || value.eq_ignore_ascii_case("!yes")
    {
        Some(false)
    } else {
        None
    }
}

fn parse_numeric(value: &str) -> Option<f64> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        return u32::from_str_radix(hex, 16)
            .ok()
            .map(|value| value as i32 as f64);
    }

    let unsigned = value
        .strip_prefix(['+', '-'])
        .filter(|unsigned| !unsigned.is_empty())
        .unwrap_or(value);
    let mut decimal_seen = false;
    let mut digits = 0usize;
    for byte in unsigned.bytes() {
        if byte == b'.' && !decimal_seen {
            decimal_seen = true;
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        digits += 1;
    }
    (digits != 0)
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .filter(|value| value.is_finite())
}

fn parse_version(value: &str) -> Option<VersionValue> {
    let value = value.trim();
    let mut parts = [-1; 4];
    let mut count = 0usize;
    for part in value.split('.') {
        if count == parts.len()
            || part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        parts[count] = part.parse().ok()?;
        count += 1;
    }
    (2..=4).contains(&count).then_some(VersionValue { parts })
}

fn compare_versions_exact(left: &VersionValue, right: &VersionValue) -> Ordering {
    left.parts.cmp(&right.parts)
}

fn compare_number_and_version(number: &f64, version: &VersionValue) -> Ordering {
    match number.partial_cmp(&(version.parts[0] as f64)).unwrap() {
        Ordering::Equal => Ordering::Less,
        ordering => ordering,
    }
}

enum ConditionOperand {
    Text(String),
    Boolean(bool),
}

impl ConditionExpression {
    fn evaluate_bool(&self, evaluator: &ExpressionEvaluator<'_>) -> Result<bool> {
        match self {
            Self::Value(value) => {
                let value = unescape_once(&evaluator.evaluate(value)?);
                parse_condition_bool(&value).ok_or_else(|| {
                    anyhow!("Expected a boolean value or comparison, found '{value}'")
                })
            }
            Self::Function(name, arguments) => {
                evaluator.evaluate_condition_function(name, arguments)
            }
            Self::Not(expression) => Ok(!expression.evaluate_bool(evaluator)?),
            Self::And(left, right) => {
                if !left.evaluate_bool(evaluator)? {
                    Ok(false)
                } else {
                    right.evaluate_bool(evaluator)
                }
            }
            Self::Or(left, right) => {
                if left.evaluate_bool(evaluator)? {
                    Ok(true)
                } else {
                    right.evaluate_bool(evaluator)
                }
            }
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.evaluate_operand(evaluator)?;
                let right = right.evaluate_operand(evaluator)?;
                match operator {
                    ComparisonOperator::Equal | ComparisonOperator::NotEqual => {
                        let equal = compare_condition_operands(left, right)?;
                        Ok(if matches!(operator, ComparisonOperator::Equal) {
                            equal
                        } else {
                            !equal
                        })
                    }
                    _ => {
                        let (ConditionOperand::Text(left), ConditionOperand::Text(right)) =
                            (left, right)
                        else {
                            bail!("Relational comparison requires value operands");
                        };
                        let ordering = compare_relational(&left, &right)?;
                        Ok(match operator {
                            ComparisonOperator::Less => ordering.is_lt(),
                            ComparisonOperator::LessOrEqual => !ordering.is_gt(),
                            ComparisonOperator::Greater => ordering.is_gt(),
                            ComparisonOperator::GreaterOrEqual => !ordering.is_lt(),
                            _ => unreachable!(),
                        })
                    }
                }
            }
        }
    }

    fn evaluate_operand(&self, evaluator: &ExpressionEvaluator<'_>) -> Result<ConditionOperand> {
        match self {
            Self::Value(value) => Ok(ConditionOperand::Text(unescape_once(
                &evaluator.evaluate(value)?,
            ))),
            _ => Ok(ConditionOperand::Boolean(self.evaluate_bool(evaluator)?)),
        }
    }
}

fn compare_condition_operands(left: ConditionOperand, right: ConditionOperand) -> Result<bool> {
    match (left, right) {
        (ConditionOperand::Text(left), ConditionOperand::Text(right)) => {
            compare_equality(&left, &right)
        }
        (ConditionOperand::Boolean(left), ConditionOperand::Boolean(right)) => Ok(left == right),
        (ConditionOperand::Boolean(left), ConditionOperand::Text(right))
        | (ConditionOperand::Text(right), ConditionOperand::Boolean(left)) => {
            parse_condition_bool(&right)
                .map(|right| left == right)
                .ok_or_else(|| anyhow!("Cannot compare a boolean with '{right}'"))
        }
    }
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
            current_item: None,
            current_item_type: None,
            current_item_identity: None,
            current_item_metadata_cleared: false,
        }
    }

    pub fn with_current_file(model: &'a ProjectModel, path: &'a Path) -> Self {
        Self {
            model,
            base_directory: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
            current_file: Some(path),
            current_item: None,
            current_item_type: None,
            current_item_identity: None,
            current_item_metadata_cleared: false,
        }
    }

    pub fn with_item(model: &'a ProjectModel, path: &'a Path, item: &'a Item) -> Self {
        Self {
            model,
            base_directory: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
            current_file: Some(path),
            current_item: Some(item),
            current_item_type: Some(&item.item_type),
            current_item_identity: Some(Cow::Borrowed(&item.escaped_name)),
            current_item_metadata_cleared: false,
        }
    }

    fn with_item_expression(
        model: &'a ProjectModel,
        path: &'a Path,
        item: &'a Item,
        escaped_identity: &str,
        metadata_cleared: bool,
    ) -> Self {
        Self {
            model,
            base_directory: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
            current_file: Some(path),
            current_item: Some(item),
            current_item_type: Some(&item.item_type),
            current_item_identity: Some(Cow::Owned(escaped_identity.to_string())),
            current_item_metadata_cleared: metadata_cleared,
        }
    }

    pub fn with_item_definition(
        model: &'a ProjectModel,
        path: &'a Path,
        item_type: &'a str,
    ) -> Self {
        Self {
            model,
            base_directory: path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
            current_file: Some(path),
            current_item: None,
            current_item_type: Some(item_type),
            current_item_identity: None,
            current_item_metadata_cleared: false,
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
        ConditionParser::new(condition)?
            .parse()?
            .evaluate_bool(self)
    }

    fn expand(&self, input: &str, depth: usize) -> Result<String> {
        let mut output = String::with_capacity(input.len());
        let mut position = 0;

        while let Some(relative_start) = input[position..].find(['$', '@', '%']) {
            let start = position + relative_start;
            output.push_str(&input[position..start]);
            if input.as_bytes().get(start + 1) != Some(&b'(') {
                output.push(input.as_bytes()[start] as char);
                position = start + 1;
                continue;
            }

            let end = find_matching_parenthesis(input, start + 1)?;
            let body = &input[start + 2..end];
            let replacement = match input.as_bytes()[start] {
                b'$' => self.evaluate_property_expression(body, depth)?,
                b'@' => self.evaluate_item_expression(body)?,
                b'%' => self.evaluate_metadata_expression(body),
                _ => unreachable!(),
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
            let value = EscapedString::new(
                self.property_value(property)
                    .unwrap_or(Cow::Borrowed(""))
                    .into_owned(),
            )
            .decode();
            let arguments = split_arguments(arguments)?
                .into_iter()
                .map(|argument| self.evaluate_with_depth(&argument, depth + 1))
                .map(|argument| {
                    argument.map(|value| EscapedString::new(value).decode().into_string())
                })
                .collect::<Result<Vec<_>>>()?;
            let argument = arguments.first().map_or("", |value| value.as_str());
            if let Some(matched) =
                match_ignore_ascii_case(method, &["Contains", "StartsWith", "EndsWith"])
            {
                let result = match matched {
                    "Contains" => value.as_str().contains(argument),
                    "StartsWith" => value.as_str().starts_with(argument),
                    _ => value.as_str().ends_with(argument),
                };
                return Ok(DecodedString::new(dotnet_bool(result))
                    .into_escaped()
                    .into_string());
            }
            if method.eq_ignore_ascii_case("Substring") {
                if !(1..=2).contains(&arguments.len()) {
                    bail!("Substring expects one or two arguments");
                }
                let start = arguments[0].parse::<usize>()?;
                let characters = value.as_str().chars().collect::<Vec<_>>();
                let end = if arguments.len() == 2 {
                    start + arguments[1].parse::<usize>()?
                } else {
                    characters.len()
                };
                let result = characters
                    .get(start..end)
                    .map(|characters| characters.iter().collect())
                    .ok_or_else(|| anyhow!("Substring range {start}..{end} is out of bounds"))?;
                return Ok(DecodedString::new(result).into_escaped().into_string());
            }
            if method.eq_ignore_ascii_case("ToLower")
                || method.eq_ignore_ascii_case("ToUpper")
                || method.eq_ignore_ascii_case("Trim")
            {
                require_arguments(method, &arguments, 0)?;
                let result = if method.eq_ignore_ascii_case("ToLower") {
                    value.as_str().to_lowercase()
                } else if method.eq_ignore_ascii_case("ToUpper") {
                    value.as_str().to_uppercase()
                } else {
                    value.as_str().trim().to_string()
                };
                return Ok(DecodedString::new(result).into_escaped().into_string());
            }
            bail!("Unsupported property method: {method}")
        }
        Ok(self
            .property_value(expression)
            .map(Cow::into_owned)
            .unwrap_or_default())
    }

    fn evaluate_metadata_expression(&self, expression: &str) -> String {
        let (qualifier, name) = expression
            .split_once('.')
            .map_or((None, expression), |(qualifier, name)| {
                (Some(qualifier), name)
            });
        if let Some(qualifier) = qualifier
            && self
                .current_item_type
                .is_none_or(|item_type| !qualifier.eq_ignore_ascii_case(item_type))
        {
            return String::new();
        }
        if let Some(item) = self.current_item {
            let escaped_identity = self
                .current_item_identity
                .as_deref()
                .unwrap_or(item.escaped_name.as_str());
            return item
                .get_metadata_for_identity_escaped(
                    name,
                    escaped_identity,
                    self.current_item_metadata_cleared,
                )
                .map(Cow::into_owned)
                .unwrap_or_default();
        }
        self.current_item_type
            .and_then(|item_type| {
                self.model
                    .get_item_definition_metadata_escaped(item_type, name)
            })
            .unwrap_or_default()
            .to_string()
    }

    fn evaluate_static_function(&self, expression: &str, depth: usize) -> Result<String> {
        Ok(
            DecodedString::new(self.evaluate_static_function_decoded(expression, depth)?)
                .into_escaped()
                .into_string(),
        )
    }

    fn evaluate_static_function_decoded(&self, expression: &str, depth: usize) -> Result<String> {
        let (type_name, invocation) = expression
            .split_once("]::")
            .ok_or_else(|| anyhow!("Malformed property function: $([{expression})"))?;
        let (method, arguments) = parse_invocation(invocation)
            .ok_or_else(|| anyhow!("Malformed property function invocation: {invocation}"))?;
        let arguments = split_arguments(arguments)?
            .into_iter()
            .map(|argument| self.evaluate_with_depth(&argument, depth + 1))
            .map(|argument| argument.map(|value| EscapedString::new(value).decode().into_string()))
            .collect::<Result<Vec<_>>>()?;

        if type_name.eq_ignore_ascii_case("MSBuild") {
            match method.to_ascii_lowercase().as_str() {
                "arefeaturesenabled" => Ok("True".to_string()),
                "isrunningfromvisualstudio" => {
                    require_arguments(method, &arguments, 0)?;
                    Ok("False".to_string())
                }
                "isosplatform" => {
                    require_arguments(method, &arguments, 1)?;
                    Ok(dotnet_bool(is_os_platform(&arguments[0])?))
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
                    Ok(dotnet_bool(result))
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
                Ok(dotnet_bool(Path::new(&arguments[0]).is_absolute()))
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

    pub(crate) fn evaluate_item_expression_items(
        &self,
        input: &str,
    ) -> Result<Option<Vec<EvaluatedItemExpression<'a>>>> {
        let input = input.trim();
        if !input.starts_with("@(") {
            return Ok(None);
        }
        let end = find_matching_parenthesis(input, 1)?;
        if end + 1 != input.len() {
            return Ok(None);
        }
        match self.evaluate_item_pipeline(&input[2..end])? {
            ItemExpressionEvaluation::Items {
                values,
                preserves_items: true,
                ..
            } => Ok(Some(values)),
            ItemExpressionEvaluation::Scalar {
                escaped_value,
                source,
                preserve_metadata,
            } => Ok(Some(vec![EvaluatedItemExpression {
                source,
                escaped_identity: escaped_value,
                preserve_metadata,
                preserve_recursive_dir: false,
            }])),
            ItemExpressionEvaluation::Items {
                values,
                separator,
                preserves_items: false,
            } => {
                let scalar = values
                    .into_iter()
                    .map(|value| value.escaped_identity)
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Some(
                    crate::escaping::tokenize_list(&scalar)?
                        .into_iter()
                        .map(|identity| EvaluatedItemExpression {
                            source: None,
                            escaped_identity: identity.to_string(),
                            preserve_metadata: false,
                            preserve_recursive_dir: false,
                        })
                        .collect(),
                ))
            }
        }
    }

    fn evaluate_item_expression(&self, expression: &str) -> Result<String> {
        match self.evaluate_item_pipeline(expression)? {
            ItemExpressionEvaluation::Items {
                values, separator, ..
            } => Ok(values
                .into_iter()
                .map(|value| value.escaped_identity)
                .collect::<Vec<_>>()
                .join(&separator)),
            ItemExpressionEvaluation::Scalar { escaped_value, .. } => Ok(escaped_value),
        }
    }

    fn evaluate_item_pipeline(&self, expression: &str) -> Result<ItemExpressionEvaluation<'a>> {
        let (pipeline, separator) = split_item_separator(expression)?;
        let stages = split_item_pipeline(pipeline)?;
        let item_type = stages
            .first()
            .map(|stage| stage.trim())
            .filter(|item_type| !item_type.is_empty())
            .ok_or_else(|| anyhow!("Item expression has no item type"))?;
        let preserves_items = separator.is_none();
        let separator = separator
            .map(|value| self.evaluate(&unquote(value.trim())))
            .transpose()?
            .unwrap_or_else(|| ";".to_string());

        let mut values = self
            .model
            .get_items(item_type)
            .into_iter()
            .flatten()
            .map(|item| EvaluatedItemExpression {
                source: Some(item),
                escaped_identity: item.escaped_name.clone(),
                preserve_metadata: true,
                preserve_recursive_dir: true,
            })
            .collect::<Vec<_>>();
        for (stage_index, stage) in stages[1..].iter().enumerate() {
            let stage = stage.trim();
            if (stage.starts_with('\'') && stage.ends_with('\''))
                || (stage.starts_with('"') && stage.ends_with('"'))
            {
                let template = unquote(stage);
                for value in &mut values {
                    let source = value
                        .source
                        .expect("item pipelines retain source provenance");
                    let evaluated = Self::with_item_expression(
                        self.model,
                        self.current_file_path(),
                        source,
                        &value.escaped_identity,
                        !value.preserve_metadata,
                    )
                    .evaluate(&template)?;
                    value.escaped_identity = EscapedString::new(evaluated)
                        .decode()
                        .into_escaped()
                        .into_string();
                    value.preserve_recursive_dir = false;
                }
                values.retain(|value| !value.escaped_identity.is_empty());
                continue;
            }

            let (method, arguments) = parse_invocation(stage)
                .ok_or_else(|| anyhow!("Malformed item function: {stage}"))?;
            let arguments = split_arguments(arguments)?
                .into_iter()
                .map(|argument| self.evaluate(&argument).map(|value| unescape_once(&value)))
                .collect::<Result<Vec<_>>>()?;
            let is_last_stage = stage_index + 1 == stages.len() - 1;
            if method.eq_ignore_ascii_case("Metadata") {
                require_arguments(method, &arguments, 1)?;
                let mut transformed = Vec::new();
                for value in values {
                    let source = value
                        .source
                        .expect("item pipelines retain source provenance");
                    let metadata = source
                        .get_metadata_for_identity_escaped(
                            &arguments[0],
                            &value.escaped_identity,
                            !value.preserve_metadata,
                        )
                        .map(Cow::into_owned)
                        .unwrap_or_default();
                    for identity in crate::escaping::tokenize_list(&metadata)? {
                        transformed.push(EvaluatedItemExpression {
                            source: value.source,
                            escaped_identity: identity.to_string(),
                            preserve_metadata: value.preserve_metadata,
                            preserve_recursive_dir: false,
                        });
                    }
                }
                values = transformed;
            } else if matches_ignore_ascii_case(method, ITEM_SPEC_MODIFIERS) {
                require_arguments(method, &arguments, 0)?;
                for value in &mut values {
                    let source = value
                        .source
                        .expect("item pipelines retain source provenance");
                    value.escaped_identity = source
                        .get_metadata_for_identity_escaped(
                            method,
                            &value.escaped_identity,
                            !value.preserve_metadata,
                        )
                        .map(Cow::into_owned)
                        .unwrap_or_default();
                    value.preserve_recursive_dir = false;
                }
                values.retain(|value| !value.escaped_identity.is_empty());
            } else if method.eq_ignore_ascii_case("Distinct") {
                require_arguments(method, &arguments, 0)?;
                let mut seen = std::collections::HashSet::new();
                values.retain(|value| {
                    seen.insert(unescape_once(&value.escaped_identity).to_lowercase())
                });
            } else if method.eq_ignore_ascii_case("DistinctWithCase") {
                require_arguments(method, &arguments, 0)?;
                let mut seen = std::collections::HashSet::new();
                values.retain(|value| seen.insert(unescape_once(&value.escaped_identity)));
            } else if method.eq_ignore_ascii_case("Reverse") {
                require_arguments(method, &arguments, 0)?;
                values.reverse();
            } else if method.eq_ignore_ascii_case("Count") {
                require_arguments(method, &arguments, 0)?;
                if !is_last_stage {
                    bail!("Count must be the last item function in a pipeline");
                }
                return Ok(ItemExpressionEvaluation::Scalar {
                    escaped_value: values.len().to_string(),
                    source: None,
                    preserve_metadata: false,
                });
            } else if method.eq_ignore_ascii_case("AnyHaveMetadataValue") {
                require_arguments(method, &arguments, 2)?;
                if !is_last_stage {
                    bail!("AnyHaveMetadataValue must be the last item function in a pipeline");
                }
                let source_value = values.iter().find(|value| {
                    item_expression_metadata(value, &arguments[0])
                        .map_or(arguments[1].is_empty(), |metadata| {
                            ordinal_ignore_case(&metadata, &arguments[1])
                        })
                });
                return Ok(ItemExpressionEvaluation::Scalar {
                    escaped_value: if source_value.is_some() {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                    source: source_value.and_then(|value| value.source),
                    preserve_metadata: source_value.is_some_and(|value| value.preserve_metadata),
                });
            } else if method.eq_ignore_ascii_case("HasMetadata") {
                require_arguments(method, &arguments, 1)?;
                values.retain(|value| {
                    item_expression_metadata(value, &arguments[0])
                        .is_some_and(|metadata| !metadata.is_empty())
                });
            } else if method.eq_ignore_ascii_case("WithMetadataValue")
                || method.eq_ignore_ascii_case("WithoutMetadataValue")
            {
                require_arguments(method, &arguments, 2)?;
                let retain_matches = method.eq_ignore_ascii_case("WithMetadataValue");
                values.retain(|value| {
                    let matches = item_expression_metadata(value, &arguments[0])
                        .map_or(arguments[1].is_empty(), |metadata| {
                            ordinal_ignore_case(&metadata, &arguments[1])
                        });
                    matches == retain_matches
                });
            } else if method.eq_ignore_ascii_case("ClearMetadata") {
                require_arguments(method, &arguments, 0)?;
                for value in &mut values {
                    value.preserve_metadata = false;
                    value.preserve_recursive_dir = false;
                }
            } else if method.eq_ignore_ascii_case("Exists") {
                require_arguments(method, &arguments, 0)?;
                values.retain(|value| {
                    value
                        .source
                        .is_some_and(|source| source.identity_exists(&value.escaped_identity))
                });
            } else if method.eq_ignore_ascii_case("DirectoryName") {
                require_arguments(method, &arguments, 0)?;
                for value in &mut values {
                    let source = value
                        .source
                        .expect("item pipelines retain source provenance");
                    value.escaped_identity = source.directory_name(&value.escaped_identity);
                    value.preserve_recursive_dir = false;
                }
                values.retain(|value| !value.escaped_identity.is_empty());
            } else if method.eq_ignore_ascii_case("Combine") {
                require_arguments(method, &arguments, 1)?;
                for value in &mut values {
                    let combined =
                        Path::new(&unescape_once(&value.escaped_identity)).join(&arguments[0]);
                    value.escaped_identity = escape(&display_path(&combined));
                    value.preserve_metadata = false;
                    value.preserve_recursive_dir = false;
                }
            } else {
                bail!("Unsupported item function: {method}");
            }
        }
        Ok(ItemExpressionEvaluation::Items {
            values,
            separator,
            preserves_items,
        })
    }

    fn evaluate_condition_function(&self, name: &str, arguments: &[String]) -> Result<bool> {
        let arguments = arguments
            .iter()
            .map(|argument| self.evaluate(argument).map(|value| unescape_once(&value)))
            .collect::<Result<Vec<_>>>()?;
        if name.eq_ignore_ascii_case("Exists") {
            require_arguments(name, &arguments, 1)?;
            let path = Path::new(&arguments[0]);
            Ok(if path.is_absolute() {
                path.exists()
            } else {
                self.base_directory.join(path).exists()
            })
        } else if name.eq_ignore_ascii_case("HasTrailingSlash") {
            require_arguments(name, &arguments, 1)?;
            Ok(arguments[0].ends_with(['/', '\\']))
        } else {
            bail!("Unsupported condition function: {name}")
        }
    }

    fn property_value(&self, name: &str) -> Option<Cow<'a, str>> {
        if let Some(path) = self.current_file
            && let Some(value) = this_file_property(name, path)
        {
            return Some(Cow::Owned(value));
        }
        self.model.get_property_escaped(name).map(Cow::Borrowed)
    }

    fn current_file_path(&self) -> &'a Path {
        self.current_file.unwrap_or_else(|| Path::new(""))
    }
}

const ITEM_SPEC_MODIFIERS: &[&str] = &[
    "Identity",
    "FullPath",
    "RootDir",
    "Filename",
    "Extension",
    "RelativeDir",
    "Directory",
    "RecursiveDir",
    "DefiningProjectFullPath",
    "DefiningProjectDirectory",
    "DefiningProjectName",
    "DefiningProjectExtension",
];

fn item_expression_metadata<'a>(
    value: &'a EvaluatedItemExpression<'a>,
    name: &str,
) -> Option<Cow<'a, str>> {
    let source = value.source?;
    source.get_metadata_for_identity(
        name,
        &unescape_once(&value.escaped_identity),
        !value.preserve_metadata,
    )
}

fn ordinal_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
        || ((!left.is_ascii() || !right.is_ascii()) && left.to_lowercase() == right.to_lowercase())
}

fn is_os_platform(platform: &str) -> Result<bool> {
    if platform.is_empty() {
        bail!("IsOSPlatform platform cannot be empty");
    }
    let current_platform = match std::env::consts::OS {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "OSX",
        _ => return Ok(false),
    };
    Ok(platform.eq_ignore_ascii_case(current_platform))
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

fn split_item_separator(input: &str) -> Result<(&str, Option<&str>)> {
    let mut depth = 0usize;
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
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            ',' if depth == 0 => return Ok((&input[..position], Some(&input[position + 1..]))),
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        bail!("Malformed item expression: {input}");
    }
    Ok((input, None))
}

fn split_item_pipeline(input: &str) -> Result<Vec<&str>> {
    let mut stages = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut start = 0;
    let bytes = input.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        let character = input[position..]
            .chars()
            .next()
            .expect("position must be a character boundary");
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            position += character.len_utf8();
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            '-' if depth == 0 && bytes.get(position + 1) == Some(&b'>') => {
                stages.push(&input[start..position]);
                position += 2;
                start = position;
                continue;
            }
            _ => {}
        }
        position += character.len_utf8();
    }
    if quote.is_some() || depth != 0 {
        bail!("Malformed item expression: {input}");
    }
    stages.push(&input[start..]);
    Ok(stages)
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

fn matches_ignore_ascii_case(value: &str, options: &[&str]) -> bool {
    options
        .iter()
        .any(|option| value.eq_ignore_ascii_case(option))
}

fn dotnet_bool(value: bool) -> String {
    if value { "True" } else { "False" }.to_string()
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
    use crate::object_model::{Item, MetadataMap, ProjectModel};
    use std::fs;
    use std::sync::Arc;
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

        let item1 = Item::new(
            "Compile".to_string(),
            "file1.cs".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("."),
            PathBuf::from("project.proj"),
        );

        let item2 = Item::new(
            "Compile".to_string(),
            "file2.cs".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("."),
            PathBuf::from("project.proj"),
        );

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
        assert!(evaluator.evaluate_condition("").is_err());
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
    fn upstream_relational_and_condition_expression_subset() {
        let mut model = ProjectModel::new();
        model.set_property("a".to_string(), "no".to_string());
        model.set_property("b".to_string(), "true".to_string());
        model.set_property("c".to_string(), "1".to_string());
        model.set_property("d".to_string(), "xxx".to_string());
        let evaluator = ExpressionEvaluator::new(&model);

        // ExpressionTree_Tests.RelationalTests.
        for expression in [
            "1234 < 1235",
            "1234 <= 1235",
            "1234 <= 1234",
            "1235 > 1234",
            "1235 >= 1235",
            "1235 >= 1234",
            "0.0==0",
        ] {
            assert!(
                evaluator.evaluate_condition(expression).unwrap(),
                "expected true: {expression}"
            );
        }
        for expression in ["1235 < 1235", "1235 <= 1234"] {
            assert!(
                !evaluator.evaluate_condition(expression).unwrap(),
                "expected false: {expression}"
            );
        }

        // Selected ExpressionTreeExpression_Tests true/false cases.
        for expression in [
            "0x1==1.0",
            "0<0.1",
            "+4>-4",
            "false==no",
            "true==yes",
            "true==!false",
            "$(c)>0",
            "1.2.3<=1.2.3.0",
            "0.8.0.0<8.0.0",
            "8.0.0>=8",
            "6<=6.0.0.1",
            "true or (SHOULDNOTEVALTHIS)",
            "false or true And true",
        ] {
            assert!(
                evaluator.evaluate_condition(expression).unwrap(),
                "expected true: {expression}"
            );
        }
        for expression in [
            "1.3.5.8>1.3.6.8",
            "0.8.0.0>=1.0",
            "8.0.0<=8.0",
            "1.2.0==1.2",
        ] {
            assert!(
                !evaluator.evaluate_condition(expression).unwrap(),
                "expected false: {expression}"
            );
        }
        let too_large_for_msbuild_numeric_coercion = format!("1{}", "0".repeat(500));
        assert!(
            evaluator
                .evaluate_condition(&format!(
                    "{too_large_for_msbuild_numeric_coercion}=={too_large_for_msbuild_numeric_coercion}"
                ))
                .unwrap()
        );
        assert!(
            !evaluator
                .evaluate_condition(&format!(
                    "{too_large_for_msbuild_numeric_coercion}==0{too_large_for_msbuild_numeric_coercion}"
                ))
                .unwrap()
        );

        // Selected ExpressionTreeExpression_Tests error cases.
        for expression in ["1 > 'x'", "x1<=1", "1<=x", "1<=1<=1", "1>=$(b)"] {
            assert!(
                evaluator.evaluate_condition(expression).is_err(),
                "expected error: {expression}"
            );
        }
    }

    #[test]
    fn numeric_coercion_matches_msbuild_double_and_int32_hex_behavior() {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        for expression in [
            "9007199254740992 == 9007199254740993",
            "0.0 == -0",
            "-0 >= 0",
            "+4.25 > -4.25",
            "0x7FFFFFFF == 2147483647",
            "0x80000000 == -2147483648",
            "0xFFFFFFFF == -1",
            "1.1 > 1.0.0",
            "1.0.0 < 1.1",
            "' 1.2 ' < 2.0",
        ] {
            assert!(
                evaluator.evaluate_condition(expression).unwrap(),
                "expected true: {expression}"
            );
        }

        assert!(
            !evaluator
                .evaluate_condition("0x100000000 == 4294967296")
                .unwrap()
        );
        assert!(!evaluator.evaluate_condition("-0 < 0").unwrap());
        assert!(evaluator.evaluate_condition("0x100000000 > 0").is_err());
        let overflowing_decimal = format!("1{}", "0".repeat(309));
        assert!(
            evaluator
                .evaluate_condition(&format!("{overflowing_decimal} > 0"))
                .is_err()
        );
    }

    #[test]
    fn boolean_aliases_are_exact_and_do_not_trim_or_coerce_empty() {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        for expression in [
            "true",
            "ON",
            "Yes",
            "'!false'",
            "'!OFF'",
            "'!No'",
            "!false",
            "!off",
            "!no",
            "true == on",
            "yes == !false",
        ] {
            assert!(
                evaluator.evaluate_condition(expression).unwrap(),
                "expected true: {expression}"
            );
        }
        for expression in [
            "false", "OFF", "No", "'!true'", "'!ON'", "'!Yes'", "!true", "!on", "!yes",
        ] {
            assert!(
                !evaluator.evaluate_condition(expression).unwrap(),
                "expected false: {expression}"
            );
        }

        assert!(!evaluator.evaluate_condition("' true ' == true").unwrap());
        assert!(!evaluator.evaluate_condition("'' == false").unwrap());
        assert!(evaluator.evaluate_condition("' true '").is_err());
        assert!(evaluator.evaluate_condition("''").is_err());
    }

    #[test]
    fn conditions_expand_only_reachable_operands() {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);
        let failing_function = "$([System.Int32]::Parse('not-a-number'))";

        assert!(
            evaluator
                .evaluate_condition(&format!("true Or {failing_function}"))
                .unwrap()
        );
        assert!(
            !evaluator
                .evaluate_condition(&format!("false And {failing_function}"))
                .unwrap()
        );
        assert!(
            evaluator
                .evaluate_condition(&format!("false Or {failing_function}"))
                .is_err()
        );
        assert!(
            evaluator
                .evaluate_condition(&format!("true And {failing_function}"))
                .is_err()
        );

        assert!(
            evaluator
                .evaluate_condition("true Or Exists($([System.Int32]::Parse('not-a-number')))")
                .unwrap()
        );
        assert!(
            evaluator
                .evaluate_condition("'$([System.IO.Path]::GetFileName('a==b.txt'))' == 'a==b.txt'")
                .unwrap()
        );
        assert!(
            evaluator
                .evaluate_condition("HasTrailingSlash('obj(parentheses)/')")
                .unwrap()
        );
    }

    #[test]
    fn is_os_platform_matches_msbuild_platform_names_case_insensitively() -> Result<()> {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        assert_eq!(
            evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform('wInDoWs'))")?,
            cfg!(target_os = "windows")
        );
        assert_eq!(
            evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform('LINUX'))")?,
            cfg!(target_os = "linux")
        );
        assert_eq!(
            evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform('oSx'))")?,
            cfg!(target_os = "macos")
        );
        assert!(!evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform('MacOS'))")?);
        assert!(!evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform('unknown'))")?);
        assert!(!evaluator.evaluate_condition("$([MSBuild]::IsOSPlatform(' '))")?);
        assert!(
            evaluator
                .evaluate_condition("$([MSBuild]::IsOSPlatform(''))")
                .is_err()
        );
        Ok(())
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
            dotnet_bool(cfg!(windows))
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
