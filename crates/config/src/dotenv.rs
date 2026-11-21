//! Разбор `.env` по правилам python-dotenv (им читает файл pydantic-settings в Python-версии).
//!
//! `dotenvy` не подходит: он отвергает значения с пробелами без кавычек
//! (`COORDINATOR_GROUP_NAME=КЦ ИТ Координаторы …`), которые python-dotenv и `docker compose`
//! принимают. Поддерживается то, что встречается в `.env`-файлах проекта и в python-dotenv:
//! * `KEY=value`, `export KEY=value`, пробелы вокруг `=`, комментарии `# …` (целой строкой
//!   и после значения через пробел);
//! * `'одинарные'` (буквально, кроме `\'` и `\\`) и `"двойные"` кавычки (escape-последовательности,
//!   многострочные значения);
//! * подстановка `${VAR}` / `${VAR:-default}` в значениях без кавычек и в двойных кавычках:
//!   сначала из предыдущих строк файла, затем из окружения;
//! * строка без `=` (`KEY`) не задаёт значения; некорректная строка пропускается с предупреждением,
//!   как в python-dotenv.

use std::collections::HashMap;

/// Пары `(ключ, значение)` в порядке появления; `warnings` — пропущенные строки.
#[derive(Debug, Default)]
pub struct Parsed {
    pub vars: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

pub fn parse(input: &str, env: &dyn Fn(&str) -> Option<String>) -> Parsed {
    let mut out = Parsed::default();
    let mut known: HashMap<String, String> = HashMap::new();
    let chars: Vec<char> = input.chars().collect();
    let mut p = Cursor {
        s: &chars,
        i: 0,
        line: 1,
    };

    loop {
        while matches!(p.peek(), Some(c) if c.is_whitespace()) {
            p.bump();
        }
        if p.eof() {
            break;
        }
        let (start_i, start_line) = (p.i, p.line);
        match parse_binding(&mut p) {
            Ok(Some((key, Some((value, interpolate))))) => {
                let value = if interpolate {
                    expand(&value, &known, env)
                } else {
                    value
                };
                known.insert(key.clone(), value.clone());
                out.vars.push((key, value));
            }
            Ok(_) => {}
            Err(msg) => {
                out.warnings.push(format!("line {start_line}: {msg}"));
                // Как python-dotenv: пропускаем только эту строку и продолжаем со следующей.
                p.i = start_i;
                p.line = start_line;
                p.skip_line();
            }
        }
    }
    out
}

struct Cursor<'a> {
    s: &'a [char],
    i: usize,
    line: usize,
}

impl Cursor<'_> {
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c != '\n' && c != '\r' && c.is_whitespace()) {
            self.bump();
        }
    }
    fn skip_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' {
                break;
            }
        }
    }
    fn starts_with(&self, prefix: &str) -> bool {
        prefix
            .chars()
            .enumerate()
            .all(|(k, c)| self.s.get(self.i + k) == Some(&c))
    }
    /// Конец значения: пробелы, необязательный комментарий, конец строки/файла.
    fn expect_eol(&mut self) -> Result<(), String> {
        self.skip_inline_ws();
        if self.peek() == Some('#') {
            self.skip_line();
            return Ok(());
        }
        match self.peek() {
            None => Ok(()),
            Some('\r') | Some('\n') => {
                if self.peek() == Some('\r') {
                    self.bump();
                }
                if self.peek() == Some('\n') {
                    self.bump();
                }
                Ok(())
            }
            Some(c) => Err(format!("unexpected character {c:?} after value")),
        }
    }
}

type Binding = Option<(String, Option<(String, bool)>)>;

fn parse_binding(p: &mut Cursor<'_>) -> Result<Binding, String> {
    // Пустые строки и строки-комментарии.
    while let Some(c) = p.peek() {
        if c.is_whitespace() {
            p.bump();
        } else {
            break;
        }
    }
    match p.peek() {
        None => return Ok(None),
        Some('#') => {
            p.skip_line();
            return Ok(None);
        }
        _ => {}
    }
    if p.starts_with("export") && matches!(p.s.get(p.i + 6), Some(c) if c.is_whitespace() && *c != '\n') {
        p.i += 6;
        p.skip_inline_ws();
    }

    let key = match p.peek() {
        Some(q @ ('\'' | '"')) => {
            p.bump();
            let mut k = String::new();
            loop {
                match p.bump() {
                    Some(c) if c == q => break,
                    Some(c) => k.push(c),
                    None => return Err("unterminated quoted key".into()),
                }
            }
            k
        }
        _ => {
            let mut k = String::new();
            while let Some(c) = p.peek() {
                if c == '=' || c == '#' || c.is_whitespace() {
                    break;
                }
                k.push(c);
                p.bump();
            }
            k
        }
    };
    if key.is_empty() {
        return Err("missing key".into());
    }

    p.skip_inline_ws();
    if p.peek() != Some('=') {
        // `KEY` без значения: python-dotenv возвращает None — переменная не задана.
        p.expect_eol()?;
        return Ok(Some((key, None)));
    }
    p.bump();
    p.skip_inline_ws();

    let value = match p.peek() {
        Some('\'') => {
            p.bump();
            let raw = read_quoted(p, '\'')?;
            p.expect_eol()?;
            (raw.replace("\\'", "'").replace("\\\\", "\\"), false)
        }
        Some('"') => {
            p.bump();
            let raw = read_quoted(p, '"')?;
            p.expect_eol()?;
            (unescape_double(&raw), true)
        }
        _ => {
            let mut raw = String::new();
            while let Some(c) = p.peek() {
                if c == '\n' || c == '\r' {
                    break;
                }
                raw.push(c);
                p.bump();
            }
            if p.peek() == Some('\r') {
                p.bump();
            }
            if p.peek() == Some('\n') {
                p.bump();
            }
            (strip_inline_comment(&raw).trim_end().to_owned(), true)
        }
    };
    Ok(Some((key, Some(value))))
}

/// Содержимое до закрывающей кавычки; `\<кавычка>` не закрывает строку.
fn read_quoted(p: &mut Cursor<'_>, q: char) -> Result<String, String> {
    let mut out = String::new();
    loop {
        match p.bump() {
            None => return Err("unterminated quoted value".into()),
            Some('\\') => {
                out.push('\\');
                if let Some(c) = p.bump() {
                    out.push(c);
                }
            }
            Some(c) if c == q => return Ok(out),
            Some(c) => out.push(c),
        }
    }
}

fn unescape_double(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('v') => out.push('\x0b'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `value  # комментарий` → `value` (комментарий начинается с `#` после пробела).
fn strip_inline_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return &raw[..i];
        }
    }
    raw
}

/// `${VAR}` и `${VAR:-default}`.
fn expand(value: &str, known: &HashMap<String, String>, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, d),
            None => (expr, ""),
        };
        let resolved = known
            .get(name)
            .cloned()
            .or_else(|| env(name))
            .unwrap_or_else(|| default.to_owned());
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(input: &str) -> Vec<(String, String)> {
        let p = parse(input, &|_| None);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        p.vars
    }

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    #[test]
    fn unquoted_with_spaces_and_comments() {
        let input = "# comment\n\nA=КЦ ИТ Координаторы  \nB = 1.0 # inline\nC=a#b\nexport D=x\nE=\n";
        assert_eq!(
            vars(input),
            vec![
                kv("A", "КЦ ИТ Координаторы"),
                kv("B", "1.0"),
                kv("C", "a#b"),
                kv("D", "x"),
                kv("E", "")
            ]
        );
    }

    #[test]
    fn quoted_values() {
        let input =
            "A='raw \\n $x # no comment'\nB=\"l1\\nl2 \\\"q\\\"\" # c\nC=\"multi\nline\"\nD='it\\'s'\n";
        assert_eq!(
            vars(input),
            vec![
                kv("A", "raw \\n $x # no comment"),
                kv("B", "l1\nl2 \"q\""),
                kv("C", "multi\nline"),
                kv("D", "it's")
            ]
        );
    }

    #[test]
    fn json_and_braces_survive() {
        let input = "CORS_ORIGINS=[\"http://localhost:5173\",\"http://localhost:3000\"]\nT={base_url}/incidents/{sysId}\n";
        assert_eq!(
            vars(input),
            vec![
                kv(
                    "CORS_ORIGINS",
                    r#"["http://localhost:5173","http://localhost:3000"]"#
                ),
                kv("T", "{base_url}/incidents/{sysId}"),
            ]
        );
    }

    #[test]
    fn interpolation() {
        let p = parse(
            "U=dash\nURL=postgres://${U}:${PW:-x}@${HOST}/d\nQ='${U}'\n",
            &|k| (k == "HOST").then(|| "db".to_owned()),
        );
        assert_eq!(p.vars[1], kv("URL", "postgres://dash:x@db/d"));
        assert_eq!(p.vars[2], kv("Q", "${U}"));
    }

    #[test]
    fn key_without_value_and_bad_lines() {
        let p = parse("LONELY\nGOOD=1\nBAD='unterminated\nAFTER=2\n", &|_| None);
        assert_eq!(p.vars, vec![kv("GOOD", "1"), kv("AFTER", "2")]);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn crlf() {
        assert_eq!(vars("A=1\r\nB=\"2\"\r\n"), vec![kv("A", "1"), kv("B", "2")]);
    }
}
