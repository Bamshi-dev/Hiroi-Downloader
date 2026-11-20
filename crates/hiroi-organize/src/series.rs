//! Which series a file belongs to, read from its name.
//!
//! `gardening vid 1.mp4`, `Gardening_vid_02.mp4`, and `gardening vid 3 -
//! tomatoes.mkv` are episodes 1, 2, and 3 of `gardening`. A name is read
//! as: the series, an optional marker such as `vid`, `part`, or
//! `lecture`, the number, and anything after it. `S01E02` works too.
//!
//! Numbers that describe the file rather than its place (`1080p`,
//! `x264`, `10bit`, years, long ids) are skipped, and names cameras and
//! programs number on their own (`IMG_0001`, `Untitled 2`) are not
//! series at all.

/// Words before a number that say "part N" and are not the series' name.
const MARKERS: &[&str] = &[
    "vid", "video", "part", "pt", "ep", "episode", "e", "lecture", "lec", "lesson", "chapter",
    "ch", "session", "week", "day", "no", "num", "number", "track", "module", "unit", "clip",
    "disc", "disk", "cd", "vol", "volume", "class", "talk", "tutorial",
];

/// Names that get numbered without being a series.
const GENERIC: &[&str] = &[
    "img",
    "dsc",
    "dscn",
    "dcim",
    "image",
    "photo",
    "pic",
    "pxl",
    "scan",
    "screenshot",
    "document",
    "doc",
    "file",
    "download",
    "untitled",
    "new",
    "copy",
    "video",
    "audio",
    "vid",
    "mov",
    "track",
];

/// Longest number taken as a position; longer ones are dates or ids.
const MAX_NUMBER_DIGITS: usize = 4;

/// One file's place in a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// The series' name as this file writes it, for a folder name.
    pub series: String,
    pub number: u32,
    /// Lowercase letters and digits of the name, for comparing.
    key: String,
}

impl Episode {
    /// Whether two episodes belong to one series. Names may differ in
    /// case, separators, and a typo or two in longer names.
    pub fn same_series(&self, other: &Episode) -> bool {
        self.key == other.key || close(&self.key, &other.key)
    }

    /// Whether `name`, such as a folder's, names this episode's series.
    pub fn is_named(&self, name: &str) -> bool {
        let key = key(name);
        self.key == key || close(&self.key, &key)
    }
}

/// Reads the series and number from a file name, if it has them.
pub fn episode(file_name: &str) -> Option<Episode> {
    let stem = stem(file_name);
    let tokens = tokens(stem);
    let (series_end, number) = season_episode(&tokens).or_else(|| numbered(&tokens))?;
    let mut series = clean(&stem[..series_end]);
    if series.is_empty() {
        // `Lecture 3 - Tomatoes`: the marker is all there is to go by.
        series = marker_before(&tokens, series_end)?.to_string();
    }
    let key = key(&series);
    let letters = key.chars().filter(|c| c.is_alphabetic()).count();
    if key.chars().count() < 3 || letters < 2 || GENERIC.contains(&key.as_str()) {
        return None;
    }
    Some(Episode {
        series,
        number,
        key,
    })
}

/// Lowercase letters and digits, for comparing names.
fn key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Number,
}

/// A run of letters or of digits.
#[derive(Debug)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    start: usize,
    /// No separator between this token and the one before (`ep` and
    /// `01` in `ep01`).
    joined: bool,
}

fn stem(file_name: &str) -> &str {
    let mut stem = file_name;
    while let Some((rest, extension)) = stem.rsplit_once('.') {
        let is_extension = !rest.is_empty()
            && (1..=5).contains(&extension.len())
            && extension.chars().all(|c| c.is_ascii_alphanumeric())
            && !extension.chars().all(|c| c.is_ascii_digit());
        if !is_extension {
            break;
        }
        stem = rest;
        // `.tar.gz` and the like lose both.
        if !rest.to_ascii_lowercase().ends_with(".tar") {
            break;
        }
    }
    stem
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let kind_of = |c: char| {
        if c.is_ascii_digit() {
            Some(Kind::Number)
        } else if c.is_alphabetic() {
            Some(Kind::Word)
        } else {
            None
        }
    };
    // Runs of one kind, as (kind, start, end).
    let mut runs: Vec<(Kind, usize, usize)> = Vec::new();
    let mut open: Option<(Kind, usize)> = None;
    for (index, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let kind = kind_of(c);
        match open {
            Some((current, _)) if Some(current) == kind => continue,
            Some((current, start)) => runs.push((current, start, index)),
            None => {}
        }
        open = kind.map(|kind| (kind, index));
    }
    let mut previous_end = None;
    runs.into_iter()
        .map(|(kind, start, end)| {
            let joined = previous_end == Some(start);
            previous_end = Some(end);
            Token {
                kind,
                text: &text[start..end],
                start,
                joined,
            }
        })
        .collect()
}

/// `S01E02`: season and episode, run together or apart.
fn season_episode(tokens: &[Token<'_>]) -> Option<(usize, u32)> {
    tokens.windows(4).find_map(|run| {
        let [s, season, e, episode] = run else {
            return None;
        };
        let shape = s.kind == Kind::Word
            && s.text.eq_ignore_ascii_case("s")
            && season.kind == Kind::Number
            && season.joined
            && e.kind == Kind::Word
            && e.text.eq_ignore_ascii_case("e")
            && e.joined
            && episode.kind == Kind::Number
            && episode.joined;
        if !shape {
            return None;
        }
        let season: u32 = season.text.parse().ok()?;
        let episode: u32 = episode.text.parse().ok()?;
        Some((s.start, season.checked_mul(1000)?.checked_add(episode)?))
    })
}

/// The number that gives the position: the first one after a marker,
/// otherwise the last one that is not a quality, codec, year, or id.
/// Returns where the series' name ends, and the number.
fn numbered(tokens: &[Token<'_>]) -> Option<(usize, u32)> {
    let positions: Vec<usize> = (0..tokens.len())
        .filter(|&i| is_position(tokens, i))
        .collect();
    let chosen = positions
        .iter()
        .copied()
        .find(|&i| marker_at(tokens, i).is_some())
        .or_else(|| positions.last().copied())?;
    let number = tokens[chosen].text.parse().ok()?;
    let end = marker_at(tokens, chosen).map_or(tokens[chosen].start, |m| tokens[m].start);
    Some((end, number))
}

fn is_position(tokens: &[Token<'_>], i: usize) -> bool {
    let token = &tokens[i];
    if token.kind != Kind::Number || token.text.len() > MAX_NUMBER_DIGITS {
        return false;
    }
    let value: u32 = token.text.parse().unwrap_or(0);
    if token.text.len() == 4 && (1900..=2099).contains(&value) {
        return false;
    }
    let word = |token: Option<&Token<'_>>| {
        token
            .filter(|t| t.kind == Kind::Word)
            .map(|t| t.text.to_ascii_lowercase())
    };
    let after = word(tokens.get(i + 1).filter(|next| next.joined));
    let before = word(
        i.checked_sub(1)
            .map(|p| &tokens[p])
            .filter(|_| token.joined),
    );
    let unit = after.is_some_and(|w| {
        [
            "p", "k", "fps", "bit", "kbps", "mbps", "hz", "khz", "x", "ch",
        ]
        .contains(&w.as_str())
    });
    // `x264`, and a number glued to a name (`CS50`), unless the word is
    // a marker (`ep01`).
    let glued = before.is_some_and(|w| !is_marker(&w));
    !unit && !glued
}

/// The marker word right before the number at `i`, if there is one.
fn marker_at(tokens: &[Token<'_>], i: usize) -> Option<usize> {
    let previous = i.checked_sub(1)?;
    (tokens[previous].kind == Kind::Word && is_marker(tokens[previous].text)).then_some(previous)
}

fn is_marker(word: &str) -> bool {
    MARKERS.contains(&word.to_lowercase().as_str())
}

fn marker_before<'a>(tokens: &[Token<'a>], end: usize) -> Option<&'a str> {
    tokens
        .iter()
        .find(|token| token.start == end && is_marker(token.text))
        .map(|token| token.text)
}

/// The series as a folder name: leading tags like `[Group]` dropped,
/// dots and underscores read as spaces when the name has none, and
/// separators trimmed from the ends.
fn clean(text: &str) -> String {
    let mut text = text.trim();
    while let Some(rest) = text
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map(|(_, rest)| rest)
    {
        text = rest.trim_start();
    }
    let spaced: String = if text.contains(' ') {
        text.to_string()
    } else {
        text.replace(['_', '.'], " ")
    };
    let words: Vec<&str> = spaced.split_whitespace().collect();
    words
        .join(" ")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != ']')
        .to_string()
}

/// Edit distance small enough for a typo: one edit in names of six or
/// more characters, two from twelve. Swapping two neighbours counts as
/// one edit, as typing them in the wrong order is the commonest typo.
fn close(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let shorter = a.len().min(b.len());
    if shorter < 6 || a.first() != b.first() {
        return false;
    }
    let allowed = if shorter >= 12 { 2 } else { 1 };
    if a.len().abs_diff(b.len()) > allowed {
        return false;
    }
    // Optimal string alignment distance; the names are short.
    let width = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * width];
    for i in 0..=a.len() {
        d[i * width] = i;
    }
    for (j, cell) in d.iter_mut().enumerate().take(width) {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[(i - 1) * width + j] + 1)
                .min(d[i * width + j - 1] + 1)
                .min(d[(i - 1) * width + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[(i - 2) * width + j - 2] + 1);
            }
            d[i * width + j] = best;
        }
    }
    d[a.len() * width + b.len()] <= allowed
}
