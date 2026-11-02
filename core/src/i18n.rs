//! Translations for texts made in Rust (toasts, reports, emails, alerts). They come from the same
//! gettext catalogue the Slint UI bundles (`lang/<lang>/LC_MESSAGES/genius-librelinkup.po`), so
//! there is one file to translate. English is the source language.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

static DE_PO: &str = include_str!("../../lang/de/LC_MESSAGES/genius-librelinkup.po");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

impl Lang {
    /// "de", "de-AT", "de_CH.UTF-8" → German; anything else English.
    pub fn from_code(code: &str) -> Lang {
        if code.trim().to_ascii_lowercase().starts_with("de") { Lang::De } else { Lang::En }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

thread_local! {
    /// set by [`with_language`] (a report in another language than the UI)
    static OVERRIDE: RefCell<Option<Lang>> = const { RefCell::new(None) };
}

pub fn set_language(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn language() -> Lang {
    OVERRIDE.with(|o| *o.borrow()).unwrap_or(if CURRENT.load(Ordering::Relaxed) == Lang::De as u8 { Lang::De } else { Lang::En })
}

/// Runs `f` with `lang` as the language of this thread.
pub fn with_language<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    let previous = OVERRIDE.with(|o| o.replace(Some(lang)));
    let r = f();
    OVERRIDE.with(|o| *o.borrow_mut() = previous);
    r
}

struct Catalog {
    singular: HashMap<String, String>,
    /// msgid → (singular, plural)
    plural: HashMap<String, (String, String)>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A small gettext `.po` reader: msgctxt is ignored, fuzzy and empty translations are skipped.
fn parse_po(text: &str) -> Catalog {
    let mut cat = Catalog { singular: HashMap::new(), plural: HashMap::new() };
    let (mut id, mut id_plural, mut strs): (String, Option<String>, Vec<String>) = (String::new(), None, Vec::new());
    let mut fuzzy = false;
    // which field continuation lines ("…") belong to: 0 msgid, 1 msgid_plural, 2+n msgstr[n]
    let mut field = 0usize;
    let mut flush = |id: &mut String, id_plural: &mut Option<String>, strs: &mut Vec<String>, fuzzy: &mut bool| {
        if !id.is_empty() && !*fuzzy {
            match id_plural.take() {
                Some(_) if strs.len() >= 2 && !strs[0].is_empty() => {
                    cat.plural.insert(std::mem::take(id), (strs[0].clone(), strs[1].clone()));
                }
                None if strs.first().is_some_and(|s| !s.is_empty()) => {
                    cat.singular.insert(std::mem::take(id), strs[0].clone());
                }
                _ => {}
            }
        }
        id.clear();
        *id_plural = None;
        strs.clear();
        *fuzzy = false;
    };
    for line in text.lines().map(str::trim) {
        // a blank line or a comment starts the next entry
        if line.is_empty() || line.starts_with('#') {
            if !strs.is_empty() {
                flush(&mut id, &mut id_plural, &mut strs, &mut fuzzy);
            }
            if line.starts_with("#,") && line.contains("fuzzy") {
                fuzzy = true;
            }
            continue;
        }
        if line.starts_with("msgctxt ") {
            if !strs.is_empty() {
                flush(&mut id, &mut id_plural, &mut strs, &mut fuzzy);
            }
            field = usize::MAX;
        } else if let Some(rest) = line.strip_prefix("msgid_plural ") {
            id_plural = Some(unquote(rest));
            field = 1;
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            if !strs.is_empty() {
                flush(&mut id, &mut id_plural, &mut strs, &mut fuzzy);
            }
            id = unquote(rest);
            field = 0;
        } else if let Some(rest) = line.strip_prefix("msgstr[") {
            let (n, value) = rest.split_once(']').unwrap_or(("0", rest));
            let n: usize = n.parse().unwrap_or(0);
            if strs.len() <= n {
                strs.resize(n + 1, String::new());
            }
            strs[n] = unquote(value);
            field = 2 + n;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            strs = vec![unquote(rest)];
            field = 2;
        } else if line.starts_with('"') {
            let more = unquote(line);
            match field {
                0 => id.push_str(&more),
                1 => id_plural.get_or_insert_with(String::new).push_str(&more),
                n if n >= 2 && n != usize::MAX => {
                    if let Some(s) = strs.get_mut(n - 2) {
                        s.push_str(&more);
                    }
                }
                _ => {}
            }
        }
    }
    flush(&mut id, &mut id_plural, &mut strs, &mut fuzzy);
    cat
}

fn german() -> &'static Catalog {
    static CAT: OnceLock<Catalog> = OnceLock::new();
    CAT.get_or_init(|| parse_po(DE_PO))
}

/// The translation of `msgid` in the current language (`msgid` itself when there is none).
pub fn tr(msgid: &'static str) -> &'static str {
    match language() {
        Lang::En => msgid,
        Lang::De => german().singular.get(msgid).map(String::as_str).unwrap_or(msgid),
    }
}

/// Fills `{}` (in order) and `{0}`…`{9}` (by position) with `args`.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(stripped) = after.strip_prefix('{') {
            out.push('{');
            rest = stripped;
            continue;
        }
        match after.find('}') {
            Some(j) => {
                let key = &after[..j];
                let index = if key.is_empty() {
                    next += 1;
                    Some(next - 1)
                } else {
                    key.parse::<usize>().ok()
                };
                match index.and_then(|n| args.get(n)) {
                    Some(arg) => out.push_str(&arg.to_string()),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("}}", "}")
}

/// "1 note" / "3 notes" in the current language: the catalogue has `{n} <noun>` with the plural.
pub fn plural(n: usize, singular: &str, plural: &str) -> String {
    let (one, many) = match language() {
        Lang::En => (singular.to_string(), plural.to_string()),
        Lang::De => german().plural.get(singular).cloned().unwrap_or((singular.to_string(), plural.to_string())),
    };
    (if n == 1 { one } else { many }).replace("{n}", &n.to_string())
}

/// Decimal comma in German: "13.9" → "13,9" wherever a dot stands between two digits.
pub fn numbers(text: &str) -> String {
    if language() != Lang::De {
        return text.to_string();
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        let between_digits = c == '.' && i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        out.push(if between_digits { ',' } else { c });
    }
    out
}

const EN_DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const DE_DAYS: [&str; 7] = ["Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag", "Sonntag"];
const DE_DAYS_SHORT: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];
const EN_MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DE_MONTHS: [&str; 12] =
    ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"];
const DE_MONTHS_SHORT: [&str; 12] = ["Jan", "Feb", "Mär", "Apr", "Mai", "Jun", "Jul", "Aug", "Sep", "Okt", "Nov", "Dez"];

/// Day and month names of a date formatted by chrono (`%a %A %b %B`) in the current language.
pub fn date(formatted: impl Display) -> String {
    let text = formatted.to_string();
    if language() != Lang::De {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let w = std::mem::take(word);
        let full_day = EN_DAYS.iter().position(|d| *d == w).map(|i| DE_DAYS[i]);
        let short_day = EN_DAYS.iter().position(|d| d[..3] == w).map(|i| DE_DAYS_SHORT[i]);
        let full_month = EN_MONTHS.iter().position(|m| *m == w).map(|i| DE_MONTHS[i]);
        let short_month = EN_MONTHS.iter().position(|m| m[..3] == w).map(|i| DE_MONTHS_SHORT[i]);
        out.push_str(full_day.or(short_day).or(full_month).or(short_month).unwrap_or(&w));
    };
    for c in text.chars() {
        if c.is_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// `tr!("text")` translates; `tr!("Imported {} and {}", a, b)` translates and fills in.
#[macro_export]
macro_rules! tr {
    ($msgid:literal) => {
        $crate::i18n::tr($msgid)
    };
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($msgid), &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_po_entries() {
        let po = "msgid \"\"\nmsgstr \"Content-Type: text/plain; charset=UTF-8\\n\"\n\n\
            #: x\nmsgid \"Save\"\nmsgstr \"Speichern\"\n\n\
            msgid \"Long \"\n\"text\"\nmsgstr \"\"\n\"Langer Text\"\n\n\
            #, fuzzy\nmsgid \"Fuzzy\"\nmsgstr \"Unscharf\"\n\n\
            msgid \"Empty\"\nmsgstr \"\"\n\n\
            msgid \"{n} note\"\nmsgid_plural \"{n} notes\"\nmsgstr[0] \"{n} Notiz\"\nmsgstr[1] \"{n} Notizen\"\n";
        let c = parse_po(po);
        assert_eq!(c.singular.get("Save").map(String::as_str), Some("Speichern"));
        assert_eq!(c.singular.get("Long text").map(String::as_str), Some("Langer Text"));
        assert!(!c.singular.contains_key("Fuzzy") && !c.singular.contains_key("Empty") && !c.singular.contains_key(""));
        assert_eq!(c.plural.get("{n} note"), Some(&("{n} Notiz".to_string(), "{n} Notizen".to_string())));
    }

    #[test]
    fn fills_placeholders() {
        assert_eq!(fill("{} and {}", &[&1, &"two"]), "1 and two");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(fill("{{literal}} {}", &[&5]), "{literal} 5");
        assert_eq!(fill("missing {}", &[]), "missing {}");
    }

    #[test]
    fn german_texts_numbers_and_dates() {
        with_language(Lang::De, || {
            assert_eq!(numbers("6.2 mmol/L · 13.9 % · 2026-11-02 12:00"), "6,2 mmol/L · 13,9 % · 2026-11-02 12:00");
            assert_eq!(date("Monday, 02 November 2026"), "Montag, 02 November 2026");
            assert_eq!(date("Tue 03 Mar"), "Di 03 Mär");
            assert_eq!(plural(1, "{n} note", "{n} notes"), "1 Notiz");
            assert_eq!(plural(3, "{n} note", "{n} notes"), "3 Notizen");
            assert_eq!(tr("Save"), "Speichern");
        });
        assert_eq!(language(), Lang::En);
        assert_eq!(numbers("6.2"), "6.2");
        assert_eq!(date("May"), "May");
        assert_eq!(Lang::from_code("de_AT.UTF-8"), Lang::De);
        assert_eq!(Lang::from_code("en-GB"), Lang::En);
    }

    /// Every `{}` / `{0}` in an entry must also be in its translation.
    #[test]
    fn translations_keep_placeholders() {
        let count = |s: &str, p: &str| s.matches(p).count();
        for (id, de) in &german().singular {
            for p in ["{}", "{0}", "{1}", "{2}", "{n}"] {
                assert_eq!(count(id, p), count(de, p), "placeholder {p} in {id:?} → {de:?}");
            }
        }
        assert!(german().singular.len() > 100);
    }
}
