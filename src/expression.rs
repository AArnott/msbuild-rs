use anyhow::{Result, anyhow, bail};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::escaping::{DecodedString, EscapedString, escape, unescape_once};
use crate::native_functions::{
    ArgumentRule, IntrinsicArgument, IntrinsicContext, IntrinsicValue, InvocationKind, ResultRule,
    dotnet_ordinal_ignore_case_key, invoke, invoke_item_string_function, is_allowed, resolve,
    resolve_item_string_function,
};
use crate::object_model::{Item, ProjectModel};
use crate::properties::{lexical_absolute, this_file_property};
use crate::registry::{expand_registry_property, missing_registry_prefix};

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

#[derive(Debug, Clone, Copy)]
pub(crate) enum ItemProvenance<'a> {
    SourceRetained(&'a Item),
    MetadataCleared(&'a Item),
    SourceLess,
}

impl<'a> ItemProvenance<'a> {
    fn source(self) -> Option<&'a Item> {
        match self {
            Self::SourceRetained(source) | Self::MetadataCleared(source) => Some(source),
            Self::SourceLess => None,
        }
    }

    fn retained_source(self) -> Option<&'a Item> {
        match self {
            Self::SourceRetained(source) => Some(source),
            Self::MetadataCleared(_) | Self::SourceLess => None,
        }
    }

    fn has_source(self) -> bool {
        !matches!(self, Self::SourceLess)
    }

    fn clear_metadata(self) -> Self {
        match self {
            Self::SourceRetained(source) | Self::MetadataCleared(source) => {
                Self::MetadataCleared(source)
            }
            Self::SourceLess => Self::SourceLess,
        }
    }
}

pub(crate) struct EvaluatedItemExpression<'a> {
    pub provenance: ItemProvenance<'a>,
    pub escaped_identity: String,
}

struct ItemExpressionEvaluation<'a> {
    values: Vec<EvaluatedItemExpression<'a>>,
    separator: String,
}

struct IntrinsicOutcome {
    value: IntrinsicValue,
    result_rule: ResultRule,
}

#[derive(Debug)]
enum ChainOperation<'a> {
    Member {
        name: &'a str,
        arguments: Option<&'a str>,
    },
    Indexer {
        argument: &'a str,
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

    pub(crate) fn evaluate_properties_only(&self, input: &str) -> Result<String> {
        let mut output = String::with_capacity(input.len());
        let mut position = 0;
        while let Some(relative_start) = input[position..].find('$') {
            let start = position + relative_start;
            output.push_str(&input[position..start]);
            if input.as_bytes().get(start + 1) != Some(&b'(') {
                output.push('$');
                position = start + 1;
                continue;
            }
            let end = find_matching_parenthesis(input, start + 1)?;
            output.push_str(&self.evaluate_property_expression(&input[start + 2..end], 0)?);
            position = end + 1;
        }
        output.push_str(&input[position..]);
        Ok(output)
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
                b'@' => self.evaluate_item_expression(body, depth)?,
                b'%' => self.evaluate_metadata_expression(body),
                _ => unreachable!(),
            };
            output.push_str(&replacement);
            position = end + 1;
        }
        output.push_str(&input[position..]);
        Ok(output)
    }

    fn expand_properties_and_metadata(&self, input: &str, depth: usize) -> Result<String> {
        let mut output = String::with_capacity(input.len());
        let mut position = 0;

        while let Some(relative_start) = input[position..].find(['$', '%']) {
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

        if expression
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Registry:"))
        {
            return expand_registry_property(expression);
        }
        if let Some(value) = missing_registry_prefix(expression)? {
            return Ok(value);
        }

        if let Some(chain_start) = expression.find(['.', '[']) {
            let property = &expression[..chain_start];
            let chain = &expression[chain_start..];
            let chain = chain.strip_prefix('.').unwrap_or(chain);
            if !property.is_empty()
                && let Ok(operations) = parse_member_chain(chain)
                && let Some(first) = operations.first()
            {
                let property_exists = self.property_value(property).is_some();
                let first_is_call = matches!(
                    first,
                    ChainOperation::Member {
                        arguments: Some(_),
                        ..
                    } | ChainOperation::Indexer { .. }
                );
                let first_is_allowed = operation_is_allowed("System.String", first);
                if property_exists || first_is_call || first_is_allowed {
                    let receiver = IntrinsicValue::String(
                        EscapedString::new(
                            self.property_value(property)
                                .unwrap_or(Cow::Borrowed(""))
                                .into_owned(),
                        )
                        .decode()
                        .into_string(),
                    );
                    let outcome = self.evaluate_instance_chain(receiver, &operations, depth)?;
                    return render_intrinsic(outcome);
                }
            }
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
        render_intrinsic(self.evaluate_static_function_outcome(expression, depth)?)
    }

    fn evaluate_static_function_outcome(
        &self,
        expression: &str,
        depth: usize,
    ) -> Result<IntrinsicOutcome> {
        let (type_name, chain) = expression
            .split_once("]::")
            .ok_or_else(|| anyhow!("Malformed property function: $([{expression})"))?;
        let operations = parse_member_chain(chain)?;
        let Some(ChainOperation::Member { name, arguments }) = operations.first() else {
            bail!("Malformed property function invocation: [{type_name}]::{chain}");
        };
        let kind = if name.eq_ignore_ascii_case("new") {
            InvocationKind::Constructor
        } else if arguments.is_some() {
            InvocationKind::StaticMethod
        } else {
            InvocationKind::StaticProperty
        };
        let mut outcome =
            self.invoke_intrinsic(type_name, name, kind, None, *arguments, depth + 1)?;
        for operation in &operations[1..] {
            outcome = self.invoke_instance_operation(outcome.value, operation, depth + 1)?;
        }
        Ok(outcome)
    }

    fn evaluate_instance_chain(
        &self,
        mut receiver: IntrinsicValue,
        operations: &[ChainOperation<'_>],
        depth: usize,
    ) -> Result<IntrinsicOutcome> {
        let mut result_rule = ResultRule::Escape;
        for operation in operations {
            let outcome = self.invoke_instance_operation(receiver, operation, depth + 1)?;
            receiver = outcome.value;
            result_rule = outcome.result_rule;
        }
        Ok(IntrinsicOutcome {
            value: receiver,
            result_rule,
        })
    }

    fn evaluate_exact_static_function_outcome(
        &self,
        expression: &str,
        depth: usize,
    ) -> Result<Option<IntrinsicOutcome>> {
        if depth > MAX_EXPRESSION_NESTING || !expression.starts_with("$(") {
            if depth > MAX_EXPRESSION_NESTING {
                bail!("Expression nesting exceeds the supported limit of {MAX_EXPRESSION_NESTING}");
            }
            return Ok(None);
        }
        let end = find_matching_parenthesis(expression, 1)?;
        if end + 1 != expression.len() {
            return Ok(None);
        }
        let Some(function) = expression[2..end].strip_prefix('[') else {
            return Ok(None);
        };
        self.evaluate_static_function_outcome(function, depth)
            .map(Some)
    }

    fn invoke_instance_operation(
        &self,
        receiver: IntrinsicValue,
        operation: &ChainOperation<'_>,
        depth: usize,
    ) -> Result<IntrinsicOutcome> {
        match operation {
            ChainOperation::Member { name, arguments } => {
                let kind = if arguments.is_some() {
                    InvocationKind::InstanceMethod
                } else {
                    InvocationKind::InstanceProperty
                };
                self.invoke_intrinsic(
                    receiver.type_name(),
                    name,
                    kind,
                    Some(&receiver),
                    *arguments,
                    depth + 1,
                )
            }
            ChainOperation::Indexer { argument } => self.invoke_intrinsic(
                receiver.type_name(),
                "Item",
                InvocationKind::Indexer,
                Some(&receiver),
                Some(argument),
                depth + 1,
            ),
        }
    }

    fn invoke_intrinsic(
        &self,
        type_name: &str,
        member: &str,
        kind: InvocationKind,
        receiver: Option<&IntrinsicValue>,
        argument_expression: Option<&str>,
        depth: usize,
    ) -> Result<IntrinsicOutcome> {
        let raw_arguments = argument_expression
            .map(split_raw_arguments)
            .transpose()?
            .unwrap_or_default();
        // Resolve the allowlist entry before expanding any argument. Besides
        // producing a better diagnostic, this guarantees a rejected receiver
        // cannot trigger nested work before it is blocked.
        let descriptor = resolve(type_name, member, kind, raw_arguments.len())?;
        let arguments = raw_arguments
            .into_iter()
            .map(|raw| {
                let raw = raw.trim();
                let quoted = (raw.starts_with('\'') && raw.ends_with('\''))
                    || (raw.starts_with('"') && raw.ends_with('"'));
                if raw.eq_ignore_ascii_case("null") && !quoted {
                    return Ok(IntrinsicArgument {
                        value: IntrinsicValue::Null,
                    });
                }
                let expression = unquote(raw);
                if let Some(outcome) =
                    self.evaluate_exact_static_function_outcome(&expression, depth + 1)?
                {
                    if matches!(&outcome.value, IntrinsicValue::String(_)) {
                        let evaluated = render_intrinsic(outcome)?;
                        let value = match descriptor.argument_rule {
                            ArgumentRule::Decoded => {
                                EscapedString::new(evaluated).decode().into_string()
                            }
                            ArgumentRule::Escaped => evaluated,
                        };
                        return Ok(IntrinsicArgument {
                            value: IntrinsicValue::String(value),
                        });
                    }
                    return Ok(IntrinsicArgument {
                        value: outcome.value,
                    });
                }
                let evaluated = self.evaluate_with_depth(&expression, depth + 1)?;
                let value = match descriptor.argument_rule {
                    ArgumentRule::Decoded => EscapedString::new(evaluated).decode().into_string(),
                    ArgumentRule::Escaped => evaluated,
                };
                Ok(IntrinsicArgument {
                    value: IntrinsicValue::String(value),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let context = IntrinsicContext {
            tools_directory: self
                .model
                .get_property("MSBuildToolsPath")
                .map(String::as_str),
            environment: self.model.environment(),
            disable_features_from_version: self
                .model
                .get_property("MSBuildDisableFeaturesFromVersion")
                .map(String::as_str),
            runtime_type: self
                .model
                .get_property("MSBuildRuntimeType")
                .map(String::as_str),
        };
        Ok(IntrinsicOutcome {
            value: invoke(descriptor, &context, receiver, &arguments)?,
            result_rule: descriptor.result_rule,
        })
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
        Ok(Some(
            self.evaluate_item_pipeline(&input[2..end], 0)?
                .values
                .into_iter()
                .filter(|value| !value.escaped_identity.is_empty())
                .collect(),
        ))
    }

    fn evaluate_item_expression(&self, expression: &str, depth: usize) -> Result<String> {
        let evaluation = self.evaluate_item_pipeline(expression, depth)?;
        Ok(evaluation
            .values
            .into_iter()
            .map(|value| value.escaped_identity)
            .collect::<Vec<_>>()
            .join(&evaluation.separator))
    }

    fn evaluate_item_pipeline(
        &self,
        expression: &str,
        depth: usize,
    ) -> Result<ItemExpressionEvaluation<'a>> {
        let (pipeline, separator) = split_item_separator(expression)?;
        let stages = split_item_pipeline(pipeline)?;
        let item_type = stages
            .first()
            .map(|stage| stage.trim())
            .filter(|item_type| !item_type.is_empty())
            .ok_or_else(|| anyhow!("Item expression has no item type"))?;
        let separator = separator
            .map(|value| self.evaluate_with_depth(&unquote(value.trim()), depth + 1))
            .transpose()?;

        let mut values = self
            .model
            .iter_items(item_type)
            .map(|item| EvaluatedItemExpression {
                provenance: ItemProvenance::SourceRetained(item),
                escaped_identity: item.escaped_name.clone(),
            })
            .collect::<Vec<_>>();
        for stage in &stages[1..] {
            let stage = stage.trim();
            if (stage.starts_with('\'') && stage.ends_with('\''))
                || (stage.starts_with('"') && stage.ends_with('"'))
            {
                let template = unquote(stage);
                if template.contains("%(") {
                    require_source_items(&values, "metadata transform")?;
                }
                for value in &mut values {
                    if value.escaped_identity.is_empty() {
                        continue;
                    }
                    value.escaped_identity = match value.provenance {
                        ItemProvenance::SourceRetained(source) => Self::with_item_expression(
                            self.model,
                            self.current_file_path(),
                            source,
                            &value.escaped_identity,
                            false,
                        )
                        .evaluate(&template)?,
                        ItemProvenance::MetadataCleared(source) => Self::with_item_expression(
                            self.model,
                            self.current_file_path(),
                            source,
                            &value.escaped_identity,
                            true,
                        )
                        .evaluate(&template)?,
                        ItemProvenance::SourceLess => {
                            Self::with_current_file(self.model, self.current_file_path())
                                .evaluate(&template)?
                        }
                    };
                }
                continue;
            }

            let (method, argument_expression) = parse_invocation(stage)
                .ok_or_else(|| anyhow!("Malformed item function: {stage}"))?;
            let argument_expression =
                self.expand_properties_and_metadata(argument_expression, depth + 1)?;
            let arguments = split_arguments(&argument_expression)?;
            if method.eq_ignore_ascii_case("Metadata") {
                require_arguments(method, &arguments, 1)?;
                let mut transformed = Vec::new();
                for value in values {
                    let metadata = value
                        .provenance
                        .retained_source()
                        .and_then(|source| source.get_metadata_escaped(&arguments[0]))
                        .map(Cow::into_owned)
                        .unwrap_or_default();
                    for identity in crate::escaping::tokenize_list(&metadata)? {
                        transformed.push(EvaluatedItemExpression {
                            provenance: value.provenance,
                            escaped_identity: identity.to_string(),
                        });
                    }
                }
                values = transformed;
            } else if matches_ignore_ascii_case(method, ITEM_SPEC_MODIFIERS) {
                require_arguments(method, &arguments, 0)?;
                require_source_items(&values, method)?;
                let mut transformed = Vec::with_capacity(values.len());
                for mut value in values {
                    if value.escaped_identity.is_empty() {
                        continue;
                    }
                    value.escaped_identity = match value.provenance {
                        ItemProvenance::SourceRetained(source) => source
                            .get_metadata_for_identity_escaped(
                                method,
                                &value.escaped_identity,
                                false,
                            )
                            .map(Cow::into_owned)
                            .unwrap_or_default(),
                        ItemProvenance::MetadataCleared(source) => source
                            .get_metadata_for_identity_escaped(
                                method,
                                &value.escaped_identity,
                                true,
                            )
                            .map(Cow::into_owned)
                            .unwrap_or_default(),
                        ItemProvenance::SourceLess => {
                            unreachable!("source capability is checked before item-spec modifiers")
                        }
                    };
                    if !value.escaped_identity.is_empty() {
                        transformed.push(value);
                    }
                }
                values = transformed;
            } else if method.eq_ignore_ascii_case("Distinct") {
                require_arguments(method, &arguments, 0)?;
                let mut seen = std::collections::HashSet::new();
                values.retain(|value| {
                    !value.escaped_identity.is_empty()
                        && seen.insert(dotnet_ordinal_ignore_case_key(&value.escaped_identity))
                });
            } else if method.eq_ignore_ascii_case("DistinctWithCase") {
                require_arguments(method, &arguments, 0)?;
                let mut seen = std::collections::HashSet::new();
                values.retain(|value| {
                    !value.escaped_identity.is_empty()
                        && seen.insert(value.escaped_identity.clone())
                });
            } else if method.eq_ignore_ascii_case("Reverse") {
                require_arguments(method, &arguments, 0)?;
                values.reverse();
            } else if method.eq_ignore_ascii_case("Count") {
                require_arguments(method, &arguments, 0)?;
                values = vec![EvaluatedItemExpression {
                    provenance: ItemProvenance::SourceLess,
                    escaped_identity: values.len().to_string(),
                }];
            } else if method.eq_ignore_ascii_case("AnyHaveMetadataValue") {
                require_arguments(method, &arguments, 2)?;
                require_source_items(&values, method)?;
                let source_value = values.iter().find(|value| {
                    item_expression_metadata(value, &arguments[0])
                        .is_some_and(|metadata| ordinal_ignore_case(&metadata, &arguments[1]))
                });
                values = vec![EvaluatedItemExpression {
                    provenance: source_value
                        .map_or(ItemProvenance::SourceLess, |value| value.provenance),
                    escaped_identity: if source_value.is_some() {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                }];
            } else if method.eq_ignore_ascii_case("HasMetadata") {
                require_arguments(method, &arguments, 1)?;
                require_source_items(&values, method)?;
                values.retain(|value| {
                    item_expression_metadata(value, &arguments[0])
                        .is_some_and(|metadata| !metadata.is_empty())
                });
            } else if method.eq_ignore_ascii_case("WithMetadataValue")
                || method.eq_ignore_ascii_case("WithoutMetadataValue")
            {
                require_arguments(method, &arguments, 2)?;
                require_source_items(&values, method)?;
                let retain_matches = method.eq_ignore_ascii_case("WithMetadataValue");
                values.retain(|value| {
                    let matches = item_expression_metadata(value, &arguments[0])
                        .is_some_and(|metadata| ordinal_ignore_case(&metadata, &arguments[1]));
                    matches == retain_matches
                });
            } else if method.eq_ignore_ascii_case("ClearMetadata") {
                require_arguments(method, &arguments, 0)?;
                for value in &mut values {
                    value.provenance = value.provenance.clear_metadata();
                }
            } else if method.eq_ignore_ascii_case("Exists") {
                require_arguments(method, &arguments, 0)?;
                require_source_items(&values, method)?;
                values.retain(|value| {
                    !value.escaped_identity.is_empty()
                        && value
                            .provenance
                            .source()
                            .is_some_and(|source| source.identity_exists(&value.escaped_identity))
                });
            } else if method.eq_ignore_ascii_case("DirectoryName") {
                require_arguments(method, &arguments, 0)?;
                require_source_items(&values, method)?;
                let mut transformed = Vec::with_capacity(values.len());
                for mut value in values {
                    if value.escaped_identity.is_empty() {
                        continue;
                    }
                    let Some(source) = value.provenance.source() else {
                        continue;
                    };
                    value.escaped_identity = source.directory_name(&value.escaped_identity);
                    if !value.escaped_identity.is_empty() {
                        transformed.push(value);
                    }
                }
                values = transformed;
            } else if method.eq_ignore_ascii_case("GetPathsOfAllDirectoriesAbove") {
                require_arguments(method, &arguments, 0)?;
                let mut directories = BTreeMap::<Vec<u16>, String>::new();
                for value in &values {
                    if value.escaped_identity.is_empty() {
                        continue;
                    }
                    let identity = normalized_item_path(&unescape_once(&value.escaped_identity));
                    let rooted = if identity.is_absolute() {
                        lexical_absolute(&identity)
                    } else {
                        let directory = value
                            .provenance
                            .source()
                            .map(Item::evaluation_directory)
                            .unwrap_or(&self.base_directory);
                        lexical_absolute(&directory.join(identity))
                    }?;
                    for ancestor in rooted.parent().into_iter().flat_map(Path::ancestors) {
                        let directory = display_path(ancestor);
                        directories
                            .entry(dotnet_ordinal_ignore_case_key(&directory))
                            .or_insert(directory);
                    }
                }
                values = directories
                    .into_values()
                    .map(|directory| EvaluatedItemExpression {
                        provenance: ItemProvenance::SourceLess,
                        escaped_identity: escape(&directory),
                    })
                    .collect();
            } else if method.eq_ignore_ascii_case("Combine") {
                require_arguments(method, &arguments, 1)?;
                let mut transformed = Vec::with_capacity(values.len());
                for mut value in values {
                    if value.escaped_identity.is_empty() {
                        continue;
                    }
                    let combined =
                        Path::new(&unescape_once(&value.escaped_identity)).join(&arguments[0]);
                    value.escaped_identity = escape(&display_path(&combined));
                    value.provenance = ItemProvenance::SourceLess;
                    transformed.push(value);
                }
                values = transformed;
            } else if let Some(item_string_function) = resolve_item_string_function(method) {
                let arguments = arguments
                    .iter()
                    .map(|argument| unescape_once(argument))
                    .collect::<Vec<_>>();
                for value in &mut values {
                    let receiver = unescape_once(&value.escaped_identity);
                    let result =
                        invoke_item_string_function(item_string_function, &receiver, &arguments)?;
                    value.escaped_identity = escape(&result);
                }
            } else {
                bail!(
                    "Unsupported item function: {method}. Per-item System.String calls are restricted to the deterministic allowlist"
                );
            }
        }

        if let Some(separator) = separator {
            let escaped_identity = values
                .into_iter()
                .map(|value| value.escaped_identity)
                .collect::<Vec<_>>()
                .join(&separator);
            values = vec![EvaluatedItemExpression {
                provenance: ItemProvenance::SourceLess,
                escaped_identity,
            }];
        }

        Ok(ItemExpressionEvaluation {
            values,
            separator: ";".to_string(),
        })
    }

    fn evaluate_condition_function(&self, name: &str, arguments: &[String]) -> Result<bool> {
        let arguments = arguments
            .iter()
            .map(|argument| self.evaluate(argument).map(|value| unescape_once(&value)))
            .collect::<Result<Vec<_>>>()?;
        if name.eq_ignore_ascii_case("Exists") {
            require_arguments(name, &arguments, 1)?;
            if arguments[0].is_empty() {
                return Ok(false);
            }
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
    "ModifiedTime",
    "CreatedTime",
    "AccessedTime",
    "DefiningProjectFullPath",
    "DefiningProjectDirectory",
    "DefiningProjectName",
    "DefiningProjectExtension",
];

fn normalized_item_path(value: &str) -> PathBuf {
    if std::path::MAIN_SEPARATOR == '\\' {
        PathBuf::from(value.replace('/', "\\"))
    } else {
        PathBuf::from(value.replace('\\', "/"))
    }
}

fn item_expression_metadata<'a>(
    value: &'a EvaluatedItemExpression<'a>,
    name: &str,
) -> Option<Cow<'a, str>> {
    let source = value.provenance.retained_source()?;
    Some(
        source
            .get_metadata_escaped(name)
            .unwrap_or(Cow::Borrowed("")),
    )
}

fn require_source_items(values: &[EvaluatedItemExpression<'_>], stage: &str) -> Result<()> {
    if values.iter().any(|value| !value.provenance.has_source()) {
        bail!(
            "Item pipeline stage '{stage}' requires source-item context, but a prior stage produced a source-less value"
        );
    }
    Ok(())
}

fn ordinal_ignore_case(left: &str, right: &str) -> bool {
    dotnet_ordinal_ignore_case_key(left) == dotnet_ordinal_ignore_case_key(right)
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

fn parse_member_chain(input: &str) -> Result<Vec<ChainOperation<'_>>> {
    let input = input.trim();
    let mut operations = Vec::new();
    let mut position = 0;

    while position < input.len() {
        while input.as_bytes().get(position) == Some(&b' ') {
            position += 1;
        }
        if input.as_bytes().get(position) == Some(&b'[') {
            let end = find_matching_bracket(input, position)?;
            operations.push(ChainOperation::Indexer {
                argument: input[position + 1..end].trim(),
            });
            position = end + 1;
        } else {
            let start = position;
            while let Some(byte) = input.as_bytes().get(position) {
                if matches!(byte, b'(' | b'.' | b'[') {
                    break;
                }
                position += 1;
            }
            let name = input[start..position].trim();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                bail!("Malformed property-function member chain: {input}");
            }
            let arguments = if input.as_bytes().get(position) == Some(&b'(') {
                let end = find_matching_parenthesis(input, position)?;
                let arguments = &input[position + 1..end];
                position = end + 1;
                Some(arguments)
            } else {
                None
            };
            operations.push(ChainOperation::Member { name, arguments });
        }

        while input.as_bytes().get(position) == Some(&b' ') {
            position += 1;
        }
        if position == input.len() {
            break;
        }
        if input.as_bytes().get(position) == Some(&b'[') {
            continue;
        }
        if input.as_bytes().get(position) != Some(&b'.') {
            bail!(
                "Malformed property-function member chain near '{}'",
                &input[position..]
            );
        }
        position += 1;
        if position == input.len() {
            bail!("Property-function member chain cannot end with '.'");
        }
    }

    if operations.is_empty() {
        bail!("Property function has no member to invoke");
    }
    Ok(operations)
}

fn find_matching_bracket(input: &str, opening: usize) -> Result<usize> {
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
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(position);
                }
            }
            _ => {}
        }
    }
    bail!("Unterminated indexer in '{input}'")
}

fn operation_is_allowed(type_name: &str, operation: &ChainOperation<'_>) -> bool {
    match operation {
        ChainOperation::Member { name, arguments } => is_allowed(
            type_name,
            name,
            if arguments.is_some() {
                InvocationKind::InstanceMethod
            } else {
                InvocationKind::InstanceProperty
            },
        ),
        ChainOperation::Indexer { .. } => is_allowed(type_name, "Item", InvocationKind::Indexer),
    }
}

fn render_intrinsic(outcome: IntrinsicOutcome) -> Result<String> {
    match (outcome.result_rule, outcome.value) {
        (ResultRule::Escape, IntrinsicValue::Strings(values)) => Ok(values
            .into_iter()
            .skip_while(String::is_empty)
            .map(|value| DecodedString::new(value).into_escaped().into_string())
            .collect::<Vec<_>>()
            .join(";")),
        (ResultRule::Escape, IntrinsicValue::Bytes(values)) => Ok(values
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(";")),
        (result_rule, value) => {
            let value = value.to_msbuild_string()?;
            Ok(if result_rule == ResultRule::Escape {
                DecodedString::new(value).into_escaped().into_string()
            } else {
                value
            })
        }
    }
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

fn split_raw_arguments(input: &str) -> Result<Vec<&str>> {
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
            ')' => {
                if depth == 0 {
                    bail!("Malformed function arguments: unexpected ')' in {input}");
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                arguments.push(input[start..position].trim());
                start = position + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        bail!("Malformed function arguments: {input}")
    }
    arguments.push(input[start..].trim());
    Ok(arguments)
}

fn split_arguments(input: &str) -> Result<Vec<String>> {
    split_raw_arguments(input)
        .map(|arguments| arguments.into_iter().map(unquote).collect::<Vec<_>>())
}

fn matches_ignore_ascii_case(value: &str, options: &[&str]) -> bool {
    options
        .iter()
        .any(|option| value.eq_ignore_ascii_case(option))
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
    fn item_function_arguments_expand_properties_and_current_metadata_before_splitting() {
        let mut model = ProjectModel::new();
        model.set_property("Args".to_string(), "'M','x'".to_string());
        for identity in ["a", "b"] {
            let mut item = Item::new(
                "I".to_string(),
                identity.to_string(),
                Arc::new(MetadataMap::new()),
                PathBuf::from("."),
                PathBuf::from("project.proj"),
            );
            item.set_metadata("M".to_string(), "x".to_string());
            model.add_item(item);
        }
        model.add_item(Item::new(
            "ArgText".to_string(),
            "M".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("."),
            PathBuf::from("project.proj"),
        ));
        let mut outer = Item::new(
            "Outer".to_string(),
            "holder".to_string(),
            Arc::new(MetadataMap::new()),
            PathBuf::from("."),
            PathBuf::from("project.proj"),
        );
        outer.set_metadata("Args".to_string(), "'M','x'".to_string());
        let evaluator = ExpressionEvaluator::with_item(&model, Path::new("project.proj"), &outer);

        assert_eq!(
            evaluator
                .evaluate("@(I->WithMetadataValue($(Args)))")
                .unwrap(),
            "a;b"
        );
        assert_eq!(
            evaluator
                .evaluate("@(I->WithMetadataValue(%(Args)))")
                .unwrap(),
            "a;b"
        );
        assert_eq!(
            evaluator
                .evaluate("@(I->HasMetadata(@(ArgText))->Count())")
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn ordinal_ignore_case_key_matches_dotnet_utf16_edge_cases() {
        assert!(ordinal_ignore_case("K", "k"));
        assert!(!ordinal_ignore_case("K", "K"));
        assert!(ordinal_ignore_case("σ", "ς"));
        assert!(ordinal_ignore_case("σ", "Σ"));
        assert!(ordinal_ignore_case("µ", "Μ"));
        assert!(!ordinal_ignore_case("i", "ı"));
        assert!(!ordinal_ignore_case("s", "ſ"));
        assert!(!ordinal_ignore_case("ß", "ẞ"));
        assert!(!ordinal_ignore_case("%41", "A"));
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
            format!("root{}root/sub", std::path::MAIN_SEPARATOR)
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
            if cfg!(windows) { "True" } else { "False" }
        );
        assert_eq!(
            evaluator.evaluate("$(SdkVersion.Substring(0, 4))").unwrap(),
            "10.0"
        );
    }

    #[test]
    fn upstream_property_function_no_arguments_and_nested_chain() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("SomeStuff".to_string(), "This IS SOME STUff".to_string());
        model.set_property("Value".to_string(), "3".to_string());
        let evaluator = ExpressionEvaluator::new(&model);

        // Ports Expander_Tests.PropertyFunctionNoArguments and
        // PropertyFunctionPropertyWithArgumentNestedAndChainedFunction.
        assert_eq!(
            evaluator.evaluate("$(SomeStuff.ToUpperInvariant())")?,
            "THIS IS SOME STUFF"
        );
        assert_eq!(
            evaluator.evaluate(
                "$(SomeStuff.SubString(1$(Value)).ToLowerInvariant().SubString($(Value)))"
            )?,
            "ff"
        );
        assert_eq!(evaluator.evaluate("$(SomeStuff.Length.ToString())")?, "18");
        Ok(())
    }

    #[test]
    fn upstream_property_function_static_method_chained() -> Result<()> {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        // Deterministic ISO-date variant of
        // Expander_Tests.PropertyFunctionStaticMethodChained.
        assert_eq!(
            evaluator.evaluate(
                "$([System.DateTime]::Parse('2010-12-25').ToString('yyyy/MM/dd HH:mm:ss'))"
            )?,
            "2010/12/25 00:00:00"
        );
        assert_eq!(
            evaluator.evaluate("$([System.Version]::Parse('10.2.3.4').ToString(2))")?,
            "10.2"
        );
        Ok(())
    }

    #[test]
    fn upstream_property_function_in_condition() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("PathRoot".to_string(), r"C:\goo".to_string());
        model.set_property("PathRoot2".to_string(), "C:\\goop\\".to_string());
        let evaluator = ExpressionEvaluator::new(&model);

        assert!(evaluator.evaluate_condition("'$(PathRoot2.Contains('\\'))' == 'true'")?);
        let error = evaluator
            .evaluate_condition("$(PathRoot.EndsWith('\\'))")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not in the native MSBuild property-function allowlist"));
        Ok(())
    }

    #[test]
    fn upstream_property_function_medley_representative_subset() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("input".to_string(), "EXPORT a".to_string());
        model.set_property("listofthings".to_string(), "a;b;c;d;e".to_string());
        model.set_property("position".to_string(), "4".to_string());
        model.set_property("e".to_string(), "xxx".to_string());
        model.set_property("a".to_string(), "no".to_string());
        model.set_property("c".to_string(), "1".to_string());
        let evaluator = ExpressionEvaluator::new(&model);

        assert_eq!(evaluator.evaluate("$(input[1])")?, "X");
        assert_eq!(
            evaluator.evaluate("$(listofthings.Split(';')[$(position)])")?,
            "e"
        );
        assert_eq!(
            evaluator.evaluate("$([MSBuild]::Add(1,2).CompareTo(3))")?,
            "0"
        );
        assert_eq!(
            evaluator.evaluate("$([System.Convert]::ToInt32($([MSBuild]::Add(1,2))).Equals(3))")?,
            "True"
        );
        assert_eq!(evaluator.evaluate("$(a.Insert(0,'%28'))")?, "%28no");
        assert_eq!(evaluator.evaluate("$(e.Length.ToString())")?, "3");
        assert_eq!(evaluator.evaluate("$([MSBuild]::Escape(';'))")?, "%3b");
        assert_eq!(evaluator.evaluate("$([MSBuild]::UnEscape('%3b'))")?, ";");
        assert_eq!(
            evaluator.evaluate("$([System.Int32]::MaxValue)")?,
            i32::MAX.to_string()
        );
        assert_eq!(evaluator.evaluate("$(a.Equals($(c)))")?, "False");
        assert_eq!(
            evaluator.evaluate("$([System.String]::CompareOrdinal($(a),$(c)))")?,
            "61"
        );
        assert!(evaluator.evaluate("$(a.CompareTo($(c)))").is_err());
        Ok(())
    }

    #[test]
    fn native_allowlist_covers_representative_required_types() -> Result<()> {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        assert_eq!(
            evaluator.evaluate("$([system.math]::aBs(-32769))")?,
            "32769"
        );
        assert_eq!(
            evaluator.evaluate("$([System.Convert]::ToInt64('28', 16))")?,
            "40"
        );
        assert_eq!(
            evaluator.evaluate("$([System.Version]::new(1, 2, 3).Build)")?,
            "3"
        );
        assert_eq!(
            evaluator.evaluate(
                "$([System.Guid]::Parse('00112233-4455-6677-8899-aabbccddeeff').ToString('N'))"
            )?,
            "00112233445566778899aabbccddeeff"
        );
        assert_eq!(
            evaluator.evaluate("$([System.Guid]::Empty)")?,
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            evaluator.evaluate("$([System.IO.Path]::DirectorySeparatorChar)")?,
            std::path::MAIN_SEPARATOR.to_string()
        );
        assert_eq!(
            evaluator.evaluate("$([MSBuild]::StableStringHash('abc'))")?,
            "536991770"
        );
        assert_eq!(
            evaluator.evaluate("$([MSBuild]::StableStringHash('abc', 'Fnv1a32bit'))")?,
            "-1373726339"
        );
        assert_eq!(
            evaluator.evaluate("$([MSBuild]::StableStringHash('abc', 'Fnv1a32bitFast'))")?,
            "440920331"
        );
        Ok(())
    }

    #[test]
    fn native_core_reviewer_cases_match_clr_semantics() -> Result<()> {
        let mut model = ProjectModel::new();
        model.set_property("SharpS".to_string(), "ß".to_string());
        model.set_property("Sigma".to_string(), "ΟΣ".to_string());
        model.set_property("S".to_string(), "aba".to_string());
        let evaluator = ExpressionEvaluator::new(&model);
        let cases = [
            (
                "$([MSBuild]::Add(9223372036854775807,1))",
                "-9223372036854775808",
            ),
            (
                "$([MSBuild]::Subtract(-9223372036854775808,1))",
                "9223372036854775807",
            ),
            ("$([MSBuild]::Multiply(9223372036854775807,2))", "-2"),
            ("$([MSBuild]::LeftShift(1,32))", "1"),
            ("$([MSBuild]::LeftShift(1,-1))", "-2147483648"),
            ("$([System.Convert]::ToInt32('FFFFFFFF',16))", "-1"),
            ("$([System.Convert]::ToInt64('FFFFFFFFFFFFFFFF',16))", "-1"),
            ("$([System.Convert]::ToString(-1,16))", "ffff"),
            (
                "$([System.Convert]::ToString($([System.Int64]::Parse('-1')),16))",
                "ffff",
            ),
            ("$([System.IO.Path]::Combine())", ""),
            ("$([System.IO.Path]::Combine('a'))", "a"),
            ("$([System.Version]::new())", "0.0"),
            ("$([System.Version]::new('1.2.3'))", "1.2.3"),
            ("$([System.Version]::new(1,2))", "1.2"),
            ("$([System.Version]::new(1,2,3,4))", "1.2.3.4"),
            ("$([System.String]::Copy('a;b').Split(';'))", "a;b"),
            (
                "$([System.String]::Join(',', $([System.String]::Copy('a;b').Split(';'))))",
                "a,b",
            ),
            (
                "$([System.String]::Join($([System.String]::Copy(',')[0]), $([System.String]::Copy('a;b').Split(';'))))",
                "a,b",
            ),
            (
                "$([System.String]::Copy('a;b').Split($([System.String]::Copy(';')[0])))",
                "a;b",
            ),
            ("$([System.String]::Copy(';;a;;').Split(';'))", "a;;"),
            ("$([System.String]::Copy('a;;b;').Split(';'))", "a;;b;"),
            ("$([System.String]::Copy(';;;').Split(';'))", ""),
            ("$([System.String]::Copy('a b').Split())", "a;b"),
            ("$(S.Replace('b',null))", "aa"),
            ("$(SharpS.ToUpperInvariant())", "ß"),
            ("$(Sigma.ToLowerInvariant())", "οσ"),
            ("$([System.String]::CompareOrdinal('😀','�'))", "-10176"),
            ("$([System.String]::CompareOrdinal(null,'a'))", "-1"),
            (
                "$([System.DateTime]::Parse('2010-12-25T01:02:03').ToString('yyyy-MM-dd HH:mm:ss'))",
                "2010-12-25 01:02:03",
            ),
            (
                "$([System.Guid]::Parse('00112233-4455-6677-8899-aabbccddeeff').ToString('X'))",
                "{0x00112233,0x4455,0x6677,{0x88,0x99,0xaa,0xbb,0xcc,0xdd,0xee,0xff}}",
            ),
            (
                "$([System.Guid]::Parse('(00112233-4455-6677-8899-aabbccddeeff)').ToString('D'))",
                "00112233-4455-6677-8899-aabbccddeeff",
            ),
            ("$([System.Int32]::Parse('42').ToString('D4'))", "0042"),
            ("$([System.Int32]::Parse('-1').ToString('X'))", "FFFFFFFF"),
            ("$([MSBuild]::Escape(null))", ""),
            ("$([MSBuild]::Unescape(null))", ""),
            ("$([System.String]::Copy($([MSBuild]::Escape(';'))))", "%3B"),
            ("$([MSBuild]::ValueOrDefault(null,'fallback'))", "fallback"),
            ("$([System.String]::IsNullOrEmpty(null))", "True"),
            ("$([System.IO.Path]::ChangeExtension(null,'.txt'))", ""),
            ("$([System.Math]::Abs(-32769))", "32769"),
        ];
        for (expression, expected) in cases {
            assert_eq!(
                evaluator.evaluate(expression)?,
                expected,
                "expression: {expression}"
            );
        }
        assert_eq!(
            evaluator.evaluate("$([System.IO.Path]::Combine('a','b','c','d','e'))")?,
            ["a", "b", "c", "d", "e"]
                .iter()
                .collect::<PathBuf>()
                .display()
                .to_string()
        );
        assert_eq!(
            evaluator.evaluate("$([System.String]::Copy('a%3Bb,c').Split(','))")?,
            "a%3Bb;c"
        );
        let components = (0..20).map(|index| format!("s{index}")).collect::<Vec<_>>();
        let arguments = components
            .iter()
            .map(|component| format!("'{component}'"))
            .collect::<Vec<_>>()
            .join(",");
        let relative = components.iter().collect::<PathBuf>().display().to_string();
        assert_eq!(
            evaluator.evaluate(&format!("$([System.IO.Path]::Combine({arguments}))"))?,
            relative
        );
        assert!(
            evaluator
                .evaluate(&format!("$([MSBuild]::NormalizePath({arguments}))"))?
                .ends_with(&relative)
        );
        Ok(())
    }

    #[test]
    fn native_core_invalid_inputs_return_errors_without_panicking() {
        let mut model = ProjectModel::new();
        model.set_property("S".to_string(), "abc".to_string());
        let evaluator = ExpressionEvaluator::new(&model);
        for expression in [
            "$([System.String]::Copy(null))",
            "$([System.String]::Copy($([System.String]::Copy('xy')[0])))",
            "$([System.String]::Copy('a b').Split(null))",
            "$([System.String]::Join(null, 'a', 'b'))",
            "$([System.IO.Path]::Combine('a',null))",
            "$([System.IO.Path]::GetFileName(null))",
            "$([System.IO.Path]::IsPathRooted(null))",
            "$([System.IO.Path]::GetDirectoryName(null))",
            "$([System.IO.Path]::GetFileNameWithoutExtension(null))",
            "$([System.IO.Path]::GetExtension(null))",
            "$([System.IO.Path]::GetPathRoot(null))",
            "$([System.IO.Path]::HasExtension(null))",
            "$(S.Replace('','x'))",
            "$(S.Substring(-1))",
            "$(S.Remove(4))",
            "$(S.Trim('a'))",
            "$(S[0].Length)",
            "$([System.Math]::Abs(-2147483648))",
            "$([System.Math]::Round(1.0,16))",
            "$([System.Math]::Max(1,2))",
            "$([System.DateTime]::Parse('2023-02-29').ToString('yyyy-MM-dd'))",
            "$([System.DateTime]::Parse('12/25/2010'))",
            "$([System.Guid]::Parse('not-a-guid'))",
            "$([System.Guid]::Parse('{0x00112233,0x4455,0x6677,{0x88,0x99,0xaa,0xbb,0xcc,0xdd,0xee,0xff}}'))",
            "$([System.Guid]::Parse('{ 0x00112233, 0x4455, 0x6677, { 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff } }'))",
            "$([System.Guid]::Empty.ToString('Z'))",
            "$([System.Int32]::Parse('not-an-int'))",
            "$([System.Int32]::Parse('1').ToString('Q'))",
            "$([System.Convert]::ToInt32('-1',16))",
            "$([System.Convert]::ToInt32('100000000',16))",
            "$([System.Version]::new(-1,2))",
            "$([System.Version]::new(1,2).ToString(3))",
            "$([MSBuild]::Divide(-9223372036854775808,-1))",
            "$([MSBuild]::Modulo(-9223372036854775808,-1))",
            "$([MSBuild]::Add(1.5,2.0))",
            "$([MSBuild]::Divide(1.0,0.0))",
            "$([System.Convert]::ToInt32(null))",
            "$([System.Convert]::ToInt32('42'))",
            "$([System.Convert]::ToDouble('inf'))",
            "$([System.Double]::Parse('inf'))",
            "$([System.Convert]::ToString(null))",
            "$([MSBuild]::SubstringByAsciiChars('abc',1,2147483647))",
            "$([MSBuild]::GetPathOfFileAbove('sub/file.props','.'))",
            "$([MSBuild]::VersionEquals('garbage','garbage'))",
            "$([MSBuild]::AreFeaturesEnabled('garbage'))",
            "$([MSBuild]::StableStringHash('abc','Fnv1a64bit'))",
            "$([System.IO.Path]::GetFullPath('child','relative-base'))",
            "$([MSBuild]::GetTargetFrameworkIdentifier('net8.0'))",
        ] {
            assert!(
                evaluator.evaluate(expression).is_err(),
                "expression should fail: {expression}"
            );
        }
    }

    #[test]
    fn restricted_allowlisted_static_works_and_state_mutation_stays_blocked() -> Result<()> {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);
        let variable = "MSBUILD_RS_PROPERTY_FUNCTION_BLOCK_TEST";
        let before = std::env::var_os(variable);

        assert_eq!(
            evaluator.evaluate("$([System.Math]::Abs(-32769))")?,
            "32769"
        );
        let error = evaluator
            .evaluate(
                "$([System.Environment]::SetEnvironmentVariable('MSBUILD_RS_PROPERTY_FUNCTION_BLOCK_TEST', 'x'))",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4185"));
        assert_eq!(std::env::var_os(variable), before);
        let current_directory = std::env::current_dir()?;
        let error = evaluator
            .evaluate("$([System.Environment]::set_CurrentDirectory('.'))")
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4185"));
        assert_eq!(std::env::current_dir()?, current_directory);

        let directory = TempDir::new()?;
        let marker = directory.path().join("must-not-exist.txt");
        let error = evaluator
            .evaluate(&format!(
                "$([System.Diagnostics.Process]::Start('cmd.exe', '/c echo bad>{}'))",
                display_path(&marker)
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4212"));
        assert!(!marker.exists());
        let error = evaluator
            .evaluate(
                "$([System.Diagnostics.Process]::Start($([System.Int32]::Parse('not-a-number'))))",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("MSB4212"));
        assert!(!error.contains("not-a-number"));
        Ok(())
    }

    #[test]
    fn registry_prefix_syntax_and_missing_values_match_msbuild() -> Result<()> {
        let model = ProjectModel::new();
        let evaluator = ExpressionEvaluator::new(&model);

        assert_eq!(
            evaluator.evaluate(
                r"$(Registry:HKEY_CURRENT_USER\Software\Microsoft\MSBuild_rs_missing@Value)"
            )?,
            ""
        );
        assert_eq!(
            evaluator.evaluate(
                r"$(HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@VSTSDBDirectory)"
            )?,
            ""
        );
        assert!(
            evaluator
                .evaluate(r"$(HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\VisualStudio\9.0\VSTSDB@Other)")
                .unwrap_err()
                .to_string()
                .contains("Registry:")
        );
        assert_eq!(
            evaluator.evaluate(
                "$([MSBuild]::GetRegistryValueFromView(null, null, $([System.Int32]::Parse('42'))).CompareTo(100))"
            )?,
            "-1"
        );
        Ok(())
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
            "$([MSBuild]::GetPathOfFileAbove('Directory.Build.props', $(MSBuildThisFileDirectory)..))",
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
            if cfg!(windows) {
                r"src\project"
            } else {
                "C:/repo/src/project"
            }
        );
        assert_eq!(
            ExpressionEvaluator::new(&model)
                .evaluate("$([MSBuild]::MakeRelative('C:\\REPO\\', 'c:\\repo\\src\\project'))")?,
            if cfg!(windows) {
                r"src\project"
            } else {
                "c:/repo/src/project"
            }
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
        assert!(!evaluator.evaluate_condition("Exists('')")?);
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
