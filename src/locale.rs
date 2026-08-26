use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use crate::model::WineLocale;

const SUPPORTED_LOCALES: &str = "/usr/share/i18n/SUPPORTED";
const LOCALE_SOURCES: &str = "/usr/share/i18n/locales";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WineLocaleChoice {
    pub locale: WineLocale,
    pub label: String,
}

/// UTF-8 locales that this host can generate for an isolated Wine prefix.
///
/// The catalog is read once because it is host runtime data, not capsule
/// state. Labels deliberately use human-readable language and territory names
/// instead of exposing glibc locale identifiers in the settings UI.
pub fn wine_locale_choices() -> &'static [WineLocaleChoice] {
    static CHOICES: OnceLock<Vec<WineLocaleChoice>> = OnceLock::new();
    CHOICES.get_or_init(load_wine_locale_choices)
}

fn load_wine_locale_choices() -> Vec<WineLocaleChoice> {
    let Ok(supported) = fs::read_to_string(SUPPORTED_LOCALES) else {
        return fallback_choices();
    };

    let mut choices = parse_supported_locale_ids(&supported)
        .into_iter()
        .filter_map(|id| {
            let locale = WineLocale::new(id.clone()).ok()?;
            let source = fs::read_to_string(Path::new(LOCALE_SOURCES).join(&id)).ok()?;
            let label = locale_label(&id, &source)?;
            Some(WineLocaleChoice { locale, label })
        })
        .collect::<Vec<_>>();

    if choices.is_empty() {
        return fallback_choices();
    }
    choices.sort_by(|left, right| {
        left.label
            .to_lowercase()
            .cmp(&right.label.to_lowercase())
            .then_with(|| left.locale.id().cmp(right.locale.id()))
    });
    choices
}

fn parse_supported_locale_ids(contents: &str) -> BTreeSet<String> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let locale = fields.next()?;
            (fields.next()? == "UTF-8")
                .then(|| locale.strip_suffix(".UTF-8").unwrap_or(locale).to_owned())
        })
        .filter(|id| WineLocale::new(id.clone()).is_ok())
        .collect()
}

fn locale_label(id: &str, source: &str) -> Option<String> {
    let language = quoted_field(source, "language")?;
    let territory = quoted_field(source, "territory");
    let mut label = match territory {
        Some(territory) => format!("{language} — {territory}"),
        None => language.to_owned(),
    };
    if let Some((_, modifier)) = id.split_once('@') {
        label.push_str(" (");
        label.push_str(&readable_modifier(modifier));
        label.push(')');
    }
    Some(label)
}

fn quoted_field<'a>(source: &'a str, field: &str) -> Option<&'a str> {
    source.lines().find_map(|line| {
        let value = line.strip_prefix(field)?.trim_start();
        let value = value.strip_prefix('"')?;
        value.split_once('"').map(|(value, _)| value)
    })
}

fn readable_modifier(modifier: &str) -> String {
    modifier
        .split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fallback_choices() -> Vec<WineLocaleChoice> {
    [
        (WineLocale::default(), "English — United States"),
        (WineLocale::japanese(), "Japanese — Japan"),
        (WineLocale::russian(), "Russian — Russia"),
    ]
    .into_iter()
    .map(|(locale, label)| WineLocaleChoice {
        locale,
        label: label.into(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_catalog_keeps_all_safe_utf8_variants() {
        let ids = parse_supported_locale_ids(
            "en_US.UTF-8 UTF-8\n\
             ja_JP.UTF-8 UTF-8\n\
             sr_RS.UTF-8@latin UTF-8\n\
             sr_RS@latin.UTF-8 UTF-8\n\
             ru_RU.KOI8-R KOI8-R\n\
             ../../bad.UTF-8 UTF-8\n",
        );

        assert!(ids.contains("en_US"));
        assert!(ids.contains("ja_JP"));
        assert!(ids.contains("sr_RS@latin"));
        assert!(!ids.contains("ru_RU"));
        assert!(!ids.iter().any(|id| id.contains("..")));
    }

    #[test]
    fn labels_are_readable_and_distinguish_script_variants() {
        let source = "language   \"Serbian\"\nterritory  \"Serbia\"\n";
        assert_eq!(
            locale_label("sr_RS@latin", source).as_deref(),
            Some("Serbian — Serbia (Latin)")
        );
    }

    #[test]
    fn modifier_words_are_humanized() {
        assert_eq!(readable_modifier("latin"), "Latin");
        assert_eq!(readable_modifier("some-script"), "Some Script");
    }
}
