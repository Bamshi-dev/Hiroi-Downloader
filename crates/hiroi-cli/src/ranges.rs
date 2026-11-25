//! Numbered batches written into one URL, as curl and other download
//! managers accept them:
//!
//! - `[1-20]` counts, and `[01-20]` keeps the zeros (`01`, `02`, ...).
//! - `[0-100:10]` counts in steps of 10.
//! - `[a-z]` runs through letters.
//! - `{intro,setup,end}` takes each word.
//!
//! Several in one URL give every combination, in order. Brackets that
//! are not a range, such as an IPv6 address (`http://[::1]/`), stay as
//! they are.

use anyhow::{Result, bail};

/// More URLs than this from one pattern is almost surely a typo.
pub const MAX_EXPANDED: usize = 10_000;

/// Every URL `pattern` stands for; the URL itself if it has no range.
pub fn expand(pattern: &str) -> Result<Vec<String>> {
    let parts = parse(pattern)?;
    let total = parts.iter().try_fold(1usize, |total, part| match part {
        Part::Text(_) => Some(total),
        Part::Choice(choices) => total.checked_mul(choices.len()),
    });
    match total {
        Some(total) if total <= MAX_EXPANDED => {}
        _ => bail!("`{pattern}` stands for more than {MAX_EXPANDED} URLs"),
    }
    let mut urls = vec![String::new()];
    for part in &parts {
        urls = match part {
            Part::Text(text) => urls.into_iter().map(|url| url + text).collect(),
            Part::Choice(choices) => urls
                .iter()
                .flat_map(|url| choices.iter().map(move |choice| format!("{url}{choice}")))
                .collect(),
        };
    }
    Ok(urls)
}

enum Part {
    Text(String),
    Choice(Vec<String>),
}

fn parse(pattern: &str) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = pattern;
    while let Some(at) = rest.find(['[', '{']) {
        text.push_str(&rest[..at]);
        let (open, close) = if rest[at..].starts_with('[') {
            ('[', ']')
        } else {
            ('{', '}')
        };
        let inner_start = at + 1;
        let choice = rest[inner_start..].find(close).and_then(|length| {
            let inner = &rest[inner_start..inner_start + length];
            let choices = if open == '[' {
                range(inner)
            } else {
                list(inner)
            }?;
            Some((choices, inner_start + length + 1))
        });
        match choice {
            Some((choices, end)) => {
                parts.push(Part::Text(std::mem::take(&mut text)));
                parts.push(Part::Choice(choices?));
                rest = &rest[end..];
            }
            None => {
                text.push(open);
                rest = &rest[inner_start..];
            }
        }
    }
    text.push_str(rest);
    parts.push(Part::Text(text));
    Ok(parts)
}

/// `1-20`, `01-20`, `0-100:10`, or `a-z`; `None` when `inner` is not a
/// range at all, an error when it is a broken one.
fn range(inner: &str) -> Option<Result<Vec<String>>> {
    let (span, step) = match inner.split_once(':') {
        Some((span, step)) if step.chars().all(|c| c.is_ascii_digit()) => (span, Some(step)),
        Some(_) => return None,
        None => (inner, None),
    };
    let (first, last) = span.split_once('-')?;
    let step: usize = match step.map(str::parse) {
        None => 1,
        Some(Ok(step)) if step > 0 => step,
        Some(_) => {
            return Some(Err(anyhow::anyhow!(
                "a step of [{inner}] must be 1 or more"
            )));
        }
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if digits(first) && digits(last) {
        let (Ok(a), Ok(b)) = (first.parse::<u64>(), last.parse::<u64>()) else {
            return Some(Err(anyhow::anyhow!("[{inner}] is too large")));
        };
        if a > b {
            return Some(Err(anyhow::anyhow!("[{inner}] counts backwards")));
        }
        let width = if first.starts_with('0') && first.len() > 1 {
            first.len()
        } else {
            0
        };
        let count = (b - a) / step as u64 + 1;
        if count > MAX_EXPANDED as u64 {
            return Some(Err(anyhow::anyhow!(
                "[{inner}] stands for more than {MAX_EXPANDED} URLs"
            )));
        }
        let numbers = (a..=b)
            .step_by(step)
            .map(|n| format!("{n:0width$}"))
            .collect();
        return Some(Ok(numbers));
    }
    let letter = |s: &str| {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Some(c),
            _ => None,
        }
    };
    let (a, b) = (letter(first)?, letter(last)?);
    if a.is_ascii_lowercase() != b.is_ascii_lowercase() || a > b {
        return Some(Err(anyhow::anyhow!("[{inner}] is not a range of letters")));
    }
    Some(Ok((a..=b).step_by(step).map(String::from).collect()))
}

/// `intro,setup,end`; `None` without a comma.
fn list(inner: &str) -> Option<Result<Vec<String>>> {
    inner
        .contains(',')
        .then(|| Ok(inner.split(',').map(str::to_string).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_letters_and_lists() {
        assert_eq!(
            expand("https://e.com/part[1-3].rar").unwrap(),
            [
                "https://e.com/part1.rar",
                "https://e.com/part2.rar",
                "https://e.com/part3.rar"
            ]
        );
        assert_eq!(
            expand("https://e.com/v[08-10].mp4").unwrap()[0],
            "https://e.com/v08.mp4"
        );
        assert_eq!(
            expand("https://e.com/v[08-10].mp4").unwrap()[2],
            "https://e.com/v10.mp4"
        );
        assert_eq!(
            expand("https://e.com/[0-20:10]").unwrap(),
            ["https://e.com/0", "https://e.com/10", "https://e.com/20"]
        );
        assert_eq!(expand("https://e.com/[a-c]").unwrap().len(), 3);
        assert_eq!(
            expand("https://e.com/{intro,end}-[1-2].mp4").unwrap(),
            [
                "https://e.com/intro-1.mp4",
                "https://e.com/intro-2.mp4",
                "https://e.com/end-1.mp4",
                "https://e.com/end-2.mp4"
            ]
        );
    }

    #[test]
    fn other_brackets_stay() {
        for url in [
            "http://[::1]:8080/f",
            "https://e.com/a[b]c",
            "https://e.com/{x}",
            "https://e.com/a[1-",
        ] {
            assert_eq!(expand(url).unwrap(), [url]);
        }
    }

    #[test]
    fn broken_ranges_are_refused() {
        for url in [
            "https://e.com/[9-1]",
            "https://e.com/[a-Z]",
            "https://e.com/[1-3:0]",
            "https://e.com/[1-99999]",
        ] {
            assert!(expand(url).is_err(), "{url}");
        }
        assert!(expand("https://e.com/[1-200]/[1-200]").is_err());
    }
}
