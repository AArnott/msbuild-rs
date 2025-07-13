use anyhow::Result;
use regex::Regex;

use crate::object_model::ProjectModel;

pub struct ExpressionEvaluator<'a> {
    model: &'a ProjectModel,
}

impl<'a> ExpressionEvaluator<'a> {
    pub fn new(model: &'a ProjectModel) -> Self {
        Self { model }
    }

    /// Evaluate a string that may contain property and item references
    pub fn evaluate(&self, input: &str) -> Result<String> {
        let mut result = input.to_string();

        // Replace property references $(PropertyName)
        let prop_regex = Regex::new(r"\$\(([^)]+)\)").unwrap();
        while let Some(captures) = prop_regex.captures(&result) {
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
        let item_regex = Regex::new(r"@\(([^)]+)\)").unwrap();
        while let Some(captures) = item_regex.captures(&result) {
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

        // Simple condition evaluation - supports basic comparisons
        if evaluated.is_empty() || evaluated == "false" || evaluated == "False" {
            return Ok(false);
        }

        if evaluated == "true" || evaluated == "True" {
            return Ok(true);
        }

        // Check for comparison operators
        if let Some((left, right)) = self.parse_comparison(&evaluated)? {
            return self.compare_values(&left, &right);
        }

        // If it's not empty and not false, consider it true
        Ok(!evaluated.trim().is_empty())
    }

    fn parse_comparison(&self, expr: &str) -> Result<Option<(String, String)>> {
        let expr = expr.trim();

        // Support == comparison
        if let Some(pos) = expr.find("==") {
            let left = expr[..pos].trim().to_string();
            let right = expr[pos + 2..].trim().to_string();
            return Ok(Some((left, right)));
        }

        // Support != comparison
        if let Some(pos) = expr.find("!=") {
            let left = expr[..pos].trim().to_string();
            let right = expr[pos + 2..].trim().to_string();
            // For != we'll return the comparison but negate the result
            return Ok(Some((format!("!{left}"), right)));
        }

        Ok(None)
    }

    fn compare_values(&self, left: &str, right: &str) -> Result<bool> {
        if let Some(actual_left) = left.strip_prefix('!') {
            // Handle != comparison
            return Ok(actual_left != right);
        }

        Ok(left == right)
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
}
