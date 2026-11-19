//! What a link points to, judged from its file extension, the tag it was
//! found in, and, after probing, the server's Content-Type.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LinkKind {
    Video,
    Audio,
    Image,
    /// An HLS (`.m3u8`) or DASH (`.mpd`) playlist.
    Stream,
    Archive,
    Document,
    Program,
    DiskImage,
    Subtitle,
    Torrent,
    Metalink,
    /// Another web page.
    Page,
    Other,
}

impl LinkKind {
    pub const ALL: [LinkKind; 13] = [
        Self::Video,
        Self::Audio,
        Self::Image,
        Self::Stream,
        Self::Archive,
        Self::Document,
        Self::Program,
        Self::DiskImage,
        Self::Subtitle,
        Self::Torrent,
        Self::Metalink,
        Self::Page,
        Self::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Stream => "stream",
            Self::Archive => "archive",
            Self::Document => "document",
            Self::Program => "program",
            Self::DiskImage => "disk-image",
            Self::Subtitle => "subtitle",
            Self::Torrent => "torrent",
            Self::Metalink => "metalink",
            Self::Page => "page",
            Self::Other => "other",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == name.trim().to_ascii_lowercase())
    }

    /// Everything a user would want to download; not pages.
    pub fn is_file(self) -> bool {
        self != Self::Page
    }

    pub(crate) fn from_extension(extension: &str) -> Option<Self> {
        let kind = match extension.to_ascii_lowercase().as_str() {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "ts" | "flv" | "wmv" | "3gp" => {
                Self::Video
            }
            "mp3" | "flac" | "ogg" | "opus" | "m4a" | "wav" | "aac" | "oga" => Self::Audio,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "svg" | "bmp" | "heic" | "tif"
            | "tiff" => Self::Image,
            "m3u8" | "mpd" => Self::Stream,
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "lz" => {
                Self::Archive
            }
            "pdf" | "epub" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "ppt" | "pptx"
            | "csv" | "txt" => Self::Document,
            "deb" | "rpm" | "appimage" | "flatpak" | "exe" | "msi" | "dmg" | "apk" | "pkg"
            | "run" => Self::Program,
            "iso" | "img" | "qcow2" | "vmdk" => Self::DiskImage,
            "srt" | "vtt" | "ass" | "ssa" => Self::Subtitle,
            "torrent" => Self::Torrent,
            "meta4" | "metalink" => Self::Metalink,
            "html" | "htm" | "php" | "asp" | "aspx" | "jsp" | "shtml" | "xhtml" => Self::Page,
            _ => return None,
        };
        Some(kind)
    }

    pub(crate) fn from_content_type(content_type: &str) -> Option<Self> {
        let essence = content_type.split(';').next()?.trim().to_ascii_lowercase();
        let kind = match essence.as_str() {
            "text/html" | "application/xhtml+xml" => Self::Page,
            "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "application/dash+xml" => Self::Stream,
            "application/x-bittorrent" => Self::Torrent,
            "application/metalink4+xml" | "application/metalink+xml" => Self::Metalink,
            "application/pdf" | "application/epub+zip" => Self::Document,
            "application/zip"
            | "application/gzip"
            | "application/x-tar"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
            | "application/x-xz"
            | "application/zstd" => Self::Archive,
            "application/x-iso9660-image" => Self::DiskImage,
            "text/vtt" => Self::Subtitle,
            other if other.starts_with("video/") => Self::Video,
            other if other.starts_with("audio/") => Self::Audio,
            other if other.starts_with("image/") => Self::Image,
            _ => return None,
        };
        Some(kind)
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
