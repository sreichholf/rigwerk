//! Serde helpers for tolerant preference loading.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

/// Reads an optional entry, treating one that does not parse as missing.
/// Used for the stored measurement: a damaged value must not cost the rest
/// of the preferences.
pub(super) fn skip_unreadable_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry<T> {
        Good(T),
        Bad(IgnoredAny),
    }
    let entry = Option::<Entry<T>>::deserialize(deserializer)?;
    Ok(entry.and_then(|entry| match entry {
        Entry::Good(value) => Some(value),
        Entry::Bad(_) => None,
    }))
}

/// Reads a list entry by entry, dropping the entries that do not parse, so
/// one damaged patch or insert does not cost the rest of the preferences.
pub(super) fn skip_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry<T> {
        Good(T),
        Bad(IgnoredAny),
    }
    let entries = Vec::<Entry<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Good(value) => Some(value),
            Entry::Bad(_) => None,
        })
        .collect())
}
