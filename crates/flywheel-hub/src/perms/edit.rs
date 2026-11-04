//! Format-preserving edits of agent config files: rules appended to a
//! Claude Code settings file's `permissions.allow`, and keys set in a
//! Codex `config.toml`. Each edit splices text into the file as written
//! (key order, indentation, other keys untouched) and is then checked by
//! parsing both versions: the result must equal the original with exactly
//! that change, else nothing is written.

use serde_json::Value;

// --- JSON -----------------------------------------------------------------------

/// A JSON value's place in the text.
#[derive(Debug)]
enum Node {
    Object {
        start: usize,
        end: usize,
        /// `(key, key start, value)`.
        members: Vec<(String, usize, Node)>,
    },
    Array {
        start: usize,
        end: usize,
        items: Vec<Node>,
    },
    Scalar {
        start: usize,
        end: usize,
    },
}

impl Node {
    fn end(&self) -> usize {
        match self {
            Node::Object { end, .. } | Node::Array { end, .. } | Node::Scalar { end, .. } => *end,
        }
    }

    fn start(&self) -> usize {
        match self {
            Node::Object { start, .. } | Node::Array { start, .. } | Node::Scalar { start, .. } => {
                *start
            }
        }
    }
}

struct Scanner<'a> {
    text: &'a [u8],
    at: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 64;

impl Scanner<'_> {
    fn ws(&mut self) {
        while self.at < self.text.len()
            && matches!(self.text[self.at], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.at += 1;
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.at;
        self.at += 1;
        while self.at < self.text.len() {
            match self.text[self.at] {
                b'"' => {
                    self.at += 1;
                    let raw = std::str::from_utf8(&self.text[start..self.at])
                        .map_err(|e| e.to_string())?;
                    return serde_json::from_str(raw).map_err(|e| e.to_string());
                }
                b'\\' => self.at += 2,
                _ => self.at += 1,
            }
        }
        Err("an unterminated string".into())
    }

    fn value(&mut self) -> Result<Node, String> {
        self.ws();
        let Some(&c) = self.text.get(self.at) else {
            return Err("unexpected end of file".into());
        };
        match c {
            b'{' | b'[' => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("nested too deeply".into());
                }
                let start = self.at;
                self.at += 1;
                let node = if c == b'{' {
                    let mut members = Vec::new();
                    loop {
                        self.ws();
                        if self.text.get(self.at) == Some(&b'}') && members.is_empty() {
                            break;
                        }
                        if self.text.get(self.at) != Some(&b'"') {
                            return Err(format!("expected a key at byte {}", self.at));
                        }
                        let key_start = self.at;
                        let key = self.string()?;
                        self.ws();
                        if self.text.get(self.at) != Some(&b':') {
                            return Err(format!("expected `:` at byte {}", self.at));
                        }
                        self.at += 1;
                        let value = self.value()?;
                        members.push((key, key_start, value));
                        self.ws();
                        match self.text.get(self.at) {
                            Some(b',') => self.at += 1,
                            Some(b'}') => break,
                            _ => return Err(format!("expected `,` or `}}` at byte {}", self.at)),
                        }
                    }
                    self.at += 1;
                    Node::Object {
                        start,
                        end: self.at,
                        members,
                    }
                } else {
                    let mut items = Vec::new();
                    loop {
                        self.ws();
                        if self.text.get(self.at) == Some(&b']') && items.is_empty() {
                            break;
                        }
                        items.push(self.value()?);
                        self.ws();
                        match self.text.get(self.at) {
                            Some(b',') => self.at += 1,
                            Some(b']') => break,
                            _ => return Err(format!("expected `,` or `]` at byte {}", self.at)),
                        }
                    }
                    self.at += 1;
                    Node::Array {
                        start,
                        end: self.at,
                        items,
                    }
                };
                self.depth -= 1;
                Ok(node)
            }
            b'"' => {
                let start = self.at;
                self.string()?;
                Ok(Node::Scalar {
                    start,
                    end: self.at,
                })
            }
            _ => {
                let start = self.at;
                while self.at < self.text.len()
                    && !matches!(
                        self.text[self.at],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    self.at += 1;
                }
                if start == self.at {
                    return Err(format!("expected a value at byte {start}"));
                }
                Ok(Node::Scalar {
                    start,
                    end: self.at,
                })
            }
        }
    }
}

/// The whitespace a line starts with, for the line holding byte `at`.
fn indent_at(text: &str, at: usize) -> &str {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[line_start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// The file's indent unit: its first nested line's indentation, else two
/// spaces.
fn indent_unit(text: &str, root: &Node) -> String {
    if let Node::Object { members, .. } = root
        && let Some((_, key_start, _)) = members.first()
    {
        let indent = indent_at(text, *key_start);
        if !indent.is_empty() && text[..*key_start].ends_with(indent) {
            return indent.to_owned();
        }
    }
    "  ".to_owned()
}

fn find<'a>(
    members: &'a [(String, usize, Node)],
    key: &str,
) -> Result<Option<&'a (String, usize, Node)>, String> {
    let mut hits = members.iter().filter(|(k, _, _)| k == key);
    let first = hits.next();
    if hits.next().is_some() {
        return Err(format!(
            "the key {key:?} appears twice: fix the file by hand first"
        ));
    }
    Ok(first)
}

/// Insert member text `member` (`"key": value`) into the object at
/// `node`, indented one level past `outer`.
fn insert_member(
    text: &str,
    node: &Node,
    member: &str,
    outer: &str,
    unit: &str,
) -> (usize, String) {
    let Node::Object {
        start,
        end,
        members,
    } = node
    else {
        unreachable!("insert_member on an object");
    };
    let inner = format!("{outer}{unit}");
    match members.last() {
        Some((_, key_start, value)) => {
            let multiline = text[*start..*key_start].contains('\n');
            if multiline {
                let indent = indent_at(text, *key_start);
                (value.end(), format!(",\n{indent}{member}"))
            } else {
                (value.end(), format!(", {member}"))
            }
        }
        None => {
            // `{}` (or `{ }`): fill the inside.
            let _ = end;
            (start + 1, format!("\n{inner}{member}\n{outer}"))
        }
    }
}

/// `text` with `rules` appended to `permissions.allow` (creating either as
/// needed). Rules already in the list are skipped; returns the new text
/// and the rules added.
pub fn add_allow_rules(text: &str, rules: &[String]) -> Result<(String, Vec<String>), String> {
    let original: Value = if text.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(text)
            .map_err(|e| format!("not valid JSON ({e}): fix the file by hand first"))?
    };
    let Value::Object(root_map) = &original else {
        return Err("the file is not a JSON object".into());
    };
    let existing: Vec<String> = match root_map.get("permissions") {
        None => Vec::new(),
        Some(Value::Object(p)) => match p.get("allow") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            Some(_) => return Err("permissions.allow is not a list".into()),
        },
        Some(_) => return Err("permissions is not an object".into()),
    };
    let mut added: Vec<String> = Vec::new();
    for rule in rules {
        if !existing
            .iter()
            .chain(&added)
            .any(|e| super::rule::same(e, rule))
        {
            added.push(rule.clone());
        }
    }
    if added.is_empty() {
        return Ok((text.to_owned(), added));
    }
    let quoted: Vec<String> = added
        .iter()
        .map(|r| serde_json::to_string(r).expect("strings serialize"))
        .collect();
    let new_text = if text.trim().is_empty() {
        let items = quoted
            .iter()
            .map(|q| format!("      {q}"))
            .collect::<Vec<_>>()
            .join(",\n");
        format!("{{\n  \"permissions\": {{\n    \"allow\": [\n{items}\n    ]\n  }}\n}}\n")
    } else {
        let mut scanner = Scanner {
            text: text.as_bytes(),
            at: 0,
            depth: 0,
        };
        let root = scanner.value()?;
        let unit = indent_unit(text, &root);
        let Node::Object { members, .. } = &root else {
            return Err("the file is not a JSON object".into());
        };
        let block = |level: usize| -> String {
            let pad = unit.repeat(level);
            let items = quoted
                .iter()
                .map(|q| format!("{pad}{unit}{q}"))
                .collect::<Vec<_>>()
                .join(",\n");
            format!("[\n{items}\n{pad}]")
        };
        let (at, insert) = match find(members, "permissions")? {
            None => {
                let pad = unit.clone();
                let inner = format!("{pad}{unit}");
                let member = format!(
                    "\"permissions\": {{\n{inner}\"allow\": {}\n{pad}}}",
                    block(2)
                );
                insert_member(text, &root, &member, "", &unit)
            }
            Some((_, key_start, perms)) => {
                let Node::Object { members: pm, .. } = perms else {
                    return Err("permissions is not an object".into());
                };
                let outer = indent_at(text, *key_start).to_owned();
                match find(pm, "allow")? {
                    None => {
                        let level = outer.len() / unit.len().max(1) + 1;
                        let member = format!("\"allow\": {}", block(level));
                        insert_member(text, perms, &member, &outer, &unit)
                    }
                    Some((_, allow_key, allow)) => {
                        let Node::Array { start, end, items } = allow else {
                            return Err("permissions.allow is not a list".into());
                        };
                        let indent = indent_at(text, *allow_key).to_owned();
                        match items.last() {
                            Some(last) => {
                                let multiline = text[*start..*end].contains('\n');
                                let joined = if multiline {
                                    let first = items.first().map_or(*start, Node::start);
                                    let item_indent = if text[..first]
                                        .trim_end_matches([' ', '\t'])
                                        .ends_with('\n')
                                    {
                                        indent_at(text, first).to_owned()
                                    } else {
                                        format!("{indent}{unit}")
                                    };
                                    quoted
                                        .iter()
                                        .map(|q| format!(",\n{item_indent}{q}"))
                                        .collect::<String>()
                                } else {
                                    quoted.iter().map(|q| format!(", {q}")).collect::<String>()
                                };
                                (last.end(), joined)
                            }
                            None => {
                                // `[]`: rewrite it as a block.
                                let level = indent.len() / unit.len().max(1);
                                let new_block = block(level);
                                let mut out = String::with_capacity(text.len() + new_block.len());
                                out.push_str(&text[..*start]);
                                out.push_str(&new_block);
                                out.push_str(&text[*end..]);
                                return verify_json(&original, &out, &added).map(|t| (t, added));
                            }
                        }
                    }
                }
            }
        };
        let mut out = String::with_capacity(text.len() + insert.len());
        out.push_str(&text[..at]);
        out.push_str(&insert);
        out.push_str(&text[at..]);
        out
    };
    let checked = verify_json(&original, &new_text, &added)?;
    Ok((checked, added))
}

/// `new_text` parses to `original` plus `added` at the end of
/// `permissions.allow`, and nothing else changed.
fn verify_json(original: &Value, new_text: &str, added: &[String]) -> Result<String, String> {
    let mut expected = original.clone();
    let root = expected.as_object_mut().expect("checked an object");
    let perms = root
        .entry("permissions")
        .or_insert_with(|| Value::Object(Default::default()));
    let allow = perms
        .as_object_mut()
        .expect("checked an object")
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()));
    let list = allow.as_array_mut().expect("checked a list");
    list.extend(added.iter().cloned().map(Value::String));
    let got: Value = serde_json::from_str(new_text)
        .map_err(|e| format!("the edited file would not be valid JSON ({e})"))?;
    if got != expected {
        return Err(
            "the edit would change more than permissions.allow: edit the file by hand".into(),
        );
    }
    Ok(new_text.to_owned())
}

// --- TOML -----------------------------------------------------------------------

/// A TOML basic string.
pub fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// A table header line's dotted key (`[projects."/a/b"]` -> `["projects",
/// "/a/b"]`); `None` for a line that is not one (or an array of tables).
fn header_key(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    let inner = t.strip_prefix('[')?;
    if inner.starts_with('[') {
        return None;
    }
    let close = inner.rfind(']')?;
    let rest = inner[close + 1..].trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return None;
    }
    let parsed: toml::Table = format!("[{}]\n", &inner[..close]).parse().ok()?;
    let mut path = Vec::new();
    let mut table = &parsed;
    loop {
        let (key, value) = table.iter().next()?;
        path.push(key.clone());
        match value {
            toml::Value::Table(t) if !t.is_empty() => table = t,
            _ => return Some(path),
        }
    }
}

/// Whether a line assigns `key` (`key = ...`, quoted or bare).
fn assigns(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    let quoted = toml_string(key);
    let rest = t
        .strip_prefix(key)
        .or_else(|| t.strip_prefix(quoted.as_str()))
        .or_else(|| t.strip_prefix(&format!("'{key}'")));
    rest.is_some_and(|r| r.trim_start().starts_with('='))
}

/// `text` with `key = value` set in table `table` (`[]`: the top level).
/// `value` is TOML (`"on-request"`). The result is checked: the parsed
/// file must equal the original with that one value set.
pub fn set_toml_key(
    text: &str,
    table: &[String],
    key: &str,
    value: &str,
) -> Result<String, String> {
    let original: toml::Table = text
        .parse()
        .map_err(|e| format!("not valid TOML ({e}): fix the file by hand first"))?;
    let parsed_value: toml::Value = format!("v = {value}")
        .parse::<toml::Table>()
        .map_err(|e| format!("bad value {value}: {e}"))?
        .remove("v")
        .ok_or("bad value")?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let line = format!(
        "{} = {value}\n",
        if key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            key.to_owned()
        } else {
            toml_string(key)
        }
    );
    let headers: Vec<(usize, Option<Vec<String>>)> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with('['))
        .map(|(i, l)| (i, header_key(l)))
        .collect();
    let section = |start: usize| -> usize {
        headers
            .iter()
            .map(|(i, _)| *i)
            .find(|&i| i > start)
            .unwrap_or(lines.len())
    };
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    if let Some(last) = out.last_mut()
        && !last.ends_with('\n')
    {
        last.push('\n');
    }
    let (from, to, insert_at) = if table.is_empty() {
        let end = headers.first().map_or(lines.len(), |(i, _)| *i);
        // Before the first table, after the top level's last line.
        let mut at = end;
        while at > 0 && out[at - 1].trim().is_empty() {
            at -= 1;
        }
        (0, end, at)
    } else {
        match headers.iter().find(|(_, k)| k.as_deref() == Some(table)) {
            Some((i, _)) => (*i + 1, section(*i), *i + 1),
            None => {
                let header = format!(
                    "[{}]\n",
                    table
                        .iter()
                        .map(|k| {
                            if k.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                            {
                                k.clone()
                            } else {
                                toml_string(k)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(".")
                );
                if out.last().is_some_and(|l| !l.trim().is_empty()) {
                    out.push("\n".into());
                }
                out.push(header);
                let at = out.len();
                (at, at, at)
            }
        }
    };
    match (from..to.min(out.len())).find(|&i| assigns(&out[i], key)) {
        Some(i) => out[i] = line,
        None => out.insert(insert_at.min(out.len()), line),
    }
    let new_text: String = out.concat();
    let mut expected = original.clone();
    let mut slot = &mut expected;
    for k in table {
        let entry = slot
            .entry(k.clone())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        slot = entry
            .as_table_mut()
            .ok_or_else(|| format!("{k} is not a table in this file: edit it by hand"))?;
    }
    slot.insert(key.to_owned(), parsed_value);
    let got: toml::Table = new_text
        .parse()
        .map_err(|_| "the file's layout cannot be edited safely (the key is set some other way): edit it by hand".to_owned())?;
    if got != expected {
        return Err("the edit would change more than that one setting (the key is set some other way): edit the file by hand".into());
    }
    Ok(new_text)
}

// --- diff ------------------------------------------------------------------------

/// Lines of context around a change.
const CONTEXT: usize = 3;
/// Larger files get a whole-file diff.
const MAX_DIFF_CELLS: usize = 4_000_000;

/// A unified diff of `old` -> `new` with `label` in its headers.
pub fn unified_diff(label: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        // Nothing changes: no diff at all (not bare headers).
        return String::new();
    }
    let mut out = format!(
        "--- {}\n+++ {label}\n",
        if old.is_empty() { "/dev/null" } else { label }
    );
    // Edit script by LCS (settings files are small).
    let ops: Vec<(char, &str)> = if a.len().saturating_mul(b.len()) > MAX_DIFF_CELLS {
        a.iter()
            .map(|l| ('-', *l))
            .chain(b.iter().map(|l| ('+', *l)))
            .collect()
    } else {
        let (n, m) = (a.len(), b.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        let mut ops = Vec::new();
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                ops.push((' ', a[i]));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                // Removals before additions, as unified diffs show them.
                ops.push(('-', a[i]));
                i += 1;
            } else {
                ops.push(('+', b[j]));
                j += 1;
            }
        }
        ops
    };
    // Group into hunks.
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != ' ')
        .map(|(i, _)| i)
        .collect();
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(CONTEXT);
        let mut end = changed[k];
        while k + 1 < changed.len() && changed[k + 1] <= end + 2 * CONTEXT + 1 {
            k += 1;
            end = changed[k];
        }
        let end = (end + CONTEXT + 1).min(ops.len());
        let a_start = ops[..start].iter().filter(|o| o.0 != '+').count();
        let b_start = ops[..start].iter().filter(|o| o.0 != '-').count();
        let a_len = ops[start..end].iter().filter(|o| o.0 != '+').count();
        let b_len = ops[start..end].iter().filter(|o| o.0 != '-').count();
        let at = |s: usize, len: usize| if len == 0 { s } else { s + 1 };
        out.push_str(&format!(
            "@@ -{},{a_len} +{},{b_len} @@\n",
            at(a_start, a_len),
            at(b_start, b_len)
        ));
        for (op, line) in &ops[start..end] {
            out.push(*op);
            out.push_str(line);
            out.push('\n');
        }
        k += 1;
    }
    out
}
