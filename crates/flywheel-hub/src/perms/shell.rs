//! Just enough shell lexing for permission rules: a command line split into
//! the simple commands Claude Code matches rules against one at a time
//! (`&&`, `||`, `;`, `|`, `|&`, `&`, newlines), each as quote-aware words,
//! with what makes a prefix rule unsafe to derive flagged (command
//! substitution, subshells, heredocs, file redirections).

/// One word of a simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// As written (quotes and escapes kept).
    pub raw: String,
    /// With quotes and escapes removed.
    pub text: String,
    /// Any part of it was quoted or escaped.
    pub quoted: bool,
}

/// One simple command of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Part {
    /// Its words, redirections left out.
    pub words: Vec<Word>,
    /// The command as written (trimmed).
    pub raw: String,
    /// `$(...)`, backticks, `(...)`, `{ ...; }`, or `<(...)`: what runs is
    /// not all in the words.
    pub substitution: bool,
    /// A heredoc (`<<`): its body is part of the command.
    pub heredoc: bool,
    /// Redirected to or from a file (not an fd duplication like `2>&1`).
    pub redirect: bool,
}

/// The lexer's state between characters.
struct Lexer<'a> {
    line: &'a str,
    parts: Vec<Part>,
    part: Part,
    part_start: usize,
    /// The word being read: `(start byte, text, quoted)`.
    word: Option<(usize, String, bool)>,
    /// The previous word was a redirection operator: the next word is its
    /// target, not an argument.
    want_target: bool,
}

impl Lexer<'_> {
    fn word_at(&mut self, at: usize) -> &mut (usize, String, bool) {
        self.word.get_or_insert_with(|| (at, String::new(), false))
    }

    fn finish_word(&mut self, end: usize) {
        if let Some((start, text, quoted)) = self.word.take() {
            if self.want_target {
                self.want_target = false;
            } else {
                let raw = self.line[start..end].to_owned();
                self.part.words.push(Word { raw, text, quoted });
            }
        }
    }

    fn finish_part(&mut self, end: usize, next: usize) {
        self.finish_word(end);
        let mut part = std::mem::take(&mut self.part);
        part.raw = self.line[self.part_start..end].trim().to_owned();
        if !part.raw.is_empty() || !part.words.is_empty() {
            self.parts.push(part);
        }
        self.want_target = false;
        self.part_start = next;
    }
}

/// Split `line` into its simple commands. An unterminated quote is an
/// error (the command cannot be read reliably).
pub fn split(line: &str) -> Result<Vec<Part>, String> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let end_of = |i: usize| chars.get(i).map_or(line.len(), |c| c.0);
    let mut lx = Lexer {
        line,
        parts: Vec::new(),
        part: Part::default(),
        part_start: 0,
        word: None,
        want_target: false,
    };
    let mut i = 0usize;
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|c| c.1);
        match c {
            '\'' => {
                lx.word_at(at).2 = true;
                i += 1;
                loop {
                    let Some(&(_, q)) = chars.get(i) else {
                        return Err("an unterminated ' quote".into());
                    };
                    i += 1;
                    if q == '\'' {
                        break;
                    }
                    lx.word_at(at).1.push(q);
                }
                continue;
            }
            '"' => {
                lx.word_at(at).2 = true;
                i += 1;
                loop {
                    let Some(&(_, q)) = chars.get(i) else {
                        return Err("an unterminated \" quote".into());
                    };
                    i += 1;
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(&(_, e)) = chars.get(i) {
                                i += 1;
                                let w = lx.word_at(at);
                                if !matches!(e, '"' | '\\' | '$' | '`' | '\n') {
                                    w.1.push('\\');
                                }
                                if e != '\n' {
                                    w.1.push(e);
                                }
                            }
                        }
                        '$' if chars.get(i).is_some_and(|c| c.1 == '(') => {
                            lx.part.substitution = true;
                            lx.word_at(at).1.push('$');
                        }
                        '`' => {
                            lx.part.substitution = true;
                            lx.word_at(at).1.push('`');
                        }
                        _ => lx.word_at(at).1.push(q),
                    }
                }
                continue;
            }
            '\\' => {
                let w = lx.word_at(at);
                w.2 = true;
                if let Some(&(_, e)) = chars.get(i + 1) {
                    if e != '\n' {
                        w.1.push(e);
                    }
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            '$' if next == Some('(') => {
                lx.part.substitution = true;
                lx.word_at(at).1.push('$');
            }
            '`' => {
                lx.part.substitution = true;
                lx.word_at(at).1.push('`');
            }
            '(' | ')' | '{' | '}' if lx.word.is_none() => {
                // A subshell or group (`(cd x && y)`, `{ a; }`).
                lx.part.substitution = true;
            }
            ' ' | '\t' => lx.finish_word(at),
            '\n' | ';' => lx.finish_part(at, end_of(i + 1)),
            '&' if next == Some('&') => {
                lx.finish_part(at, end_of(i + 2));
                i += 2;
                continue;
            }
            '|' if next == Some('|') || next == Some('&') => {
                lx.finish_part(at, end_of(i + 2));
                i += 2;
                continue;
            }
            '|' => lx.finish_part(at, end_of(i + 1)),
            '&' if next == Some('>') => {
                // `&>file`, `&>>file`: both streams to a file.
                lx.finish_word(at);
                lx.part.redirect = true;
                lx.want_target = true;
                i += 2;
                if chars.get(i).is_some_and(|c| c.1 == '>') {
                    i += 1;
                }
                continue;
            }
            '&' => lx.finish_part(at, end_of(i + 1)),
            '>' | '<' => {
                // An fd number right before it (`2>`) belongs to the
                // operator, not to the words.
                let fd = matches!(&lx.word, Some((_, text, false))
                    if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()));
                if fd {
                    lx.word = None;
                } else {
                    lx.finish_word(at);
                }
                let mut j = i + 1;
                if c == '<' && chars.get(j).is_some_and(|c| c.1 == '<') {
                    lx.part.heredoc = true;
                    j += 1;
                    if chars.get(j).is_some_and(|c| c.1 == '<') {
                        j += 1; // `<<<` here-string
                    }
                } else if c == '<' && chars.get(j).is_some_and(|c| c.1 == '(') {
                    lx.part.substitution = true;
                } else if c == '>' && chars.get(j).is_some_and(|c| c.1 == '>' || c.1 == '|') {
                    j += 1;
                }
                // `>&2`, `2>&1`, `<&0`: an fd duplication, no file.
                if chars.get(j).is_some_and(|c| c.1 == '&') {
                    j += 1;
                    while chars
                        .get(j)
                        .is_some_and(|c| c.1.is_ascii_digit() || c.1 == '-')
                    {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                if !lx.part.heredoc {
                    lx.part.redirect = true;
                }
                lx.want_target = true;
                i = j;
                continue;
            }
            _ => lx.word_at(at).1.push(c),
        }
        i += 1;
    }
    lx.finish_part(line.len(), line.len());
    Ok(lx.parts)
}

/// `NAME=value`: a variable assignment word.
pub fn is_assignment(word: &Word) -> bool {
    let Some((name, _)) = word.raw.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The leading words Claude Code strips before matching a rule (its fixed
/// wrapper list: `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`,
/// `builtin`, `noglob`, and bare `xargs`): how many there are.
pub fn wrapper_len(words: &[Word]) -> usize {
    wrapper_scan(words).0
}

/// [`wrapper_len`], and whether the next wrapper would take every word
/// left (so nothing after it is the program).
fn wrapper_scan(words: &[Word]) -> (usize, bool) {
    let mut at = 0;
    loop {
        let Some(word) = words.get(at) else {
            return (at, false);
        };
        let consumed = match word.text.as_str() {
            "time" | "nohup" | "builtin" | "noglob" => 1,
            // `command -v` looks a command up; it runs nothing.
            "command" if words.get(at + 1).is_some_and(|w| !w.text.starts_with('-')) => 1,
            "xargs" if words.get(at + 1).is_some_and(|w| !w.text.starts_with('-')) => 1,
            "timeout" => {
                // timeout [OPTION]... DURATION COMMAND
                let mut j = at + 1;
                while let Some(w) = words.get(j) {
                    if !w.text.starts_with('-') {
                        break;
                    }
                    let takes_value =
                        matches!(w.text.as_str(), "-k" | "-s" | "--kill-after" | "--signal");
                    j += if takes_value { 2 } else { 1 };
                }
                if words
                    .get(j)
                    .is_some_and(|w| w.text.starts_with(|c: char| c.is_ascii_digit()))
                {
                    j + 1 - at
                } else {
                    return (at, false);
                }
            }
            "nice" => {
                let mut j = at + 1;
                match words.get(j).map(|w| w.text.as_str()) {
                    Some("-n" | "--adjustment") => j += 2,
                    Some(flag) if flag.starts_with('-') => j += 1,
                    _ => {}
                }
                j - at
            }
            "stdbuf" => {
                let mut j = at + 1;
                while words.get(j).is_some_and(|w| w.text.starts_with('-')) {
                    j += 1;
                }
                j - at
            }
            _ => return (at, false),
        };
        if at + consumed >= words.len() {
            return (at, true);
        }
        at += consumed;
    }
}

/// The wrapper programs [`wrapper_len`] strips.
const WRAPPERS: &[&str] = &[
    "time", "nohup", "builtin", "noglob", "command", "xargs", "timeout", "nice", "stdbuf",
];

/// Whether `words` (a rule's literal words before its wildcard) are only
/// wrappers, so the wildcard stands where the program goes: `timeout 5 *`,
/// `nohup *`, `stdbuf -oL *`, `nice -n 5 *`, `tim*`. `partial`: the last
/// word is cut short by the wildcard.
pub fn only_wrappers(words: &[Word], partial: bool) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    if partial && words.len() == 1 {
        return WRAPPERS.iter().any(|w| w.starts_with(first.text.as_str()));
    }
    if !WRAPPERS.contains(&first.text.as_str()) {
        return false;
    }
    let (at, exhausted) = wrapper_scan(words);
    if exhausted {
        return true;
    }
    let rest = &words[at..];
    let Some(last) = rest.first().map(|w| w.text.as_str()) else {
        return true;
    };
    // The last wrapper and only its own words reach the end (`command -v`
    // looks a command up; it runs nothing).
    WRAPPERS.contains(&last)
        && rest[1..].iter().all(|w| {
            let w = w.text.as_str();
            (w.starts_with('-') || w.starts_with(|c: char| c.is_ascii_digit()))
                && !(last == "command" && matches!(w, "-v" | "-V"))
        })
}

/// The words a rule is matched against: leading wrappers stripped.
pub fn effective_words(words: &[Word]) -> &[Word] {
    &words[wrapper_len(words)..]
}
