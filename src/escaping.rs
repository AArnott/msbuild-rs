use anyhow::{Result, bail};

/// Whether an item specification contains an authored (unescaped) wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemSpecKind {
    Literal,
    Glob,
}

/// Text that still carries MSBuild's `%XX` escaping and is safe to reinsert
/// into an expression or list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapedString(String);

impl EscapedString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn decode(&self) -> DecodedString {
        DecodedString(unescape_once(&self.0))
    }
}

/// Text after exactly one MSBuild unescape operation. It must be escaped again
/// before being reinserted into an expression or list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedString(String);

impl DecodedString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn into_escaped(self) -> EscapedString {
        EscapedString(escape(&self.0))
    }
}

/// Decode one MSBuild `%XX` layer.
///
/// Callers retain the escaped source separately whenever list boundaries or
/// wildcard classification still matter.
pub fn unescape_once(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'%'
            && position + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[position + 1]), hex(bytes[position + 2]))
        {
            output.push(char::from_u32(high * 16 + low).expect("two hex digits are a character"));
            position += 3;
            continue;
        }

        let character = value[position..]
            .chars()
            .next()
            .expect("position must be on a character boundary");
        output.push(character);
        position += character.len_utf8();
    }
    output
}

/// Escape the characters that have syntactic meaning to MSBuild.
pub fn escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        let escaped = match character {
            '%' => Some("25"),
            '*' => Some("2A"),
            '?' => Some("3F"),
            '@' => Some("40"),
            '$' => Some("24"),
            '(' => Some("28"),
            ')' => Some("29"),
            ';' => Some("3B"),
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

/// Split an MSBuild list without splitting expression bodies or quoted
/// transform/function arguments. Empty and whitespace-only entries are removed.
pub fn tokenize_list(expression: &str) -> Result<Vec<&str>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut expression_depth = 0usize;
    let mut quote = None;
    let bytes = expression.as_bytes();
    let mut position = 0;

    while position < bytes.len() {
        let character = expression[position..]
            .chars()
            .next()
            .expect("position must be on a character boundary");

        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            position += character.len_utf8();
            continue;
        }

        if expression_depth > 0 && matches!(character, '\'' | '"') {
            quote = Some(character);
            position += character.len_utf8();
            continue;
        }

        match character {
            '$' | '@' | '%' if bytes.get(position + 1) == Some(&b'(') => {
                expression_depth += 1;
                position += 2;
            }
            '(' if expression_depth > 0 => {
                expression_depth += 1;
                position += 1;
            }
            ')' if expression_depth > 0 => {
                expression_depth -= 1;
                position += 1;
            }
            ';' if expression_depth == 0 => {
                push_trimmed(&mut tokens, &expression[start..position]);
                start = position + 1;
                position += 1;
            }
            _ => position += character.len_utf8(),
        }
    }

    if expression_depth != 0 || quote.is_some() {
        bail!("Malformed expression in list: {expression}");
    }
    push_trimmed(&mut tokens, &expression[start..]);
    Ok(tokens)
}

pub fn classify_item_spec(escaped: &str) -> ItemSpecKind {
    let bytes = escaped.as_bytes();
    let mut position = 0;
    let mut has_wildcard = false;
    let mut has_escaped_wildcard = false;
    while position < bytes.len() {
        if bytes[position] == b'%'
            && position + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[position + 1]), hex(bytes[position + 2]))
        {
            has_escaped_wildcard |= matches!(high * 16 + low, 0x2a | 0x3f);
            position += 3;
            continue;
        }
        if matches!(bytes[position], b'*' | b'?') {
            has_wildcard = true;
        }
        position += 1;
    }
    let recursive_operators_are_legal = unescape_once(escaped)
        .split(['/', '\\'])
        .all(|component| !component.contains("**") || component == "**");
    if has_wildcard && !has_escaped_wildcard && recursive_operators_are_legal {
        ItemSpecKind::Glob
    } else {
        ItemSpecKind::Literal
    }
}

fn push_trimmed<'a>(tokens: &mut Vec<&'a str>, token: &'a str) {
    let token = token.trim();
    if !token.is_empty() {
        tokens.push(token);
    }
}

fn hex(value: u8) -> Option<u32> {
    match value {
        b'0'..=b'9' => Some(u32::from(value - b'0')),
        b'a'..=b'f' => Some(u32::from(value - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(value - b'A' + 10)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_is_decoded_exactly_once() {
        assert_eq!(unescape_once("a%3Bb%3bc"), "a;b;c");
        assert_eq!(unescape_once("%2A%3F%2512"), "*?%12");
        assert_eq!(escape("*?;%12"), "%2A%3F%3B%2512");
    }

    #[test]
    fn upstream_semicolon_tokenizer_tokenize_expression() -> Result<()> {
        // Exact data port of dotnet/msbuild
        // SemiColonTokenizer_Tests.TokenizeExpression.
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            (";", &[]),
            (";;", &[]),
            (" ; ; ", &[]),
            ("First", &["First"]),
            ("First;", &["First"]),
            ("First;Second", &["First", "Second"]),
            ("First;Second;Third", &["First", "Second", "Third"]),
            (
                " First ;\tSecond\t;\nThird\n",
                &["First", "Second", "Third"],
            ),
            (
                "@(foo->'xxx;xxx');@(foo, 'xxx;xxx');@(foo->'xxx;xxx', 'xxx;xxx')",
                &[
                    "@(foo->'xxx;xxx')",
                    "@(foo, 'xxx;xxx')",
                    "@(foo->'xxx;xxx', 'xxx;xxx')",
                ],
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(&tokenize_list(input)?, expected, "input: {input}");
        }
        Ok(())
    }

    #[test]
    fn escaped_wildcards_are_classified_as_literals() {
        assert_eq!(classify_item_spec("%2A"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("a%3Fb"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("[literal].txt"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("%2A-*.txt"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("%3f-?.txt"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("tree/**.txt"), ItemSpecKind::Literal);
        assert_eq!(classify_item_spec("*.cs"), ItemSpecKind::Glob);
        assert_eq!(classify_item_spec("a?.cs"), ItemSpecKind::Glob);
    }
}
