//! Minimal JSON reader, just enough for config.json: objects, arrays,
//! strings, booleans, null and non-negative integers. Writing is done by
//! `config::Config::to_json`.

#[derive(Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Nesting limit so a hostile file cannot overflow the stack.
const MAX_DEPTH: u32 = 32;

pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser { b: text.as_bytes(), i: 0, depth: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

/// Appends `s` as a quoted JSON string.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: u32,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("JSON: {what} at byte {}", self.i)
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.b.get(self.i) == Some(&c);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.b.get(self.i) {
            Some(&c @ (b'{' | b'[')) => {
                if self.depth >= MAX_DEPTH {
                    return Err(self.err("nesting too deep"));
                }
                self.depth += 1;
                self.i += 1;
                let v = if c == b'{' { self.object() } else { self.array() };
                self.depth -= 1;
                v
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("expected a value")),
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        let mut fields = Vec::new();
        if self.eat(b'}') {
            return Ok(Json::Obj(fields));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let key = self.string()?;
            if !self.eat(b':') {
                return Err(self.err("expected ':'"));
            }
            fields.push((key, self.value()?));
            if self.eat(b'}') {
                return Ok(Json::Obj(fields));
            }
            if !self.eat(b',') {
                return Err(self.err("expected ',' or '}'"));
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        let mut items = Vec::new();
        if self.eat(b']') {
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            if self.eat(b']') {
                return Ok(Json::Arr(items));
            }
            if !self.eat(b',') {
                return Err(self.err("expected ',' or ']'"));
            }
        }
    }

    /// Only plain non-negative integers occur in the config, so anything
    /// else (sign, fraction, exponent, leading zero) is rejected.
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.b.get(self.i) {
            self.i += 1;
        }
        let t = &self.b[start..self.i];
        let plain = t.iter().all(u8::is_ascii_digit) && (t.len() == 1 || t[0] != b'0');
        std::str::from_utf8(t)
            .ok()
            .filter(|_| plain)
            .and_then(|s| s.parse().ok())
            .map(Json::Num)
            .ok_or_else(|| self.err("expected a non-negative integer"))
    }

    /// Called with `self.i` on the opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            // The input came from a &str and the run stops at an ASCII byte,
            // so the slice is always valid UTF-8.
            let run = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad UTF-8"))?;
            out.push_str(run);
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    let e = self.b.get(self.i + 1).copied();
                    self.i += 2;
                    out.push(match e {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => self.unicode()?,
                        _ => return Err(self.err("invalid escape")),
                    });
                }
                _ => return Err(self.err("unterminated string or raw control character")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .b
            .get(self.i..self.i + 4)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| self.err("invalid \\u escape"))?;
        self.i += 4;
        Ok(h)
    }

    /// `\uXXXX`, including surrogate pairs. NUL is refused so no string in
    /// the config can carry an interior NUL into a Win32 call.
    fn unicode(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        let cp = if (0xD800..0xDC00).contains(&hi) {
            if self.b.get(self.i..self.i + 2) != Some(b"\\u".as_slice()) {
                return Err(self.err("lone surrogate"));
            }
            self.i += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(self.err("invalid surrogate pair"));
            }
            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            hi
        };
        match char::from_u32(cp) {
            Some(c) if c != '\0' => Ok(c),
            _ => Err(self.err("invalid or NUL \\u escape")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_values() {
        let v = parse(r#" { "a": [1, true, null, "x\"\\\/\n\u00e9\ud83e\udd80"], "b": {} } "#).unwrap();
        assert_eq!(
            v,
            Json::Obj(vec![
                (
                    "a".into(),
                    Json::Arr(vec![
                        Json::Num(1),
                        Json::Bool(true),
                        Json::Null,
                        Json::Str("x\"\\/\né🦀".into())
                    ])
                ),
                ("b".into(), Json::Obj(vec![]))
            ])
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "", "{", "[1,]", "{\"a\" 1}", "{\"a\":1,}", "01", "-1", "1.5", "1e3",
            "\"\\u0000\"", "\"\\ud800\"", "\"a\nb\"", "\"\\x\"", "tru", "1 2",
            "99999999999999999999999",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn limits_depth() {
        assert!(parse(&"[".repeat(100)).is_err());
        assert!(parse(&format!("{}{}", "[".repeat(10), "]".repeat(10))).is_ok());
    }

    #[test]
    fn write_str_roundtrips() {
        let s = "a\"b\\c\n\t\u{1}日本";
        let mut out = String::new();
        write_str(&mut out, s);
        assert_eq!(parse(&out).unwrap(), Json::Str(s.into()));
    }
}
