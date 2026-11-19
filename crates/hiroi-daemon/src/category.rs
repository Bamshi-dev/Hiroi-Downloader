//! Sorting downloads into folders by file type, as IDM does.

use std::path::{Path, PathBuf};

use hiroi_proto::{Categories, Category};

/// The categories a new installation starts with.
pub fn defaults() -> Categories {
    let category = |name: &str, folder: &str, extensions: &[&str]| Category {
        name: name.into(),
        folder: folder.into(),
        extensions: extensions.iter().map(|ext| ext.to_string()).collect(),
    };
    Categories {
        enabled: true,
        categories: vec![
            category(
                "video",
                "Videos",
                &[
                    "mp4", "mkv", "webm", "avi", "mov", "m4v", "ts", "flv", "wmv",
                ],
            ),
            category(
                "music",
                "Music",
                &["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac"],
            ),
            category(
                "pictures",
                "Pictures",
                &["jpg", "jpeg", "png", "gif", "webp", "avif", "svg", "heic"],
            ),
            category(
                "archives",
                "Archives",
                &["zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst"],
            ),
            category(
                "documents",
                "Documents",
                &[
                    "pdf", "epub", "doc", "docx", "odt", "xls", "xlsx", "ods", "ppt", "pptx", "txt",
                ],
            ),
            category(
                "programs",
                "Programs",
                &[
                    "deb", "rpm", "appimage", "flatpak", "exe", "msi", "dmg", "apk", "sh",
                ],
            ),
            category("disk images", "Disk images", &["iso", "img"]),
        ],
    }
}

/// The folder for a job that names none: the category's folder when the
/// name matches one, otherwise `base`.
pub fn folder_for(categories: &Categories, base: &Path, name: Option<&str>) -> PathBuf {
    let extension = name.and_then(extension);
    let matching = categories.categories.iter().find(|category| {
        extension
            .as_ref()
            .is_some_and(|ext| category.extensions.contains(ext))
    });
    match matching {
        Some(category) if categories.enabled && is_relative_name(&category.folder) => {
            base.join(&category.folder)
        }
        _ => base.to_path_buf(),
    }
}

/// Lower-case extension, with `.tar.gz` style double extensions folded
/// to their last part.
fn extension(name: &str) -> Option<String> {
    let (_, extension) = name.rsplit_once('.')?;
    (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// A category folder must stay inside the download folder.
fn is_relative_name(folder: &str) -> bool {
    let path = Path::new(folder);
    !folder.is_empty()
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

/// File name a URL suggests, for choosing a category before the server
/// has been asked.
pub fn name_from_url(url: &str) -> Option<String> {
    let parsed = hiroi_engine::Url::parse(url).ok()?;
    let last = parsed
        .path_segments()?
        .rfind(|segment| !segment.is_empty())?;
    Some(last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_to_their_category_folder() {
        let categories = defaults();
        let base = Path::new("/home/me/Downloads");
        assert_eq!(
            folder_for(&categories, base, Some("film.MKV")),
            base.join("Videos")
        );
        assert_eq!(
            folder_for(&categories, base, Some("src.tar.gz")),
            base.join("Archives")
        );
        assert_eq!(folder_for(&categories, base, Some("notes")), base);
        assert_eq!(folder_for(&categories, base, None), base);
    }

    #[test]
    fn disabled_or_escaping_categories_use_the_base_folder() {
        let base = Path::new("/d");
        let mut categories = defaults();
        categories.enabled = false;
        assert_eq!(folder_for(&categories, base, Some("a.mp4")), base);
        let escaping = Categories {
            enabled: true,
            categories: vec![Category {
                name: "x".into(),
                extensions: vec!["mp4".into()],
                folder: "../../etc".into(),
            }],
        };
        assert_eq!(folder_for(&escaping, base, Some("a.mp4")), base);
    }
}
