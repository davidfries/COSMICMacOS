// SPDX-License-Identifier: GPL-3.0-or-later
//! Calculator plugin.
//!
//! `= <expr>` always evaluates (and owns the query); any other input that
//! parses as an arithmetic expression with at least one binary operator
//! (`2+2`, `(1.5 * 4) ^ 2`) also gets a result row ahead of the other
//! plugins. Activating the row copies the result to the clipboard.
//!
//! The evaluator is a small recursive-descent parser over `f64`:
//! `+ - * / ^`, parentheses, unary `+`/`-` and decimals (`.5`, `1.`).
//! `^` is right-associative and binds tighter than unary minus, so
//! `-2^2 = -4` and `2^3^2 = 512`. `×`, `÷` and `−` are accepted as
//! aliases.

use crate::matching::Rank;
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};

/// Why an expression could not be evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalcError {
    Empty,
    /// Unexpected character or token at this character offset.
    Unexpected(usize),
    /// Input ended in the middle of an expression.
    UnexpectedEnd,
    /// The result is infinite or not a number (e.g. division by zero).
    NotFinite,
    /// Parentheses or unary signs are nested too deeply.
    TooDeep,
}

impl std::fmt::Display for CalcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalcError::Empty => write!(f, "type an expression"),
            CalcError::Unexpected(at) => write!(f, "unexpected input at position {}", at + 1),
            CalcError::UnexpectedEnd => write!(f, "incomplete expression"),
            CalcError::NotFinite => write!(f, "result is not a finite number"),
            CalcError::TooDeep => write!(f, "expression is nested too deeply"),
        }
    }
}

/// A parsed and evaluated expression.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Evaluation {
    pub value: f64,
    /// Whether the expression contains a binary operator (so `42` or `-3`
    /// alone does not count as a calculation).
    pub has_operator: bool,
}

/// Evaluates an arithmetic expression.
pub fn evaluate(input: &str) -> Result<Evaluation, CalcError> {
    let chars: Vec<char> = input.chars().collect();
    let mut parser = Parser {
        chars: &chars,
        pos: 0,
        depth: 0,
        has_operator: false,
    };
    parser.skip_whitespace();
    if parser.at_end() {
        return Err(CalcError::Empty);
    }
    let value = parser.expression()?;
    parser.skip_whitespace();
    if !parser.at_end() {
        return Err(CalcError::Unexpected(parser.pos));
    }
    if !value.is_finite() {
        return Err(CalcError::NotFinite);
    }
    Ok(Evaluation {
        value,
        has_operator: parser.has_operator,
    })
}

/// Formats a result for display and the clipboard: integers without a
/// fraction, other values with up to 10 decimals (trailing zeros dropped),
/// very large or very small magnitudes in scientific notation.
pub fn format_value(value: f64) -> String {
    let value = if value == 0.0 { 0.0 } else { value }; // no "-0"
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-6..1e15).contains(&magnitude) {
        return format!("{value:e}");
    }
    if value.fract() == 0.0 {
        return format!("{value:.0}");
    }
    let fixed = format!("{value:.10}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" || trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    /// Current nesting of parentheses and unary signs.
    depth: usize,
    has_operator: bool,
}

/// Deepest nesting accepted; keeps the recursion off the stack limit.
const MAX_DEPTH: usize = 256;

impl Parser<'_> {
    fn enter(&mut self) -> Result<(), CalcError> {
        if self.depth >= MAX_DEPTH {
            return Err(CalcError::TooDeep);
        }
        self.depth += 1;
        Ok(())
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn skip_whitespace(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Next non-whitespace character, normalised (`×` → `*`, …).
    fn peek(&mut self) -> Option<char> {
        self.skip_whitespace();
        self.chars.get(self.pos).map(|&c| match c {
            '×' | 'x' | 'X' => '*',
            '÷' => '/',
            '−' => '-',
            '*' if self.chars.get(self.pos + 1) == Some(&'*') => '^',
            other => other,
        })
    }

    fn bump(&mut self, op: char) {
        // `**` is two characters; everything else one.
        if op == '^' && self.chars.get(self.pos) == Some(&'*') {
            self.pos += 1;
        }
        self.pos += 1;
    }

    /// expression := term (('+' | '-') term)*
    fn expression(&mut self) -> Result<f64, CalcError> {
        let mut value = self.term()?;
        while let Some(op @ ('+' | '-')) = self.peek() {
            self.bump(op);
            self.has_operator = true;
            let rhs = self.term()?;
            value = if op == '+' { value + rhs } else { value - rhs };
        }
        Ok(value)
    }

    /// term := unary (('*' | '/') unary)*
    fn term(&mut self) -> Result<f64, CalcError> {
        let mut value = self.unary()?;
        while let Some(op @ ('*' | '/')) = self.peek() {
            self.bump(op);
            self.has_operator = true;
            let rhs = self.unary()?;
            value = if op == '*' { value * rhs } else { value / rhs };
        }
        Ok(value)
    }

    /// unary := ('+' | '-') unary | power
    fn unary(&mut self) -> Result<f64, CalcError> {
        match self.peek() {
            Some(op @ ('+' | '-')) => {
                self.bump(op);
                self.enter()?;
                let value = self.unary();
                self.depth -= 1;
                let value = value?;
                Ok(if op == '-' { -value } else { value })
            }
            _ => self.power(),
        }
    }

    /// power := atom ('^' unary)?   (right-associative)
    fn power(&mut self) -> Result<f64, CalcError> {
        let base = self.atom()?;
        if self.peek() == Some('^') {
            self.bump('^');
            self.has_operator = true;
            let exponent = self.unary()?;
            return Ok(base.powf(exponent));
        }
        Ok(base)
    }

    /// atom := number | '(' expression ')'
    fn atom(&mut self) -> Result<f64, CalcError> {
        match self.peek() {
            None => Err(CalcError::UnexpectedEnd),
            Some('(') => {
                self.bump('(');
                self.enter()?;
                let value = self.expression();
                self.depth -= 1;
                let value = value?;
                match self.peek() {
                    Some(')') => {
                        self.bump(')');
                        Ok(value)
                    }
                    None => Err(CalcError::UnexpectedEnd),
                    Some(_) => Err(CalcError::Unexpected(self.pos)),
                }
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(_) => Err(CalcError::Unexpected(self.pos)),
        }
    }

    fn number(&mut self) -> Result<f64, CalcError> {
        let start = self.pos;
        let mut seen_dot = false;
        let mut digits = 0;
        while let Some(&c) = self.chars.get(self.pos) {
            if c.is_ascii_digit() {
                digits += 1;
            } else if c == '.' && !seen_dot {
                seen_dot = true;
            } else {
                break;
            }
            self.pos += 1;
        }
        if digits == 0 {
            return Err(CalcError::Unexpected(start));
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse().map_err(|_| CalcError::Unexpected(start))
    }
}

/// What the latest query evaluated to.
enum Outcome {
    Value(String),
    Error,
}

/// The calculator plugin. See the module docs for when it answers.
#[derive(Default)]
pub struct CalcPlugin {
    last: Option<Outcome>,
}

impl CalcPlugin {
    /// Splits off the explicit `=` prefix. Returns the expression and
    /// whether the prefix was present.
    fn expression(query: &str) -> (&str, bool) {
        match query.trim_start().strip_prefix('=') {
            Some(rest) => (rest, true),
            None => (query, false),
        }
    }
}

impl Plugin for CalcPlugin {
    fn name(&self) -> &'static str {
        "calc"
    }

    fn isolates(&self, query: &str) -> bool {
        Self::expression(query).1
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        self.last = None;
        let (expression, explicit) = Self::expression(query);
        let (outcome, name, description) = match evaluate(expression) {
            Ok(evaluation) if explicit || evaluation.has_operator => {
                let value = format_value(evaluation.value);
                let description =
                    format!("{} = {value} · Return copies the result", expression.trim());
                (Outcome::Value(value.clone()), value, description)
            }
            Err(error) if explicit => (Outcome::Error, "Calculator".to_owned(), error.to_string()),
            _ => return Vec::new(),
        };
        self.last = Some(outcome);
        vec![SearchResult {
            id: 0,
            name,
            description,
            icon: Some(IconSource::Name("accessories-calculator-symbolic")),
            window: None,
            rank: Rank::EXACT,
        }]
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        match (&self.last, id) {
            (Some(Outcome::Value(value)), 0) => Some(Action::CopyText(value.clone())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(input: &str) -> f64 {
        evaluate(input)
            .unwrap_or_else(|error| panic!("{input}: {error:?}"))
            .value
    }

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(eval("1 + 2 * 3"), 7.0);
        assert_eq!(eval("(1 + 2) * 3"), 9.0);
        assert_eq!(eval("10 - 4 - 3"), 3.0);
        assert_eq!(eval("2 ^ 3 ^ 2"), 512.0);
        assert_eq!(eval("2**3"), 8.0);
        assert_eq!(eval("-2^2"), -4.0);
        assert_eq!(eval("2^-1"), 0.5);
        assert_eq!(eval("8 / 2 / 2"), 2.0);
        assert_eq!(eval("--3"), 3.0);
        assert_eq!(eval("+4 * -(1 + 1)"), -8.0);
    }

    #[test]
    fn decimals_and_aliases() {
        assert_eq!(eval(".5 + 1."), 1.5);
        assert_eq!(eval("3 × 4 ÷ 2 − 1"), 5.0);
        assert_eq!(eval("3x4"), 12.0);
        assert!((eval("0.1 + 0.2") - 0.3).abs() < 1e-12);
    }

    #[test]
    fn operator_detection() {
        assert!(!evaluate("42").unwrap().has_operator);
        assert!(!evaluate("-3").unwrap().has_operator);
        assert!(!evaluate("(7)").unwrap().has_operator);
        assert!(evaluate("2+2").unwrap().has_operator);
        assert!(evaluate("2^2").unwrap().has_operator);
    }

    #[test]
    fn errors() {
        assert_eq!(evaluate(""), Err(CalcError::Empty));
        assert_eq!(evaluate("   "), Err(CalcError::Empty));
        assert_eq!(evaluate("1 +"), Err(CalcError::UnexpectedEnd));
        assert_eq!(evaluate("(1 + 2"), Err(CalcError::UnexpectedEnd));
        assert_eq!(evaluate("1 + a"), Err(CalcError::Unexpected(4)));
        assert_eq!(evaluate("1 2"), Err(CalcError::Unexpected(2)));
        assert_eq!(evaluate("1..2"), Err(CalcError::Unexpected(2)));
        assert_eq!(evaluate("."), Err(CalcError::Unexpected(0)));
        assert_eq!(evaluate("1 / 0"), Err(CalcError::NotFinite));
        assert_eq!(evaluate("(-1) ^ 0.5"), Err(CalcError::NotFinite));
        assert_eq!(evaluate("safari"), Err(CalcError::Unexpected(0)));
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let parens = format!("{}1{}", "(".repeat(50_000), ")".repeat(50_000));
        assert_eq!(evaluate(&parens), Err(CalcError::TooDeep));
        let minuses = format!("{}1", "-".repeat(50_000));
        assert_eq!(evaluate(&minuses), Err(CalcError::TooDeep));
        // Just inside the limit still works.
        let ok = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        assert_eq!(evaluate(&ok).unwrap().value, 1.0);
        let ok = format!("{}1", "-".repeat(100));
        assert_eq!(evaluate(&ok).unwrap().value, 1.0);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_value(4.0), "4");
        assert_eq!(format_value(-12.0), "-12");
        assert_eq!(format_value(-0.0), "0");
        assert_eq!(format_value(1.5), "1.5");
        assert_eq!(format_value(1.0 / 3.0), "0.3333333333");
        assert_eq!(format_value(0.1 + 0.2), "0.3");
        assert_eq!(format_value(2f64.powi(60)), "1.152921504606847e18");
        assert_eq!(format_value(1e-9), "1e-9");
    }

    #[test]
    fn plugin_answers_explicit_and_implicit_expressions() {
        let mut calc = CalcPlugin::default();
        assert!(calc.isolates("= 1"));
        assert!(calc.isolates("  =1"));
        assert!(!calc.isolates("1+1"));

        let results = calc.search("2+2");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "4");
        assert_eq!(results[0].rank, Rank::EXACT);
        assert_eq!(calc.activate(0), Some(Action::CopyText("4".into())));
        assert_eq!(calc.activate(1), None);

        // A bare number is only a calculation with the explicit prefix.
        assert!(calc.search("42").is_empty());
        assert_eq!(calc.activate(0), None);
        assert_eq!(calc.search("=42")[0].name, "42");

        // Plain words are not calculations.
        assert!(calc.search("safari").is_empty());
    }

    #[test]
    fn explicit_prefix_shows_errors_without_an_action() {
        let mut calc = CalcPlugin::default();
        let results = calc.search("= 1 +");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].description, "incomplete expression");
        assert_eq!(calc.activate(0), None);
        assert!(calc.search("1 +").is_empty());
    }
}
