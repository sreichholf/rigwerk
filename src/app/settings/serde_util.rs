//! Serde helpers for tolerant preference loading.
//!
//! The placeholder read itself lives in `audio/serde_util`: the saved device
//! rows need the same read, and `audio` may not reach in here.  The
//! [`Placeholder`] values and the policy that reports a replacement stay in
//! this layer.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

/// One stored list entry, readable or not.  `IgnoredAny` takes whatever the
/// entry is, so only the type mismatch fails.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry<T> {
    Good(T),
    Bad(IgnoredAny),
}

/// A value [`placeholder_unreadable`] leaves where a stored entry could not be
/// read.  The load has to recognize it in the parsed list, because the parse
/// itself is the only record of which slots it filled.
pub(super) trait Placeholder: Default {
    /// The replacement for an entry that does not parse.  It must be safe to
    /// keep if the load never reaches it.
    fn placeholder() -> Self;
}

/// Reads an optional entry, treating one that does not parse as missing.
/// Used for the stored measurement: a damaged value must not cost the rest
/// of the preferences.
pub(super) fn skip_unreadable_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let entry = Option::<Entry<T>>::deserialize(deserializer)?;
    Ok(entry.and_then(|entry| match entry {
        Entry::Good(value) => Some(value),
        Entry::Bad(_) => None,
    }))
}

/// Reads a list entry by entry, dropping the entries that do not parse, so
/// one damaged session or effect does not cost the rest of the preferences.
pub(super) fn skip_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let entries = Vec::<Entry<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Good(value) => Some(value),
            Entry::Bad(_) => None,
        })
        .collect())
}

/// Reads a list entry by entry, replacing the entries that do not parse with
/// [`Placeholder::placeholder`] in their own slot.  For a list whose positions
/// carry the meaning a skip would be a silent edit: dropping one path would
/// move every later path.  The replacement keeps the index and marks itself,
/// so the load reports it from the same parse.
///
/// RON's `()` is an entry written as an empty struct, not damage: it reads as
/// `T::default()` and stays unmarked, so it raises no notice and no backup.
pub(super) fn placeholder_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Placeholder,
{
    crate::audio::serde_util::read_or_placeholder(deserializer, T::placeholder, T::default)
}
