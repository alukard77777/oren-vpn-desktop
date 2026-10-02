//! The English catalog covers every string the desktop builds in Rust.
//!
//! A source scan over `src/*.rs`: every `tr!`/`trn!` literal must have an English entry in
//! `translations/en/LC_MESSAGES/multicore-desktop.po`, and no other Cyrillic string literal may
//! remain in code that ships (outside `#[cfg(test)]` items), so nothing stays Russian in
//! English mode. A literal that is matched against rather than shown, or that only the Russian
//! branch of a language-specific rule uses, is marked on its line with `// i18n-ignore: why`
//! or `// i18n-ru-only`.

use std::path::{Path, PathBuf};

/// Files compiled only into tests (their `mod` declaration is `#[cfg(test)]`-gated).
const TEST_ONLY_FILES: &[&str] = &[
    "linux_shell_tests.rs",
    "preview_render.rs",
    "rust_catalog_tests.rs",
];

/// TODO(i18n): translated on their own branches; the coordinator drops them from this list
/// once those are merged.
const CYRILLIC_SCAN_PENDING: &[&str] = &["view_model.rs"];

#[derive(Debug)]
struct Literal {
    line: usize,
    text: String,
    /// `tr!` msgid, first or second `trn!` msgid, or neither.
    role: Role,
    in_test: bool,
    marked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Plain,
    Tr,
    PluralOne,
    PluralOther,
}

fn sources() -> Vec<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = std::fs::read_dir(&directory)
        .expect("desktop sources")
        .map(|entry| entry.expect("source entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// End of the comment, string, raw string or char literal that starts at `index`, if any.
fn token_end(source: &[u8], index: usize) -> Option<usize> {
    let rest = &source[index..];
    if rest.starts_with(b"//") {
        return Some(
            rest.iter()
                .position(|&byte| byte == b'\n')
                .map_or(source.len(), |offset| index + offset),
        );
    }
    if rest.starts_with(b"/*") {
        return Some(
            rest.windows(2)
                .skip(2)
                .position(|pair| pair == b"*/")
                .map_or(source.len(), |offset| index + offset + 4),
        );
    }
    let word_start = |at: usize| at == 0 || !is_ident(source[at - 1]);
    if rest[0] == b'r'
        && (word_start(index) || (index > 0 && source[index - 1] == b'b' && word_start(index - 1)))
    {
        let hashes = rest[1..].iter().take_while(|&&byte| byte == b'#').count();
        if rest.get(1 + hashes) == Some(&b'"') {
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            let body = index + 2 + hashes;
            return Some(
                source[body..]
                    .windows(close.len())
                    .position(|window| window == close.as_slice())
                    .map_or(source.len(), |offset| body + offset + close.len()),
            );
        }
    }
    match rest[0] {
        b'"' => {
            let mut at = index + 1;
            while at < source.len() {
                match source[at] {
                    b'\\' => at += 2,
                    b'"' => return Some(at + 1),
                    _ => at += 1,
                }
            }
            Some(source.len())
        }
        b'\'' => {
            if rest.get(1) == Some(&b'\\') {
                let close = rest[2..].iter().position(|&byte| byte == b'\'')?;
                return Some(index + 2 + close + 1);
            }
            // One UTF-8 character, by its leading byte; otherwise this is a lifetime.
            let width = match rest.get(1).copied().unwrap_or(0) {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            (rest.get(1 + width) == Some(&b'\'')).then_some(index + 2 + width)
        }
        _ => None,
    }
}

/// Byte ranges of items behind `#[cfg(test)]` or `#[cfg(all(test, …))]`.
fn test_spans(source: &[u8]) -> Vec<(usize, usize)> {
    let text = std::str::from_utf8(source).expect("UTF-8 source");
    let mut spans = Vec::new();
    for attribute in ["#[cfg(test)]", "#[cfg(all(test"] {
        let mut from = 0;
        while let Some(offset) = text[from..].find(attribute) {
            let start = from + offset;
            from = start + attribute.len();
            // The item ends at its first `;` before any `{`, or at its matching `}`.
            let mut at = from;
            let mut depth = 0_usize;
            while at < source.len() {
                if let Some(end) = token_end(source, at) {
                    at = end;
                    continue;
                }
                match source[at] {
                    b';' if depth == 0 => break,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            spans.push((start, at + 1));
        }
    }
    spans
}

fn decode(literal: &str) -> String {
    let body = &literal[1..literal.len() - 1];
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('x') => {
                let code = chars.by_ref().take(2).collect::<String>();
                out.push(char::from(u8::from_str_radix(&code, 16).expect("hex byte")));
            }
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('u') => {
                let code = chars
                    .by_ref()
                    .skip(1)
                    .take_while(|&c| c != '}')
                    .collect::<String>();
                out.push(
                    char::from_u32(u32::from_str_radix(&code, 16).expect("hex")).expect("char"),
                );
            }
            Some('\n') | Some('\r') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            other => panic!("unknown escape {other:?} in {literal}"),
        }
    }
    out
}

fn literals(path: &Path) -> Vec<Literal> {
    let raw = std::fs::read_to_string(path).expect("read source");
    let source = raw.replace("\r\n", "\n");
    let bytes = source.as_bytes();
    let spans = test_spans(bytes);
    let mut found = Vec::new();
    let mut index = 0;
    let mut previous_plural_one = false;
    while index < bytes.len() {
        let Some(end) = token_end(bytes, index) else {
            index += 1;
            continue;
        };
        if bytes[index] == b'"' {
            let before = source[..index].trim_end();
            let macro_call = |name: &str| {
                before
                    .strip_suffix(name)
                    .is_some_and(|head| head.as_bytes().last().is_none_or(|&byte| !is_ident(byte)))
            };
            let role = if macro_call("tr!(") {
                Role::Tr
            } else if macro_call("trn!(") {
                Role::PluralOne
            } else if previous_plural_one && before.ends_with(',') {
                Role::PluralOther
            } else {
                Role::Plain
            };
            previous_plural_one = role == Role::PluralOne;
            let line_start = source[..index].rfind('\n').map_or(0, |at| at + 1);
            let line_end = source[index..]
                .find('\n')
                .map_or(source.len(), |at| index + at);
            let line = &source[line_start..line_end];
            found.push(Literal {
                line: source[..index].matches('\n').count() + 1,
                text: decode(&source[index..end]),
                role,
                in_test: spans
                    .iter()
                    .any(|&(start, stop)| (start..stop).contains(&index)),
                marked: line.contains("i18n-ignore") || line.contains("i18n-ru-only"),
            });
        }
        index = end;
    }
    found
}

fn cyrillic(text: &str) -> bool {
    text.chars().any(|c| ('\u{400}'..='\u{4ff}').contains(&c))
}

#[test]
fn every_tr_msgid_has_an_english_translation() {
    let mut missing = Vec::new();
    let mut checked = 0;
    for path in sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let found = literals(&path);
        for (position, literal) in found.iter().enumerate() {
            let known = match literal.role {
                Role::Plain | Role::PluralOther => continue,
                Role::Tr => multicore_i18n::english(&literal.text).is_some(),
                Role::PluralOne => found.get(position + 1).is_some_and(|other| {
                    other.role == Role::PluralOther
                        && multicore_i18n::english_plural(&literal.text, &other.text).is_some()
                }),
            };
            checked += 1;
            if !known {
                missing.push(format!("{file}:{}: {:?}", literal.line, literal.text));
            }
        }
    }
    assert!(checked > 100, "the scan found only {checked} tr! strings");
    assert!(
        missing.is_empty(),
        "no English catalog entry for:\n{}",
        missing.join("\n")
    );
}

#[test]
fn no_russian_literal_ships_outside_the_catalog() {
    let mut untranslated = Vec::new();
    for path in sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        if TEST_ONLY_FILES.contains(&file.as_str())
            || CYRILLIC_SCAN_PENDING.contains(&file.as_str())
        {
            continue;
        }
        for literal in literals(&path) {
            if literal.role == Role::Plain
                && !literal.in_test
                && !literal.marked
                && cyrillic(&literal.text)
            {
                untranslated.push(format!("{file}:{}: {:?}", literal.line, literal.text));
            }
        }
    }
    assert!(
        untranslated.is_empty(),
        "Russian text outside tr!/trn! (wrap it, or mark the line `// i18n-ignore: why`):\n{}",
        untranslated.join("\n")
    );
}

#[test]
fn the_scanner_reads_rust_literals() {
    assert_eq!(decode(r#""a\nb""#), "a\nb");
    assert_eq!(decode("\"a \\\n     b\""), "a b");
    assert_eq!(decode(r#""\u{1f}x""#), "\u{1f}x");
    let source = br##"let x = '"'; let y = "{"; #[cfg(test)] mod t { fn f() { let z = "}"; } } let w = r#"q"#;"##;
    let spans = test_spans(source);
    assert_eq!(spans.len(), 1);
    let text = std::str::from_utf8(source).unwrap();
    assert!(text[spans[0].0..spans[0].1].ends_with("} }"));
}
