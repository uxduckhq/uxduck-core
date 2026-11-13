//! Calculated telemetry (ADR-0056 K3): a person's formula over a vehicle's
//! telemetry items and parameters, read once and worked out whenever one of
//! its items changes. Numbers, item keys such as `battery.voltage_v`, the
//! operators `+ - * / % ^`, comparisons (1 when true, 0 when false), brackets,
//! the functions below and `param("NAME")`. A value it cannot work out (an
//! item with no value, a division by zero) is none, never a guess. No I/O.

use std::fmt;

/// The longest formula, in characters.
pub const MAX_FORMULA_CHARS: usize = 400;
/// How deeply brackets, calls and signs may nest.
const MAX_DEPTH: usize = 32;
/// The longest parameter name MAVLink has.
const MAX_PARAM_CHARS: usize = 16;

/// Why a formula cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormulaError {
    /// For people, a whole sentence.
    pub message: String,
    /// The character it starts at, counting from 1, when it is one place.
    pub at: Option<usize>,
}

impl fmt::Display for FormulaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FormulaError {}

fn error(message: impl Into<String>, at: Option<usize>) -> FormulaError {
    FormulaError {
        message: message.into(),
        at,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Function {
    Abs,
    Min,
    Max,
    Avg,
    Round,
    Floor,
    Ceil,
    Sqrt,
    Clamp,
    If,
}

impl Function {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "abs" => Self::Abs,
            "min" => Self::Min,
            "max" => Self::Max,
            "avg" => Self::Avg,
            "round" => Self::Round,
            "floor" => Self::Floor,
            "ceil" => Self::Ceil,
            "sqrt" => Self::Sqrt,
            "clamp" => Self::Clamp,
            "if" => Self::If,
            _ => return None,
        })
    }

    /// How many values it takes, and how to say so.
    const fn arity(self) -> (usize, usize, &'static str) {
        match self {
            Self::Abs => (1, 1, "abs takes one value, such as abs(speed.climb_mps)."),
            Self::Floor => (1, 1, "floor takes one value."),
            Self::Ceil => (1, 1, "ceil takes one value."),
            Self::Sqrt => (1, 1, "sqrt takes one value."),
            Self::Round => (
                1,
                2,
                "round takes a value and, if you like, a number of decimals, such as round(battery.voltage_v, 1).",
            ),
            Self::Clamp => (
                3,
                3,
                "clamp takes a value, a lowest and a highest, such as clamp(x, 0, 100).",
            ),
            Self::If => (
                3,
                3,
                "if takes a test, a value when it is true and one when it is false, such as if(speed.ground_mps > 1, 1, 0).",
            ),
            Self::Min => (1, usize::MAX, "min takes one or more values."),
            Self::Max => (1, usize::MAX, "max takes one or more values."),
            Self::Avg => (1, usize::MAX, "avg takes one or more values."),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Number(f64),
    Item(String),
    Param(String),
    Neg(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Call(Function, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Name(String),
    Text(String),
    Op(Op),
    Open,
    Close,
    Comma,
}

/// A token and the character it starts at, counting from 1.
type Spanned = (Token, usize);

fn lex(text: &str) -> Result<Vec<Spanned>, FormulaError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = i + 1;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && matches!(chars[i], 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j], '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let digits: String = chars[start..i].iter().collect();
            let number = digits
                .parse::<f64>()
                .map_err(|_| error(format!("“{digits}” is not a number."), Some(at)))?;
            tokens.push((Token::Number(number), at));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            tokens.push((Token::Name(chars[start..i].iter().collect()), at));
            continue;
        }
        if c == '"' || c == '\'' {
            let start = i + 1;
            let Some(end) = chars[start..].iter().position(|q| *q == c) else {
                return Err(error(
                    format!("The text started at character {at} has no closing {c}."),
                    Some(at),
                ));
            };
            tokens.push((Token::Text(chars[start..start + end].iter().collect()), at));
            i = start + end + 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        let (token, width) = match (c, next) {
            ('<', Some('=')) => (Token::Op(Op::Le), 2),
            ('>', Some('=')) => (Token::Op(Op::Ge), 2),
            ('=', Some('=')) => (Token::Op(Op::Eq), 2),
            ('!', Some('=')) => (Token::Op(Op::Ne), 2),
            ('<', _) => (Token::Op(Op::Lt), 1),
            ('>', _) => (Token::Op(Op::Gt), 1),
            ('+', _) => (Token::Op(Op::Add), 1),
            ('-' | '−', _) => (Token::Op(Op::Sub), 1),
            ('*' | '×', _) => (Token::Op(Op::Mul), 1),
            ('/' | '÷', _) => (Token::Op(Op::Div), 1),
            ('%', _) => (Token::Op(Op::Rem), 1),
            ('^', _) => (Token::Op(Op::Pow), 1),
            ('(', _) => (Token::Open, 1),
            (')', _) => (Token::Close, 1),
            (',', _) => (Token::Comma, 1),
            ('=', _) => {
                return Err(error(
                    format!("Compare with == at character {at}, such as gps.fix_type == 3."),
                    Some(at),
                ));
            }
            _ => {
                return Err(error(
                    format!("“{c}” at character {at} is not part of a formula."),
                    Some(at),
                ));
            }
        };
        tokens.push((token, at));
        i += width;
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Spanned>,
    next: usize,
    depth: usize,
    /// Where the formula ends, for a problem at its end.
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.next).map(|(t, _)| t)
    }

    fn at(&self) -> usize {
        self.tokens.get(self.next).map_or(self.end, |(_, at)| *at)
    }

    fn take(&mut self) -> Option<Spanned> {
        let token = self.tokens.get(self.next).cloned();
        self.next += 1;
        token
    }

    fn deeper(&mut self) -> Result<(), FormulaError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(error("This formula nests too deeply.", Some(self.at())));
        }
        Ok(())
    }

    fn comparison(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.additive()?;
        while let Some(Token::Op(op @ (Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne))) =
            self.peek()
        {
            let op = *op;
            self.next += 1;
            let right = self.additive()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.multiplicative()?;
        while let Some(Token::Op(op @ (Op::Add | Op::Sub))) = self.peek() {
            let op = *op;
            self.next += 1;
            let right = self.multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.unary()?;
        while let Some(Token::Op(op @ (Op::Mul | Op::Div | Op::Rem))) = self.peek() {
            let op = *op;
            self.next += 1;
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, FormulaError> {
        match self.peek() {
            Some(Token::Op(Op::Sub)) => {
                self.next += 1;
                self.deeper()?;
                let inner = self.unary()?;
                self.depth -= 1;
                Ok(Expr::Neg(Box::new(inner)))
            }
            Some(Token::Op(Op::Add)) => {
                self.next += 1;
                self.deeper()?;
                let inner = self.unary()?;
                self.depth -= 1;
                Ok(inner)
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Result<Expr, FormulaError> {
        let base = self.primary()?;
        if let Some(Token::Op(Op::Pow)) = self.peek() {
            self.next += 1;
            self.deeper()?;
            let exponent = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Binary(Op::Pow, Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }

    fn primary(&mut self) -> Result<Expr, FormulaError> {
        let at = self.at();
        let Some((token, _)) = self.take() else {
            return Err(error(
                "The formula ends where a number, an item or a bracket should be.",
                Some(at),
            ));
        };
        match token {
            Token::Number(n) => Ok(Expr::Number(n)),
            Token::Open => {
                self.deeper()?;
                let inner = self.comparison()?;
                self.close(at)?;
                self.depth -= 1;
                Ok(inner)
            }
            Token::Name(name) => self.named(&name, at),
            Token::Text(_) => Err(error(
                format!(
                    "Text in quotes at character {at} goes only in param(), such as param(\"BATT_CAPACITY\")."
                ),
                Some(at),
            )),
            Token::Close | Token::Comma | Token::Op(_) => Err(error(
                format!("A number, an item or a bracket should come at character {at}."),
                Some(at),
            )),
        }
    }

    fn close(&mut self, opened: usize) -> Result<(), FormulaError> {
        match self.take() {
            Some((Token::Close, _)) => Ok(()),
            _ => Err(error(
                format!("The bracket at character {opened} is not closed."),
                Some(opened),
            )),
        }
    }

    fn named(&mut self, name: &str, at: usize) -> Result<Expr, FormulaError> {
        let called = matches!(self.peek(), Some(Token::Open));
        if name == "param" {
            if !called {
                return Err(error(
                    "param takes a parameter's name in quotes, such as param(\"BATT_CAPACITY\").",
                    Some(at),
                ));
            }
            self.next += 1;
            let Some((Token::Text(param), _)) = self.take() else {
                return Err(error(
                    "param takes a parameter's name in quotes, such as param(\"BATT_CAPACITY\").",
                    Some(at),
                ));
            };
            self.close(at)?;
            let valid = !param.is_empty()
                && param.chars().count() <= MAX_PARAM_CHARS
                && param
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !valid {
                return Err(error(
                    format!(
                        "“{param}” is not a parameter's name. Names are up to {MAX_PARAM_CHARS} capital letters, digits and _, such as BATT_CAPACITY."
                    ),
                    Some(at),
                ));
            }
            return Ok(Expr::Param(param));
        }
        if called {
            let Some(function) = Function::named(name) else {
                return Err(error(
                    format!(
                        "There is no function called “{name}”. The functions are abs, min, max, avg, round, floor, ceil, sqrt, clamp, if and param."
                    ),
                    Some(at),
                ));
            };
            self.next += 1;
            self.deeper()?;
            let mut args = Vec::new();
            if !matches!(self.peek(), Some(Token::Close)) {
                loop {
                    args.push(self.comparison()?);
                    if matches!(self.peek(), Some(Token::Comma)) {
                        self.next += 1;
                    } else {
                        break;
                    }
                }
            }
            self.close(at)?;
            self.depth -= 1;
            let (fewest, most, usage) = function.arity();
            if args.len() < fewest || args.len() > most {
                return Err(error(usage, Some(at)));
            }
            return Ok(Expr::Call(function, args));
        }
        if Function::named(name).is_some() {
            return Err(error(
                format!("{name} needs its values in brackets, such as {name}(x)."),
                Some(at),
            ));
        }
        item(name, at).map(Expr::Item)
    }
}

/// An item's key as a formula names it: lowercase words joined by dots.
fn item(name: &str, at: usize) -> Result<String, FormulaError> {
    let parameter_like = name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if parameter_like {
        return Err(error(
            format!("“{name}” looks like a parameter. Read it as param(\"{name}\")."),
            Some(at),
        ));
    }
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if !name.contains('.') || !name.split('.').all(word) {
        return Err(error(
            format!(
                "“{name}” is not a telemetry item. Items are named in lowercase with a dot, such as battery.voltage_v."
            ),
            Some(at),
        ));
    }
    if name.starts_with(CALCULATED_GROUP) {
        return Err(error(
            format!(
                "A formula reads what the vehicle sends and what UXDuck works out, not another calculated value such as {name}."
            ),
            Some(at),
        ));
    }
    Ok(name.to_owned())
}

/// The group every calculated value's key is in, such as `custom.power`.
pub const CALCULATED_GROUP: &str = "custom.";

/// A formula, read and ready to work out.
#[derive(Clone, Debug, PartialEq)]
pub struct Formula {
    expr: Expr,
    items: Vec<String>,
    params: Vec<String>,
}

impl Formula {
    /// Reads a formula.
    ///
    /// # Errors
    ///
    /// When it is empty, too long, or not a formula, saying why and where.
    pub fn parse(text: &str) -> Result<Self, FormulaError> {
        if text.trim().is_empty() {
            return Err(error(
                "Write a formula, such as battery.voltage_v * battery.current_a.",
                None,
            ));
        }
        let length = text.chars().count();
        if length > MAX_FORMULA_CHARS {
            return Err(error(
                format!(
                    "A formula is at most {MAX_FORMULA_CHARS} characters, and this is {length}."
                ),
                None,
            ));
        }
        let tokens = lex(text)?;
        let mut parser = Parser {
            tokens,
            next: 0,
            depth: 0,
            end: length + 1,
        };
        let expr = parser.comparison()?;
        if let Some((token, at)) = parser.tokens.get(parser.next) {
            let message = if matches!(token, Token::Close) {
                format!("The bracket at character {at} closes nothing.")
            } else {
                format!("Something is missing before character {at}, such as an operator.")
            };
            return Err(error(message, Some(*at)));
        }
        let mut items = Vec::new();
        let mut params = Vec::new();
        names(&expr, &mut items, &mut params);
        if items.is_empty() {
            return Err(error(
                "A formula reads at least one telemetry item, such as battery.voltage_v.",
                None,
            ));
        }
        Ok(Self {
            expr,
            items,
            params,
        })
    }

    /// The telemetry items it reads, each once, in the order written.
    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// The parameters it reads, each once, in the order written.
    #[must_use]
    pub fn params(&self) -> &[String] {
        &self.params
    }

    /// Works it out from an item's and a parameter's current value; none
    /// when a value it needs is missing or the answer is not a finite number.
    #[must_use]
    pub fn evaluate(
        &self,
        item: &dyn Fn(&str) -> Option<f64>,
        param: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<f64> {
        evaluate(&self.expr, item, param).filter(|v| v.is_finite())
    }
}

fn names(expr: &Expr, items: &mut Vec<String>, params: &mut Vec<String>) {
    match expr {
        Expr::Number(_) => {}
        Expr::Item(key) => {
            if !items.contains(key) {
                items.push(key.clone());
            }
        }
        Expr::Param(name) => {
            if !params.contains(name) {
                params.push(name.clone());
            }
        }
        Expr::Neg(inner) => names(inner, items, params),
        Expr::Binary(_, left, right) => {
            names(left, items, params);
            names(right, items, params);
        }
        Expr::Call(_, args) => {
            for arg in args {
                names(arg, items, params);
            }
        }
    }
}

const fn truth(test: bool) -> f64 {
    if test { 1.0 } else { 0.0 }
}

#[allow(
    clippy::float_cmp,
    reason = "a formula's == and != mean exactly equal, as for a fix type of 3"
)]
fn evaluate(
    expr: &Expr,
    item: &dyn Fn(&str) -> Option<f64>,
    param: &dyn Fn(&str) -> Option<f64>,
) -> Option<f64> {
    let value = |e: &Expr| evaluate(e, item, param);
    let result = match expr {
        Expr::Number(n) => *n,
        Expr::Item(key) => item(key)?,
        Expr::Param(name) => param(name)?,
        Expr::Neg(inner) => -value(inner)?,
        Expr::Binary(op, left, right) => {
            let (a, b) = (value(left)?, value(right)?);
            match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div | Op::Rem if b == 0.0 => return None,
                Op::Div => a / b,
                Op::Rem => a % b,
                Op::Pow => a.powf(b),
                Op::Lt => truth(a < b),
                Op::Le => truth(a <= b),
                Op::Gt => truth(a > b),
                Op::Ge => truth(a >= b),
                Op::Eq => truth(a == b),
                Op::Ne => truth(a != b),
            }
        }
        Expr::Call(Function::If, args) => {
            let test = value(&args[0])?;
            return if test == 0.0 {
                value(&args[2])
            } else {
                value(&args[1])
            };
        }
        Expr::Call(function, args) => {
            let values: Vec<f64> = args.iter().map(value).collect::<Option<_>>()?;
            match function {
                Function::Abs => values[0].abs(),
                Function::Floor => values[0].floor(),
                Function::Ceil => values[0].ceil(),
                Function::Sqrt => values[0].sqrt(),
                Function::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
                Function::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                Function::Avg => {
                    let (sum, count) = values
                        .iter()
                        .fold((0.0, 0.0), |(sum, count), v| (sum + v, count + 1.0));
                    sum / count
                }
                Function::Clamp => {
                    let (lowest, highest) = (values[1], values[2]);
                    if lowest > highest {
                        return None;
                    }
                    values[0].clamp(lowest, highest)
                }
                Function::Round => {
                    let decimals = values.get(1).copied().unwrap_or(0.0);
                    let scale = (0..=6)
                        .find(|n| f64::from(*n) == decimals)
                        .map(|n| 10_f64.powi(n))?;
                    (values[0] * scale).round() / scale
                }
                Function::If => unreachable!("if is worked out above"),
            }
        }
    };
    result.is_finite().then_some(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn work_out(text: &str, items: &[(&str, f64)]) -> Option<f64> {
        let item = |key: &str| items.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let param = |name: &str| (name == "BATT_CAPACITY").then_some(5000.0);
        Formula::parse(text).unwrap().evaluate(&item, &param)
    }

    fn refused(text: &str) -> FormulaError {
        Formula::parse(text).unwrap_err()
    }

    const PACK: [(&str, f64); 3] = [
        ("battery.voltage_v", 16.0),
        ("battery.current_a", 12.5),
        ("battery.consumed_mah", 1200.0),
    ];

    #[test]
    fn works_out_arithmetic_in_the_usual_order() {
        assert_eq!(
            work_out("battery.voltage_v * battery.current_a", &PACK),
            Some(200.0)
        );
        assert_eq!(work_out("battery.voltage_v + 2 * 3", &PACK), Some(22.0));
        assert_eq!(work_out("(battery.voltage_v + 2) * 3", &PACK), Some(54.0));
        assert_eq!(work_out("-battery.voltage_v ^ 2", &PACK), Some(-256.0));
        assert_eq!(
            work_out("2 ^ 3 ^ 2 + battery.voltage_v * 0", &PACK),
            Some(512.0)
        );
        assert_eq!(work_out("battery.voltage_v / 4", &PACK), Some(4.0));
        assert_eq!(work_out("battery.voltage_v % 5", &PACK), Some(1.0));
        assert_eq!(work_out("1.5e1 + battery.voltage_v", &PACK), Some(31.0));
        assert_eq!(work_out("battery.voltage_v × 2 − 1", &PACK), Some(31.0));
    }

    #[test]
    fn compares_as_one_and_zero_and_if_reads_only_its_branch() {
        assert_eq!(work_out("battery.voltage_v > 15", &PACK), Some(1.0));
        assert_eq!(work_out("battery.voltage_v == 15", &PACK), Some(0.0));
        assert_eq!(work_out("battery.voltage_v != 15", &PACK), Some(1.0));
        assert_eq!(
            work_out("if(battery.voltage_v >= 16, 1, gps.hdop)", &PACK),
            Some(1.0),
            "the branch not taken may lack its item"
        );
        assert_eq!(
            work_out("if(battery.voltage_v < 16, 1, gps.hdop)", &PACK),
            None
        );
    }

    #[test]
    fn functions_and_parameters() {
        assert_eq!(work_out("min(battery.voltage_v, 3, 9)", &PACK), Some(3.0));
        assert_eq!(work_out("max(battery.voltage_v, 3)", &PACK), Some(16.0));
        assert_eq!(work_out("avg(battery.voltage_v, 4)", &PACK), Some(10.0));
        assert_eq!(work_out("abs(-battery.voltage_v)", &PACK), Some(16.0));
        assert_eq!(
            work_out("round(battery.voltage_v / 3, 2)", &PACK),
            Some(5.33)
        );
        assert_eq!(work_out("round(battery.voltage_v / 3)", &PACK), Some(5.0));
        assert_eq!(work_out("round(battery.voltage_v, 1.5)", &PACK), None);
        assert_eq!(
            work_out("clamp(battery.voltage_v, 0, 10)", &PACK),
            Some(10.0)
        );
        assert_eq!(work_out("clamp(battery.voltage_v, 10, 0)", &PACK), None);
        assert_eq!(work_out("sqrt(battery.voltage_v)", &PACK), Some(4.0));
        assert_eq!(
            work_out("floor(battery.current_a) + ceil(battery.current_a)", &PACK),
            Some(25.0)
        );
        assert_eq!(
            work_out("param(\"BATT_CAPACITY\") - battery.consumed_mah", &PACK),
            Some(3800.0)
        );
        assert_eq!(
            work_out("param('BATT_CAPACITY') - battery.consumed_mah", &PACK),
            Some(3800.0)
        );
        assert_eq!(
            work_out("param(\"BATT_LOW_MAH\") + battery.voltage_v", &PACK),
            None
        );
    }

    #[test]
    fn what_cannot_be_worked_out_is_none() {
        assert_eq!(work_out("battery.voltage_v / 0", &PACK), None);
        assert_eq!(work_out("battery.voltage_v % 0", &PACK), None);
        assert_eq!(work_out("sqrt(-battery.voltage_v)", &PACK), None);
        assert_eq!(work_out("battery.voltage_v ^ 1000", &PACK), None);
        assert_eq!(work_out("battery.voltage_v * gps.hdop", &PACK), None);
    }

    #[test]
    fn says_what_it_reads() {
        let formula = Formula::parse(
            "param(\"BATT_CAPACITY\") - battery.consumed_mah + battery.consumed_mah * battery_2.cell_1_v",
        )
        .unwrap();
        assert_eq!(
            formula.items(),
            ["battery.consumed_mah", "battery_2.cell_1_v"]
        );
        assert_eq!(formula.params(), ["BATT_CAPACITY"]);
    }

    #[test]
    fn says_why_a_formula_cannot_be_read_and_where() {
        assert_eq!(
            refused("").message,
            "Write a formula, such as battery.voltage_v * battery.current_a."
        );
        assert_eq!(refused("battery.voltage_v *").at, Some(20));
        assert_eq!(
            refused("(battery.voltage_v").message,
            "The bracket at character 1 is not closed."
        );
        assert_eq!(
            refused("battery.voltage_v)").message,
            "The bracket at character 18 closes nothing."
        );
        assert_eq!(
            refused("battery.voltage_v battery.current_a").message,
            "Something is missing before character 19, such as an operator."
        );
        assert_eq!(
            refused("battery.voltage_v = 3").message,
            "Compare with == at character 19, such as gps.fix_type == 3."
        );
        assert!(
            refused("foo(battery.voltage_v)")
                .message
                .starts_with("There is no function called “foo”.")
        );
        assert_eq!(
            refused("BATT_CAPACITY - battery.consumed_mah").message,
            "“BATT_CAPACITY” looks like a parameter. Read it as param(\"BATT_CAPACITY\")."
        );
        assert!(
            refused("voltage * 2")
                .message
                .starts_with("“voltage” is not a telemetry item.")
        );
        assert!(
            refused("custom.power * 2")
                .message
                .contains("not another calculated value")
        );
        assert!(
            refused("3 * 4")
                .message
                .starts_with("A formula reads at least one telemetry item")
        );
        assert!(
            refused("clamp(battery.voltage_v, 1)")
                .message
                .starts_with("clamp takes")
        );
        assert!(
            refused("param(battery.voltage_v)")
                .message
                .starts_with("param takes")
        );
        assert!(
            refused("param(\"batt\") + battery.voltage_v")
                .message
                .starts_with("“batt” is not a parameter's name.")
        );
        assert!(
            refused("battery.voltage_v & 2")
                .message
                .starts_with("“&” at character 19")
        );
        assert!(
            refused("abs + battery.voltage_v")
                .message
                .starts_with("abs needs its values in brackets")
        );
        assert!(
            refused(&"(".repeat(40))
                .message
                .contains("nests too deeply")
        );
        assert!(
            refused(&format!("battery.voltage_v{}", " + 1".repeat(120)))
                .message
                .starts_with("A formula is at most 400 characters")
        );
    }

    #[test]
    fn no_message_has_a_semicolon() {
        for text in [
            "", "(", ")", "=", "x", "X_Y", "param(1)", "if(1)", "foo(1)", "'a",
        ] {
            if let Err(e) = Formula::parse(text) {
                assert!(!e.message.contains(';'), "{}", e.message);
            }
        }
    }
}
