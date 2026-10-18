//! Strict RFC 8259 JSON reading for model tool-call arguments
//! (`docs/designs/tool-call-records.md`, TCR-D5). One parser serves the
//! argument redactor and the service's typed argument reader. Member order and
//! number lexemes are kept as received. Work is linear in the text length and
//! bounded by [`MAX_DEPTH`] and [`MAX_VALUES`]; no I/O.

/// Nesting depth of arrays and objects.
pub const MAX_DEPTH: usize = 32;
/// Keys plus values: every value (containers included) counts 1, every key 1.
pub const MAX_VALUES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The exact number lexeme as received.
    Number(String),
    String(String),
    Array(Vec<Value>),
    /// Members in received order.
    Object(Vec<(String, Value)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Invalid,
    Limit,
    DuplicateKey,
}

/// Parses one JSON text. `Invalid` and `Limit` stop at the first problem in
/// document order; a duplicate key is remembered and reported only when the
/// rest of the text is valid and within limits.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        at: 0,
        values: 0,
        duplicate: false,
    };
    parser.space();
    let value = parser.value(0)?;
    parser.space();
    if parser.at != parser.bytes.len() {
        return Err(ParseError::Invalid);
    }
    if parser.duplicate {
        return Err(ParseError::DuplicateKey);
    }
    Ok(value)
}

/// Canonical form: member order as received, no whitespace, number lexemes as
/// received; strings escape only `"`, `\` and U+0000–U+001F.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// Writes one canonical JSON string, quotes included.
pub fn write_canonical_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(lexeme) => out.push_str(lexeme),
        Value::String(s) => write_canonical_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
    values: usize,
    duplicate: bool,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }
    fn count(&mut self) -> Result<(), ParseError> {
        self.values += 1;
        if self.values > MAX_VALUES {
            Err(ParseError::Limit)
        } else {
            Ok(())
        }
    }
    fn literal(&mut self, word: &[u8]) -> Result<(), ParseError> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(ParseError::Invalid)
        }
    }
    /// Depth counts enclosing containers: a container at `depth` holds values at `depth + 1`.
    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        let Some(&b) = self.bytes.get(self.at) else {
            return Err(ParseError::Invalid);
        };
        match b {
            b'{' | b'[' => {
                if depth >= MAX_DEPTH {
                    return Err(ParseError::Limit);
                }
                self.count()?;
                if b == b'{' {
                    self.object(depth + 1)
                } else {
                    self.array(depth + 1)
                }
            }
            b'"' => {
                self.count()?;
                Ok(Value::String(self.string()?))
            }
            b't' => {
                self.literal(b"true")?;
                self.count()?;
                Ok(Value::Bool(true))
            }
            b'f' => {
                self.literal(b"false")?;
                self.count()?;
                Ok(Value::Bool(false))
            }
            b'n' => {
                self.literal(b"null")?;
                self.count()?;
                Ok(Value::Null)
            }
            b'-' | b'0'..=b'9' => {
                let lexeme = self.number()?;
                self.count()?;
                Ok(Value::Number(lexeme))
            }
            _ => Err(ParseError::Invalid),
        }
    }
    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        self.at - start
    }
    fn number(&mut self) -> Result<String, ParseError> {
        let start = self.at;
        if self.bytes[self.at] == b'-' {
            self.at += 1;
        }
        match self.bytes.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(ParseError::Invalid),
        }
        if self.bytes.get(self.at) == Some(&b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err(ParseError::Invalid);
            }
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(ParseError::Invalid);
            }
        }
        Ok(self.text[start..self.at].to_owned())
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or(ParseError::Invalid)?;
        let mut n = 0u32;
        for &d in digits {
            n = n * 16 + (d as char).to_digit(16).ok_or(ParseError::Invalid)?;
        }
        self.at += 4;
        Ok(n)
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.at;
            while self
                .bytes
                .get(self.at)
                .is_some_and(|&b| b != b'"' && b != b'\\' && b >= 0x20)
            {
                self.at += 1;
            }
            // Boundaries are ASCII bytes, so the slice is valid UTF-8.
            out.push_str(&self.text[start..self.at]);
            match self.bytes.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escape = *self.bytes.get(self.at).ok_or(ParseError::Invalid)?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if self.bytes.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return Err(ParseError::Invalid);
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(ParseError::Invalid);
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                high
                            };
                            out.push(char::from_u32(code).ok_or(ParseError::Invalid)?);
                        }
                        _ => return Err(ParseError::Invalid),
                    }
                }
                _ => return Err(ParseError::Invalid),
            }
        }
    }
    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b']') {
            self.at += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.space();
            items.push(self.value(depth)?);
            self.space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(ParseError::Invalid),
            }
        }
    }
    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.at += 1;
        let mut members: Vec<(String, Value)> = Vec::new();
        let mut keys = std::collections::HashSet::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.space();
            if self.bytes.get(self.at) != Some(&b'"') {
                return Err(ParseError::Invalid);
            }
            self.count()?;
            let key = self.string()?;
            self.space();
            if self.bytes.get(self.at) != Some(&b':') {
                return Err(ParseError::Invalid);
            }
            self.at += 1;
            self.space();
            let value = self.value(depth)?;
            if !keys.insert(key.clone()) {
                self.duplicate = true;
            }
            members.push((key, value));
            self.space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(ParseError::Invalid),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_accepts_and_rejects() {
        for ok in [
            "{}",
            "[]",
            " {\"a\" : [1, -0, 2.5e+3, true, false, null, \"x\"]} ",
            "\"\\u00e9\\ud83d\\ude00\\/\"",
            "0",
            "-1.0E-2",
        ] {
            assert!(parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "1e",
            "+1",
            "NaN",
            "Infinity",
            "tru",
            "{a:1}",
            "\"\\x\"",
            "\"\u{1}\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\ud800\\u0041\"",
            "1 2",
            "// c\n{}",
            "\"open",
        ] {
            assert_eq!(parse(bad), Err(ParseError::Invalid), "{bad:?}");
        }
    }

    #[test]
    fn duplicates_limits_and_precedence() {
        assert_eq!(parse("{\"a\":1,\"a\":2}"), Err(ParseError::DuplicateKey));
        assert_eq!(parse("{\"a\":1,\"a\":2,}"), Err(ParseError::Invalid));
        let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(&deep(MAX_DEPTH)).is_ok());
        assert_eq!(parse(&deep(MAX_DEPTH + 1)), Err(ParseError::Limit));
        // A limit found first wins over later invalid text.
        assert_eq!(
            parse(&format!("{} x", deep(MAX_DEPTH + 1))),
            Err(ParseError::Limit)
        );
        // 1 array + 4,095 values = 4,096.
        let values = |n: usize| format!("[{}]", vec!["0"; n].join(","));
        assert!(parse(&values(MAX_VALUES - 1)).is_ok());
        assert_eq!(parse(&values(MAX_VALUES)), Err(ParseError::Limit));
        // Keys count: an object with 2,047 members = 1 + 2 × 2,047 = 4,095.
        let members = |n: usize| {
            let items: Vec<_> = (0..n).map(|i| format!("\"k{i}\":0")).collect();
            format!("{{{}}}", items.join(","))
        };
        assert!(parse(&members(2047)).is_ok());
        assert_eq!(parse(&members(2048)), Err(ParseError::Limit));
    }

    #[test]
    fn canonical_keeps_order_and_lexemes_and_escapes_minimally() {
        let value = parse(" { \"b\" : 1.50E+2 , \"a\" : \"\\u0041\\n\\u0001/\\\"é\" } ").unwrap();
        assert_eq!(
            canonical(&value),
            "{\"b\":1.50E+2,\"a\":\"A\\n\\u0001/\\\"é\"}"
        );
        let mut s = String::new();
        write_canonical_string(&mut s, "\u{8}\u{c}\r\t\u{1f}\u{7f}\\");
        assert_eq!(s, "\"\\b\\f\\r\\t\\u001f\u{7f}\\\\\"");
        assert_eq!(parse(&canonical(&value)).unwrap(), value);
    }
}
