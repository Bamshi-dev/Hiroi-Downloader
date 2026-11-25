//! Sorting downloads into folders by file type and by site, as IDM
//! does.

use std::path::{Path, PathBuf};

use hiroi_proto::{Categories, Category};

/// The categories a new installation starts with.
pub fn defaults() -> Categories {
    let category = |name: &str, folder: &str, extensions: &[&str]| Category {
        name: name.into(),
        folder: folder.into(),
        extensions: extensions.iter().map(|ext| ext.to_string()).collect(),
        sites: Vec::new(),
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

/// The folder for a job that names none: the folder of the category
/// the file matches, otherwise `base`. Categories for sites come first,
/// being the more specific.
pub fn folder_for(
    categories: &Categories,
    base: &Path,
    name: Option<&str>,
    host: Option<&str>,
) -> PathBuf {
    let extension = name.and_then(extension);
    let host = host.map(str::to_ascii_lowercase);
    let matches = |category: &&Category| {
        let type_matches = category.extensions.is_empty()
            || extension
                .as_ref()
                .is_some_and(|ext| category.extensions.contains(ext));
        let site_matches = category.sites.is_empty()
            || host.as_deref().is_some_and(|host| {
                category.sites.iter().any(|site| {
                    host == site
                        || host
                            .strip_suffix(site.as_str())
                            .is_some_and(|rest| rest.ends_with('.'))
                })
            });
        // A category with neither matches nothing.
        let specific = !category.extensions.is_empty() || !category.sites.is_empty();
        specific && type_matches && site_matches
    };
    let by_site = categories
        .categories
        .iter()
        .filter(|category| !category.sites.is_empty())
        .find(matches);
    let matching = by_site.or_else(|| {
        categories
            .categories
            .iter()
            .filter(|category| category.sites.is_empty())
            .find(matches)
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
            folder_for(&categories, base, Some("film.MKV"), None),
            base.join("Videos")
        );
        assert_eq!(
            folder_for(&categories, base, Some("src.tar.gz"), None),
            base.join("Archives")
        );
        assert_eq!(folder_for(&categories, base, Some("notes"), None), base);
        assert_eq!(folder_for(&categories, base, None, None), base);
    }

    #[test]
    fn disabled_or_escaping_categories_use_the_base_folder() {
        let base = Path::new("/d");
        let mut categories = defaults();
        categories.enabled = false;
        assert_eq!(folder_for(&categories, base, Some("a.mp4"), None), base);
        let escaping = Categories {
            enabled: true,
            categories: vec![Category {
                name: "x".into(),
                extensions: vec!["mp4".into()],
                folder: "../../etc".into(),
                sites: Vec::new(),
            }],
        };
        assert_eq!(folder_for(&escaping, base, Some("a.mp4"), None), base);
    }

    #[test]
    fn site_categories_come_first() {
        let base = Path::new("/d");
        let mut categories = defaults();
        categories.categories.push(Category {
            name: "course".into(),
            extensions: Vec::new(),
            folder: "Course".into(),
            sites: vec!["uni.example.edu".into()],
        });
        let at = |name, host| folder_for(&categories, base, Some(name), Some(host));
        assert_eq!(at("talk.mp4", "media.uni.example.edu"), base.join("Course"));
        assert_eq!(at("talk.mp4", "UNI.example.edu"), base.join("Course"));
        assert_eq!(at("talk.mp4", "notuni.example.edu"), base.join("Videos"));
        assert_eq!(at("talk.mp4", "example.edu"), base.join("Videos"));
    }
}
