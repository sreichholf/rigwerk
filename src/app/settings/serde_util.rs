//! The placeholder read lives in `audio/serde_util`: `audio` cannot
//! import here.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

/// `IgnoredAny` takes whatever the entry is, so only a type mismatch fails.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry<T> {
    Good(T),
    Bad(IgnoredAny),
}

/// The parse is the only record of which slots were filled, so the
/// load has to recognize the placeholder in the parsed list.
pub(super) trait Placeholder: Default {
    /// The replacement for an entry that does not parse.  It must be safe to
    /// keep if the load never reaches it.
    fn placeholder() -> Self;
}

/// A damaged measurement must not cost the rest of the preferences.
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

/// One damaged session or effect does not cost the rest.
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

/// A skip would move every later path, so a bad entry keeps its slot
/// as [`Placeholder::placeholder`].  RON `()` is an empty struct, not
/// damage: it reads as `T::default()` and stays unmarked.
pub(super) fn placeholder_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Placeholder,
{
    crate::audio::serde_util::read_or_placeholder(deserializer, T::placeholder, T::default)
}
