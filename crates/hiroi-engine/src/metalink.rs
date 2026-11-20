//! Metalink files (RFC 5854 `.meta4`, and the older version 3
//! `.metalink`): a list of files, each with its size, its hashes, and
//! the servers that have it.
//!
//! A Metalink is not what the user wants on disk; it describes what
//! they want. We read it, and each file it lists becomes a download of
//! its own: the servers become mirrors, in the order the file prefers,
//! and the strongest hash becomes the checksum, so a server that sends
//! a tampered file is caught.

use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Url;

use crate::checksum::{Checksum, HashAlgorithm, PieceHashes};
use crate::error::{EngineError, Result};
use crate::net::probe::RemoteFile;
use crate::storage::naming::sanitize_file_name;
use crate::task::{self, TaskContext};
use crate::xml::{self, Element};

/// Largest Metalink we read; real ones are a few kilobytes.
const MAX_METALINK: usize = 4 * 1024 * 1024;
/// Most files we take from one Metalink.
const MAX_FILES: usize = 1000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MetaFile {
    /// Relative path, made safe: no `..`, no absolute paths.
    pub path: Vec<String>,
    pub size: Option<u64>,
    pub checksum: Option<Checksum>,
    pub pieces: Option<PieceHashes>,
    /// Best first.
    pub urls: Vec<Url>,
}

pub(crate) fn detect(remote: &RemoteFile) -> bool {
    let content_type = remote.content_type.as_deref().unwrap_or("");
    if content_type.contains("metalink") {
        return true;
    }
    let path = remote.final_url.path().to_ascii_lowercase();
    path.ends_with(".meta4") || path.ends_with(".metalink")
}

pub(crate) fn parse(text: &str) -> Result<Vec<MetaFile>> {
    let invalid = |reason: &str| EngineError::Protocol(format!("bad Metalink: {reason}"));
    let document = xml::parse(text).map_err(|e| invalid(&e))?;
    let root = document
        .child("metalink")
        .ok_or_else(|| invalid("no metalink element"))?;
    // Version 3 wraps files, hashes, and URLs in extra elements.
    let files = root.children("file").chain(
        root.child("files")
            .into_iter()
            .flat_map(|f| f.children("file")),
    );
    let mut parsed = Vec::new();
    for file in files.take(MAX_FILES) {
        let Some(path) = file.attr("name").and_then(safe_path) else {
            tracing::warn!(name = ?file.attr("name"), "skipping a Metalink file with an unusable name");
            continue;
        };
        let size = file.child("size").and_then(|s| s.text.trim().parse().ok());
        let checksum = strongest_hash(file);
        let pieces = strongest_pieces(file);
        let urls = urls(file);
        if urls.is_empty() {
            tracing::warn!(name = %path.join("/"), "skipping a Metalink file without sources we can use");
            continue;
        }
        parsed.push(MetaFile {
            path,
            size,
            checksum,
            pieces,
            urls,
        });
    }
    if parsed.is_empty() {
        return Err(invalid("no file we can download"));
    }
    Ok(parsed)
}

/// `dir/name.iso` as path components. `.` and `..` are dropped, so a
/// Metalink can never write outside the download folder.
fn safe_path(name: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = name
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .map(sanitize_file_name)
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn strongest_hash(file: &Element) -> Option<Checksum> {
    let hashes: Vec<&Element> = file
        .children("hash")
        .chain(
            file.child("verification")
                .into_iter()
                .flat_map(|v| v.children("hash")),
        )
        .collect();
    ["sha-512", "sha-256", "sha-1", "md5"]
        .iter()
        .find_map(|wanted| {
            hashes.iter().find_map(|hash| {
                let kind = hash.attr("type")?.to_ascii_lowercase();
                if kind.replace('-', "") != wanted.replace('-', "") {
                    return None;
                }
                format!("{wanted}:{}", hash.text.trim()).parse().ok()
            })
        })
}

/// The piece hashes with the strongest algorithm. Version 4 puts
/// `<pieces>` in the file, version 3 inside `<verification>`; both list
/// one `<hash>` per piece.
fn strongest_pieces(file: &Element) -> Option<PieceHashes> {
    let lists = file.children("pieces").chain(
        file.child("verification")
            .into_iter()
            .flat_map(|v| v.children("pieces")),
    );
    lists
        .filter_map(|list| {
            let algorithm = HashAlgorithm::from_name(list.attr("type")?)?;
            let length = list.attr("length")?.trim().parse().ok()?;
            let mut hashes: Vec<(usize, String)> = list
                .children("hash")
                .enumerate()
                .map(|(order, hash)| {
                    let index = hash
                        .attr("piece")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(order);
                    (index, hash.text.trim().to_ascii_lowercase())
                })
                .collect();
            hashes.sort_by_key(|(index, _)| *index);
            let hashes = hashes.into_iter().map(|(_, hash)| hash).collect();
            Some(PieceHashes {
                algorithm,
                length,
                hashes,
            })
        })
        .max_by_key(|pieces| strength(pieces.algorithm))
}

fn strength(algorithm: HashAlgorithm) -> u8 {
    match algorithm {
        HashAlgorithm::Md5 => 0,
        HashAlgorithm::Sha1 => 1,
        HashAlgorithm::Sha256 => 2,
        HashAlgorithm::Sha512 => 3,
    }
}

/// Sources this build can use, best first. Version 4 ranks by `priority`
/// (1 is best); version 3 by `preference` (100 is best).
fn urls(file: &Element) -> Vec<Url> {
    let mut ranked: Vec<(u32, Url)> = file
        .children("url")
        .chain(
            file.child("resources")
                .into_iter()
                .flat_map(|r| r.children("url")),
        )
        .filter_map(|url| {
            let parsed = Url::parse(url.text.trim()).ok()?;
            if !crate::request::is_supported(parsed.scheme()) {
                return None;
            }
            let rank = match (url.attr("priority"), url.attr("preference")) {
                (Some(priority), _) => priority.parse().unwrap_or(u32::MAX),
                (None, Some(preference)) => 101u32.saturating_sub(preference.parse().unwrap_or(0)),
                (None, None) => u32::MAX,
            };
            Some((rank, parsed))
        })
        .collect();
    // Stable, so equal ranks keep the file's order.
    ranked.sort_by_key(|(rank, _)| *rank);
    let mut urls: Vec<Url> = Vec::new();
    for (_, url) in ranked {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// Downloads every file of the Metalink at `url`, one after the other.
/// The job's file is the first; the others sit next to it.
pub(crate) async fn download(ctx: &Arc<TaskContext>, url: &Url) -> Result<PathBuf> {
    let body = crate::media::fetch_small(ctx, url, MAX_METALINK).await?;
    let text = String::from_utf8(body)
        .map_err(|_| EngineError::Protocol("Metalink is not UTF-8 text".into()))?;
    let files = parse(&text)?;
    tracing::info!(files = files.len(), "following a Metalink");
    let single = files.len() == 1;
    let mut first = None;
    for file in files {
        let mut request = (*ctx.request).clone();
        let (name, folders) = file.path.split_last().expect("safe_path keeps one part");
        request.output_dir = folders
            .iter()
            .fold(request.output_dir.clone(), |dir, folder| dir.join(folder));
        // The user's own name and checksum only make sense for one file.
        if !single || request.file_name.is_none() {
            request.file_name = Some(name.clone());
        }
        if !single || request.checksum.is_none() {
            request.checksum = file.checksum.clone();
        }
        request.sources = file.urls;
        request.pieces = file.pieces.map(Arc::new);
        let path = Box::pin(task::run(Arc::new(ctx.derive(request)))).await?;
        first.get_or_insert(path);
    }
    first.ok_or_else(|| EngineError::Protocol("empty Metalink".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256_OF_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn version_4_files_rank_their_sources() {
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <metalink xmlns="urn:ietf:params:xml:ns:metalink">
              <file name="disk.iso">
                <size>3</size>
                <hash type="md5">900150983cd24fb0d6963f7d28e17f72</hash>
                <hash type="sha-256">{SHA256_OF_ABC}</hash>
                <url priority="2">https://two.example/disk.iso</url>
                <url priority="1">https://one.example/disk.iso</url>
                <url priority="1">ftp://old.example/disk.iso</url>
                <metaurl mediatype="torrent">https://one.example/disk.torrent</metaurl>
              </file>
            </metalink>"#
        );
        let files = parse(&text).unwrap();
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, ["disk.iso"]);
        assert_eq!(file.size, Some(3));
        assert_eq!(
            file.checksum.as_ref().unwrap().to_string(),
            format!("sha256:{SHA256_OF_ABC}")
        );
        let hosts: Vec<_> = file.urls.iter().map(|u| u.host_str().unwrap()).collect();
        if cfg!(feature = "ftp") {
            assert_eq!(hosts, ["one.example", "old.example", "two.example"]);
        } else {
            assert_eq!(hosts, ["one.example", "two.example"]);
        }
    }

    #[test]
    fn version_3_files_and_unsafe_names() {
        let text = r#"<metalink version="3.0" xmlns="http://www.metalinker.org/"><files>
            <file name="../../etc/passwd"><resources>
              <url type="http" preference="10">http://a.example/p</url>
              <url type="http" preference="90">http://b.example/p</url>
            </resources></file>
            <file name="docs/./guide.pdf"><verification>
              <hash type="sha1">a9993e364706816aba3e25717850c26c9cd0d89d</hash>
            </verification><resources><url type="http">http://a.example/g</url></resources></file>
            <file name="nowhere.bin"><resources><url type="ftp">ftp://a.example/n</url></resources></file>
            </files></metalink>"#;
        let files = parse(text).unwrap();
        let expected = if cfg!(feature = "ftp") { 3 } else { 2 };
        assert_eq!(
            files.len(),
            expected,
            "an FTP-only file needs the ftp feature"
        );
        assert_eq!(files[0].path, ["etc", "passwd"]);
        assert_eq!(files[0].urls[0].host_str(), Some("b.example"));
        assert_eq!(files[1].path, ["docs", "guide.pdf"]);
        assert!(files[1].checksum.is_some());
    }

    #[test]
    fn piece_hashes_of_both_versions() {
        let v4 = r#"<metalink><file name="a"><url>https://a.example/a</url>
            <pieces length="1024" type="sha-1"><hash>AA</hash><hash>bb</hash></pieces>
            <pieces length="2048" type="sha-256"><hash>cc</hash></pieces></file></metalink>"#;
        let pieces = parse(v4).unwrap()[0].pieces.clone().unwrap();
        assert_eq!(
            (pieces.length, pieces.hashes.as_slice()),
            (2048, ["cc".to_string()].as_slice())
        );

        let v3 = r#"<metalink version="3.0"><files><file name="b"><verification>
            <pieces length="512" type="sha1"><hash piece="1">22</hash><hash piece="0">11</hash></pieces>
            </verification><resources><url type="http">http://a.example/b</url></resources></file></files></metalink>"#;
        let pieces = parse(v3).unwrap()[0].pieces.clone().unwrap();
        assert_eq!(
            pieces.hashes,
            ["11", "22"],
            "ordered by the piece attribute"
        );
    }

    #[test]
    fn documents_without_files_are_refused() {
        assert!(parse("<metalink/>").is_err());
        assert!(parse("<html></html>").is_err());
    }
}
