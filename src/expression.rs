use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::sync::LazyLock;

use crate::object_model::ProjectModel;

static PROPERTY_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\(([A-Za-z_][A-Za-z0-9_.-]*)\)").unwrap());
static ITEM_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@\(([A-Za-z_][A-Za-z0-9_.-]*)\)").unwrap());

pub struct ExpressionEvaluator<'a> {
    model: &'a ProjectModel,
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
    pub fn new(model: &'a ProjectModel) -> Self {
        Self { model }
    }

    /// Evaluate a string that may contain property and item references
    pub fn evaluate(&self, input: &str) -> Result<String> {
        let mut result = input.to_string();

        // Replace property references $(PropertyName)
        while let Some(captures) = PROPERTY_REFERENCE.captures(&result) {
            let full_match = &captures[0];
            let prop_name = &captures[1];

            let replacement = self
                .model
                .get_property(prop_name)
                .cloned()
                .unwrap_or_default();

            result = result.replace(full_match, &replacement);
        }

        // Replace item references @(ItemType)
        while let Some(captures) = ITEM_REFERENCE.captures(&result) {
            let full_match = &captures[0];
            let item_type = &captures[1];

            let replacement = self.model.get_all_item_names(item_type);
            result = result.replace(full_match, &replacement);
        }

        Ok(result)
    }

    /// Evaluate a condition expression
    pub fn evaluate_condition(&self, condition: &str) -> Result<bool> {
        let evaluated = self.evaluate(condition)?;
        ConditionParser::new(&evaluated)?.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_model::{Item, ProjectModel};
    use std::collections::HashMap;

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
}
