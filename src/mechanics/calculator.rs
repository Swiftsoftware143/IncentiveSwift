//! Safe formula evaluator for the calculator mechanic.
//!
//! Uses a hand-rolled recursive-descent parser restricted to:
//!   +  -  *  /  ( )  numbers  named variables
//!
//! NEVER use eval(), a scripting engine, or std::process::Command here.
//! This is the highest-risk surface in the application.

use std::collections::HashMap;

/// Allowed characters in a safe formula.
/// Restrict to digits, arithmetic operators, parentheses, dot, spaces, and variable placeholders.
fn is_safe_formula(formula: &str) -> bool {
    formula.chars().all(|c| {
        c.is_ascii_digit()
            || c == '+'
            || c == '-'
            || c == '*'
            || c == '/'
            || c == '('
            || c == ')'
            || c == '.'
            || c == ' '
            || c == '{'
            || c == '}'
            || c == '_'
            || c.is_ascii_lowercase()
    })
}

/// Substitute variable placeholders like {monthly_revenue} with their values.
fn substitute_vars(formula: &str, vars: &HashMap<String, f64>) -> String {
    let mut result = formula.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{}}}", key);
        result = result.replace(&placeholder, &value.to_string());
    }
    result
}

#[derive(Debug)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            ' ' => { i += 1; }
            '+' => { tokens.push(Token::Plus); i += 1; }
            '-' => { tokens.push(Token::Minus); i += 1; }
            '*' => { tokens.push(Token::Star); i += 1; }
            '/' => { tokens.push(Token::Slash); i += 1; }
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let num_str: String = chars[start..i].iter().collect();
                let num: f64 = num_str.parse().map_err(|_| format!("Invalid number: {}", num_str))?;
                tokens.push(Token::Number(num));
            }
            c => {
                return Err(format!("Unexpected character '{}' in formula", c));
            }
        }
    }

    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn consume(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        token
    }

    /// parse_expression handles + and -
    fn parse_expression(&mut self) -> Result<f64, String> {
        let mut left = self.parse_term()?;

        while let Some(token) = self.peek() {
            match token {
                Token::Plus => {
                    self.consume();
                    let right = self.parse_term()?;
                    left += right;
                }
                Token::Minus => {
                    self.consume();
                    let right = self.parse_term()?;
                    left -= right;
                }
                _ => break,
            }
        }

        Ok(left)
    }

    /// parse_term handles * and /
    fn parse_term(&mut self) -> Result<f64, String> {
        let mut left = self.parse_factor()?;

        while let Some(token) = self.peek() {
            match token {
                Token::Star => {
                    self.consume();
                    let right = self.parse_factor()?;
                    left *= right;
                }
                Token::Slash => {
                    self.consume();
                    let right = self.parse_factor()?;
                    if right == 0.0 {
                        return Err("Division by zero".to_string());
                    }
                    left /= right;
                }
                _ => break,
            }
        }

        Ok(left)
    }

    /// parse_factor handles numbers and parenthesized expressions
    fn parse_factor(&mut self) -> Result<f64, String> {
        match self.consume() {
            Some(Token::Number(n)) => Ok(n),
            Some(Token::LParen) => {
                let val = self.parse_expression()?;
                match self.consume() {
                    Some(Token::RParen) => Ok(val),
                    _ => Err("Expected closing parenthesis".to_string()),
                }
            }
            Some(Token::Minus) => {
                // Unary minus
                let val = self.parse_factor()?;
                Ok(-val)
            }
            _ => Err("Expected number or parenthesis".to_string()),
        }
    }
}

/// Evaluate a formula string safely.
///
/// # Arguments
/// * `formula` - A formula string like "{monthly_revenue} * 0.1 + 500"
/// * `vars` - A map of variable names to their numeric values
///
/// # Returns
/// The computed f64 value, or an error if the formula is invalid or contains unsafe characters.
pub fn evaluate(formula: &str, vars: &HashMap<String, f64>) -> Result<f64, String> {
    // First pass: check for unsafe characters
    if !is_safe_formula(formula) {
        return Err("Formula contains unsafe characters".to_string());
    }

    // Substitute variables
    let substituted = substitute_vars(formula, vars);

    // Tokenize and parse
    let tokens = tokenize(&substituted)?;
    let mut parser = Parser::new(tokens);
    let result = parser.parse_expression()?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_addition() {
        let vars = HashMap::new();
        assert_eq!(evaluate("2 + 3", &vars).unwrap(), 5.0);
    }

    #[test]
    fn test_multiplication_before_addition() {
        let vars = HashMap::new();
        assert_eq!(evaluate("2 + 3 * 4", &vars).unwrap(), 14.0);
    }

    #[test]
    fn test_parentheses() {
        let vars = HashMap::new();
        assert_eq!(evaluate("(2 + 3) * 4", &vars).unwrap(), 20.0);
    }

    #[test]
    fn test_variable_substitution() {
        let mut vars = HashMap::new();
        vars.insert("monthly_revenue".to_string(), 10000.0);
        assert_eq!(evaluate("{monthly_revenue} * 0.1", &vars).unwrap(), 1000.0);
    }

    #[test]
    fn test_division() {
        let vars = HashMap::new();
        assert_eq!(evaluate("10 / 2", &vars).unwrap(), 5.0);
    }

    #[test]
    fn test_division_by_zero() {
        let vars = HashMap::new();
        assert!(evaluate("10 / 0", &vars).is_err());
    }

    #[test]
    fn test_unsafe_chars_rejected() {
        let vars = HashMap::new();
        assert!(evaluate("2 + eval('danger')", &vars).is_err());
    }

    #[test]
    fn test_unary_minus() {
        let vars = HashMap::new();
        assert_eq!(evaluate("-5 + 3", &vars).unwrap(), -2.0);
    }

    #[test]
    fn test_complex_formula() {
        let mut vars = HashMap::new();
        vars.insert("leads".to_string(), 50.0);
        vars.insert("rate".to_string(), 0.25);
        let formula = "{leads} * {rate} + 10";
        assert!((evaluate(formula, &vars).unwrap() - 22.5).abs() < 1e-10);
    }
}
