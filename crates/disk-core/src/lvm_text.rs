//! Bounded parser for LVM2's on-disk text metadata.
use std::collections::BTreeMap;

pub(super) type Record = BTreeMap<String, Value>;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Value {
    Text(String),
    Number(u64),
    List(Vec<Value>),
    Section(Record),
}

impl Value {
    pub(super) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    pub(super) fn number(&self) -> Option<u64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub(super) fn section(&self) -> Option<&Record> {
        match self {
            Self::Section(fields) => Some(fields),
            _ => None,
        }
    }

    pub(super) fn list(&self) -> Option<&[Value]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

const SYNTAX: &str = "LVM2 文本元数据语法无效、重复或超出限制。";

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
    items: usize,
}

pub(super) fn parse(data: &[u8]) -> Result<Record, String> {
    let end = data.iter().position(|&byte| byte == 0).ok_or(SYNTAX)?;
    if data[end..].iter().any(|&byte| byte != 0) {
        return Err(SYNTAX.into());
    }
    Parser {
        input: &data[..end],
        position: 0,
        items: 0,
    }
    .record(0)
    .map_err(str::to_owned)
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<u8> {
        loop {
            let byte = *self.input.get(self.position)?;
            if byte == b'#' {
                while self.input.get(self.position).is_some_and(|&b| b != b'\n') {
                    self.position += 1;
                }
            } else if byte.is_ascii_whitespace() {
                self.position += 1;
            } else {
                return Some(byte);
            }
        }
    }

    fn account(&mut self) -> Result<(), &'static str> {
        self.items += 1;
        if self.items > 65536 {
            return Err(SYNTAX);
        }
        Ok(())
    }

    fn word(&mut self) -> Result<String, &'static str> {
        let start = self.position;
        while self
            .input
            .get(self.position)
            .is_some_and(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
        {
            self.position += 1;
        }
        if !(1..=255).contains(&(self.position - start)) {
            return Err(SYNTAX);
        }
        Ok(String::from_utf8(self.input[start..self.position].to_vec()).map_err(|_| SYNTAX)?)
    }

    fn quoted(&mut self) -> Result<String, &'static str> {
        self.position += 1;
        let mut text = Vec::new();
        while let Some(&byte) = self.input.get(self.position) {
            self.position += 1;
            match byte {
                b'"' => return String::from_utf8(text).map_err(|_| SYNTAX),
                b'\\' => {
                    let &escaped = self.input.get(self.position).ok_or(SYNTAX)?;
                    self.position += 1;
                    text.push(escaped);
                }
                byte => text.push(byte),
            }
        }
        Err(SYNTAX)
    }

    fn atom(&mut self) -> Result<Value, &'static str> {
        self.account()?;
        if self.peek() == Some(b'"') {
            return self.quoted().map(Value::Text);
        }
        let word = self.word()?;
        Ok(word
            .parse::<u64>()
            .map(Value::Number)
            .unwrap_or(Value::Text(word)))
    }

    fn value(&mut self) -> Result<Value, &'static str> {
        if self.peek() != Some(b'[') {
            return self.atom();
        }
        self.position += 1;
        let mut values = Vec::new();
        while self.peek() != Some(b']') {
            values.push(self.atom()?);
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => break,
                _ => return Err(SYNTAX),
            }
        }
        self.position += 1;
        Ok(Value::List(values))
    }

    fn record(&mut self, depth: usize) -> Result<Record, &'static str> {
        if depth > 8 {
            return Err(SYNTAX);
        }
        let mut fields = BTreeMap::new();
        loop {
            match self.peek() {
                None if depth == 0 => return Ok(fields),
                Some(b'}') if depth != 0 => {
                    self.position += 1;
                    return Ok(fields);
                }
                None | Some(b'}') => return Err(SYNTAX),
                _ => {}
            }
            self.account()?;
            let name = self.word()?;
            let separator = self.peek().ok_or(SYNTAX)?;
            self.position += 1;
            let value = match separator {
                b'{' => Value::Section(self.record(depth + 1)?),
                b'=' => self.value()?,
                _ => return Err(SYNTAX),
            };
            if fields.insert(name, value).is_some() {
                return Err(SYNTAX);
            }
        }
    }
}
