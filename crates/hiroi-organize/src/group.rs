//! Names for groups of downloads added together, when the user gives
//! none.

use crate::series::episode;

/// Longest group name taken from a page title, in characters.
const MAX_TITLE_CHARS: usize = 60;
/// A shared beginning shorter than this says nothing about the files.
const MIN_PREFIX_CHARS: usize = 4;

/// What is known about a batch of downloads.
#[derive(Debug, Default, Clone, Copy)]
pub struct Batch<'a> {
    /// The files' names, as far as they are known.
    pub names: &'a [&'a str],
    /// The title of the page they came from.
    pub title: Option<&'a str>,
    /// The site they came from.
    pub host: Option<&'a str>,
    /// Today, as `YYYY-MM-DD`.
    pub date: &'a str,
}

/// A name for the batch, from the best of: the series most of its files
/// belong to, the beginning their names share, the page's title, and
/// the site with the date.
pub fn group_name(batch: Batch<'_>) -> String {
    series_of(batch.names)
        .or_else(|| shared_prefix(batch.names))
        .or_else(|| batch.title.and_then(title))
        .unwrap_or_else(|| match batch.host {
            Some(host) => format!("{} {}", host.trim_start_matches("www."), batch.date),
            None => format!("Downloads {}", batch.date),
        })
}

/// The series that at least half of the files, and two or more, are in.
fn series_of(names: &[&str]) -> Option<String> {
    let episodes: Vec<_> = names.iter().filter_map(|name| episode(name)).collect();
    episodes
        .iter()
        .map(|candidate| {
            let members = episodes.iter().filter(|e| e.same_series(candidate)).count();
            (members, candidate)
        })
        .filter(|&(members, _)| members >= 2 && members * 2 >= names.len())
        .max_by_key(|&(members, _)| members)
        .map(|(_, episode)| episode.series.clone())
}

/// The words every name starts with.
fn shared_prefix(names: &[&str]) -> Option<String> {
    let [first, rest @ ..] = names else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let words = |name: &'_ str| -> Vec<String> {
        name.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut shared = words(first);
    for name in rest {
        let other = words(name);
        let common = shared
            .iter()
            .zip(&other)
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .count();
        shared.truncate(common);
    }
    // The last word of a single name is often the extension.
    let prefix = shared.join(" ");
    (prefix.chars().count() >= MIN_PREFIX_CHARS).then_some(prefix)
}

/// A page title without the site's name after it (`Lecture notes |
/// Example University`), shortened.
fn title(text: &str) -> Option<String> {
    let main = [" | ", " – ", " — ", " · "]
        .iter()
        .fold(text, |text, separator| {
            text.split(separator).next().unwrap_or(text)
        })
        .trim();
    if main.chars().count() < MIN_PREFIX_CHARS {
        return None;
    }
    let short: String = main.chars().take(MAX_TITLE_CHARS).collect();
    Some(short.trim_end().to_string())
}
