//! The Slint interface is fully translated: every `@tr("…")` has an English catalog entry
//! with the same placeholders, and no Russian string literal is left outside `@tr()`.

const SOURCES: [(&str, &str); 5] = [
    ("app.slint", include_str!("../ui/app.slint")),
    ("components.slint", include_str!("../ui/components.slint")),
    ("tray_popup.slint", include_str!("../ui/tray_popup.slint")),
    (
        "troubleshoot.slint",
        include_str!("../ui/troubleshoot.slint"),
    ),
    ("theme.slint", include_str!("../ui/theme.slint")),
];

/// Russian literals that are deliberately not translated: the language switch shows each
/// language in itself and keeps a bilingual label, and `а`/`А` is the Russian-layout key of
/// the Ctrl+F shortcut.
const UNTRANSLATED: [&str; 4] = ["Русский", "Язык / Language", "а", "А"];

#[derive(Debug)]
struct Literal {
    /// Byte range of the literal (quotes included) in the source.
    start: usize,
    end: usize,
    value: String,
}

/// String literals outside `//` comments, unescaped.
fn literals(source: &str) -> Vec<Literal> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'"' => {
                let start = index;
                let mut value = String::new();
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' {
                        match bytes.get(index + 1) {
                            Some(b'n') => value.push('\n'),
                            Some(&other) => value.push(other as char),
                            None => {}
                        }
                        index += 2;
                        continue;
                    }
                    let character = source[index..].chars().next().expect("a character");
                    value.push(character);
                    index += character.len_utf8();
                }
                index += 1;
                found.push(Literal {
                    start,
                    end: index,
                    value,
                });
            }
            _ => index += 1,
        }
    }
    found
}

fn is_cyrillic(text: &str) -> bool {
    text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

/// `{}`, `{0}`, `{n}` placeholders of a template, in order.
fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        found.push(rest[start..start + end + 1].to_owned());
        rest = &rest[start + end + 1..];
    }
    found.sort();
    found
}

struct Scan {
    singular: Vec<String>,
    plural: Vec<(String, String)>,
    /// Russian literals outside `@tr()`: (file, literal, a little context before it).
    stray: Vec<(&'static str, String, String)>,
}

fn scan() -> Scan {
    let mut result = Scan {
        singular: Vec::new(),
        plural: Vec::new(),
        stray: Vec::new(),
    };
    for (name, source) in SOURCES {
        let literals = literals(source);
        let mut index = 0;
        while index < literals.len() {
            let literal = &literals[index];
            let before = source[..literal.start].trim_end();
            if before.ends_with("@tr(") {
                let after = source[literal.end..].trim_start();
                if after.starts_with('|') && index + 1 < literals.len() {
                    result
                        .plural
                        .push((literal.value.clone(), literals[index + 1].value.clone()));
                    index += 2;
                } else {
                    result.singular.push(literal.value.clone());
                    index += 1;
                }
                continue;
            }
            index += 1;
            if !is_cyrillic(&literal.value) || UNTRANSLATED.contains(&literal.value.as_str()) {
                continue;
            }
            let after = source[literal.end..].trim_start();
            // State values (the event filter keys) are compared, never shown.
            let state_value = before.ends_with("==")
                || before.ends_with("!=")
                || after.starts_with("==")
                || after.starts_with("!=")
                || before.ends_with("filter-events(")
                || before.ends_with("event-filter:");
            if !state_value {
                let context_start = before.len().saturating_sub(60);
                let context = before
                    .char_indices()
                    .find(|(position, _)| *position >= context_start)
                    .map_or("", |(position, _)| &before[position..]);
                result
                    .stray
                    .push((name, literal.value.clone(), context.to_owned()));
            }
        }
    }
    result
}

#[test]
fn the_scanner_sees_the_interface() {
    let scan = scan();
    assert!(scan.singular.len() > 200, "{}", scan.singular.len());
    assert!(scan.singular.iter().any(|msgid| msgid == "Подключить"));
    assert!(scan.singular.iter().any(|msgid| msgid
        == "Скачаем обновление, не отключая VPN.\nТуннель остановится только после загрузки."));
    assert!(!scan.plural.is_empty());
}

#[test]
fn every_slint_string_has_an_english_translation_with_the_same_placeholders() {
    let scan = scan();
    let mut missing = Vec::new();
    for msgid in &scan.singular {
        match multicore_i18n::english(msgid) {
            Some(english) if placeholders(english) == placeholders(msgid) => {}
            Some(english) => missing.push(format!("placeholders differ: {msgid:?} -> {english:?}")),
            None => missing.push(format!("untranslated: {msgid:?}")),
        }
    }
    for (msgid, plural) in &scan.plural {
        match multicore_i18n::english_plural(msgid, plural) {
            Some((one, other))
                if placeholders(one) == placeholders(msgid)
                    && placeholders(other) == placeholders(plural) => {}
            Some(forms) => missing.push(format!("placeholders differ: {msgid:?} -> {forms:?}")),
            None => missing.push(format!("untranslated plural: {msgid:?}")),
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn english_slint_strings_have_no_russian_left() {
    let scan = scan();
    let russian = scan
        .singular
        .iter()
        .filter_map(|msgid| multicore_i18n::english(msgid))
        .filter(|english| is_cyrillic(english))
        .collect::<Vec<_>>();
    assert!(russian.is_empty(), "{russian:?}");
}

#[test]
fn no_russian_literal_is_left_outside_tr() {
    let stray = scan().stray;
    assert!(
        stray.is_empty(),
        "Russian text outside @tr():\n{}",
        stray
            .iter()
            .map(|(file, literal, context)| format!("{file}: {literal:?} after `{context}`"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
