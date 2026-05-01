use anyhow::Result;
use lofty::error::LoftyError;
use lofty::file::AudioFile;
use lofty::file::TaggedFileExt;
use lofty::read_from_path;
use lofty::tag::Accessor;
use std::path::Path;
use thiserror::Error;

/// Track metadata extracted from audio file tags
#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub file_path: String,
    pub file_name: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_artist: String,
    pub duration: f64,
    pub track_number: Option<u32>,
}

/// Lyrics information from external files
#[derive(Debug, Clone, Default)]
pub struct LyricsInfo {
    pub txt_lyrics: Option<String>,
    pub lrc_lyrics: Option<String>,
}

/// Errors that can occur during metadata extraction
#[derive(Error, Debug)]
pub enum MetadataError {
    #[error("Cannot parse metadata from `{path}`: {source}")]
    ParseFailed { path: String, source: LoftyError },

    #[error("Missing required field `{field}` in `{path}`")]
    MissingField { field: String, path: String },

    #[error("No primary tag found in `{path}`")]
    NoPrimaryTag { path: String },
}

impl TrackMetadata {
    /// Extract metadata from an audio file path
    pub fn from_path(path: &Path) -> Result<Self, MetadataError> {
        let file_path = path.display().to_string();
        let file_name = path
            .file_name()
            .and_then(|f| f.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_default();

        // Parse audio file
        let tagged_file = read_from_path(&file_path).map_err(|e| MetadataError::ParseFailed {
            path: file_path.clone(),
            source: e,
        })?;

        let tag = tagged_file
            .primary_tag()
            .ok_or_else(|| MetadataError::NoPrimaryTag {
                path: file_path.clone(),
            })?
            .to_owned();

        let properties = tagged_file.properties();

        let tag_title = tag.title();

        // Extract required fields
        let title = metadata_title_or_file_name(tag_title.as_deref(), &file_name)
            .ok_or_else(|| MetadataError::MissingField {
                field: "title".to_string(),
                path: file_path.clone(),
            })?
            .to_string();

        let album = tag
            .album()
            .ok_or_else(|| MetadataError::MissingField {
                field: "album".to_string(),
                path: file_path.clone(),
            })?
            .to_string();

        let artist = tag
            .artist()
            .ok_or_else(|| MetadataError::MissingField {
                field: "artist".to_string(),
                path: file_path.clone(),
            })?
            .to_string();

        // Album artist is optional, fallback to artist
        let album_artist = tag
            .get_string(lofty::tag::ItemKey::AlbumArtist)
            .map(|s| s.to_string())
            .unwrap_or_else(|| artist.clone());

        let duration = properties.duration().as_secs_f64();
        let track_number = tag.track();

        Ok(TrackMetadata {
            file_path,
            file_name,
            title,
            album,
            artist,
            album_artist,
            duration,
            track_number,
        })
    }
}

impl LyricsInfo {
    /// Extract lyrics from external files (.txt and .lrc)
    pub fn from_path(path: &Path) -> Self {
        let mut result = LyricsInfo::default();

        // Get file stem (filename without extension)
        let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

        let parent_path = path.parent().unwrap_or(Path::new("."));

        // Try to read .txt lyrics
        let txt_path = parent_path.join(format!("{}.txt", file_stem));
        if let Ok(content) = std::fs::read_to_string(&txt_path) {
            result.txt_lyrics = Some(content);
        }

        // Try to read .lrc lyrics
        let lrc_path = parent_path.join(format!("{}.lrc", file_stem));
        if let Ok(content) = std::fs::read_to_string(&lrc_path) {
            result.lrc_lyrics = Some(content);
        }

        result
    }
}

/// Convenience function to extract both metadata and lyrics
pub fn extract_track_info(path: &Path) -> Result<(TrackMetadata, LyricsInfo), MetadataError> {
    let metadata = TrackMetadata::from_path(path)?;
    let lyrics = LyricsInfo::from_path(path);
    Ok((metadata, lyrics))
}

/// Check if a file is an instrumental based on lyrics content
pub fn is_instrumental_lyrics(lrc_lyrics: &Option<String>) -> bool {
    match lrc_lyrics {
        Some(lyrics) => {
            // Check for instrumental tag
            let re = regex::Regex::new(r"\[au:\s*instrumental\]").unwrap();
            re.is_match(lyrics)
        }
        None => false,
    }
}

fn metadata_title_or_file_name(title: Option<&str>, file_name: &str) -> Option<String> {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let file_name = file_name.trim();
            if file_name.is_empty() {
                None
            } else {
                Some(file_name.to_string())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_title_uses_tag_title_when_present() {
        assert_eq!(
            metadata_title_or_file_name(Some("Track Title"), "01 - File Name.flac"),
            Some("Track Title".to_string())
        );
    }

    #[test]
    fn metadata_title_falls_back_to_file_name_when_tag_title_is_missing() {
        assert_eq!(
            metadata_title_or_file_name(None, "01 - File Name.flac"),
            Some("01 - File Name.flac".to_string())
        );
    }

    #[test]
    fn metadata_title_falls_back_to_file_name_when_tag_title_is_blank() {
        assert_eq!(
            metadata_title_or_file_name(Some("   "), "01 - File Name.flac"),
            Some("01 - File Name.flac".to_string())
        );
    }
}
