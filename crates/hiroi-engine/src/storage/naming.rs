use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use reqwest::Url;

use crate::constants::{CONTROL_EXTENSION, MAX_FILE_NAME_BYTES, PART_EXTENSION};

/// Room left for `.hiroi-state.tmp` so temporary names stay within limits.
const RESERVED_SUFFIX_BYTES: usize = 1 + CONTROL_EXTENSION.len() + 4;

/// Extensions treated as one unit when numbering (`a.tar.gz` -> `a (1).tar.gz`).
const DOUBLE_EXTENSIONS: [&str; 5] = [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz"];

/// Turns a name suggested by a server or user into one that is safe to
/// create inside the output directory.
///
/// Only the last path component is kept, so `../../.bashrc` cannot escape the
/// directory. Control characters and characters other systems reject are
/// replaced, a leading dot is prefixed (no hidden files from servers), and
/// the result is shortened to fit file system limits.
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            c if c.is_control() => '_',
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        return None;
    }
    let name = if trimmed.starts_with('.') || is_reserved_on_windows(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    Some(truncate(&name, MAX_FILE_NAME_BYTES - RESERVED_SUFFIX_BYTES))
}

/// Windows refuses files named after devices (`CON`, `NUL`, `COM1`,
/// ...), with any extension. We avoid them everywhere, so a folder of
/// downloads can be copied to Windows as it is.
fn is_reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|rest| {
            matches!(
                rest,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    };
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
}

pub(crate) fn file_name_from_url(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    Some(percent_decode_str(last).decode_utf8_lossy().into_owned())
}

/// `name.ext` -> `name (n).ext`
pub(crate) fn numbered(name: &str, n: u32) -> String {
    let (stem, extension) = split_extension(name);
    format!("{stem} ({n}){extension}")
}

/// The three files that belong to one download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadPaths {
    pub final_path: PathBuf,
    pub part_path: PathBuf,
    pub control_path: PathBuf,
}

impl DownloadPaths {
    pub fn new(dir: &Path, name: &str) -> Self {
        Self {
            final_path: dir.join(name),
            part_path: dir.join(format!("{name}.{PART_EXTENSION}")),
            control_path: dir.join(format!("{name}.{CONTROL_EXTENSION}")),
        }
    }

    pub fn file_name(&self) -> String {
        self.final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn split_extension(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    if let Some(double) = DOUBLE_EXTENSIONS.iter().find(|ext| lower.ends_with(*ext)) {
        let at = name.len() - double.len();
        if at > 0 {
            return name.split_at(at);
        }
    }
    match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    }
}

/// Shortens to `max` bytes on a char boundary, keeping a short extension.
fn truncate(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, extension) = split_extension(name);
    let extension = if extension.len() <= 16 { extension } else { "" };
    let mut cut = max - extension.len();
    while !stem.is_char_boundary(cut.min(stem.len())) {
        cut -= 1;
    }
    format!("{}{extension}", &stem[..cut.min(stem.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_directories_and_traversal() {
        assert_eq!(
            sanitize_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_file_name(r"..\..\boot.ini").as_deref(),
            Some("boot.ini")
        );
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name("dir/"), None);
    }

    #[test]
    fn neutralizes_hidden_and_odd_characters() {
        assert_eq!(sanitize_file_name(".bashrc").as_deref(), Some("_.bashrc"));
        assert_eq!(sanitize_file_name("nul.txt").as_deref(), Some("_nul.txt"));
        assert_eq!(
            sanitize_file_name("COM3.tar.gz").as_deref(),
            Some("_COM3.tar.gz")
        );
        assert_eq!(
            sanitize_file_name("console.log").as_deref(),
            Some("console.log")
        );
        assert_eq!(
            sanitize_file_name("LPT10.bin").as_deref(),
            Some("LPT10.bin")
        );
        assert_eq!(
            sanitize_file_name("a\u{0}b\nc.txt").as_deref(),
            Some("a_b_c.txt")
        );
        assert_eq!(
            sanitize_file_name("what?.txt").as_deref(),
            Some("what_.txt")
        );
        assert_eq!(
            sanitize_file_name("  spaced.iso.  ").as_deref(),
            Some("spaced.iso")
        );
    }

    #[test]
    fn keeps_long_names_within_limits() {
        let long = format!("{}.iso", "é".repeat(300));
        let name = sanitize_file_name(&long).unwrap();
        assert!(name.len() <= MAX_FILE_NAME_BYTES - RESERVED_SUFFIX_BYTES);
        assert!(name.ends_with(".iso"));
    }

    #[test]
    fn numbers_before_the_extension() {
        assert_eq!(numbered("ubuntu.iso", 1), "ubuntu (1).iso");
        assert_eq!(numbered("src.tar.gz", 2), "src (2).tar.gz");
        assert_eq!(numbered("README", 3), "README (3)");
        assert_eq!(numbered(".env", 1), ".env (1)");
    }

    #[test]
    fn name_from_url_is_decoded() {
        let url = Url::parse("https://e.com/dir/my%20file.iso?x=1").unwrap();
        assert_eq!(file_name_from_url(&url).as_deref(), Some("my file.iso"));
        assert_eq!(
            file_name_from_url(&Url::parse("https://e.com/").unwrap()),
            None
        );
    }
}
