#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

pub mod autosync;
pub mod db;
pub mod export;
pub mod library;
pub mod lrclib;
pub mod lyricsfile;
pub mod parser;
pub mod persistent_entities;
pub mod player;
pub mod scanner;
pub mod state;
pub mod translation;
pub mod utils;

use anyhow::Context;
use persistent_entities::{
    PersistentAlbum, PersistentArtist, PersistentConfig, PersistentTrack, PlayableTrack,
};
use player::Player;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use state::{AppState, Notify, NotifyType, ServiceAccess};
use tauri::{AppHandle, Emitter, Manager, State};
use translation::{LyricTranslation, LyricTranslationUpsert, TranslationRequest};

struct ResolvedLyricsPayload {
    plain_lyrics: String,
    synced_lyrics: String,
    is_instrumental: bool,
    provided_lyricsfile: Option<String>,
}

const LRCLIB_TRACK_NOT_FOUND: &str = "This track does not exist in LRCLIB database";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishLyricsProgress {
    request_challenge: String,
    solve_challenge: String,
    publish_lyrics: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlagLyricsProgress {
    request_challenge: String,
    solve_challenge: String,
    flag_lyrics: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportLyricsFormat {
    Txt,
    Lrc,
    Embedded,
}

/// Match quality for track matching results
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum MatchQuality {
    Strong,
    Partial,
}

/// Track matching result with quality information
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MatchingTrack {
    #[serde(flatten)]
    track: PersistentTrack,
    match_quality: MatchQuality,
}

/// Audio metadata extracted from a file (for file picker)
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioMetadataResponse {
    pub file_path: String,
    pub file_name: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_artist: String,
    pub duration: f64,
    pub track_number: Option<u32>,
}

impl From<ExportLyricsFormat> for export::ExportFormat {
    fn from(value: ExportLyricsFormat) -> Self {
        match value {
            ExportLyricsFormat::Txt => export::ExportFormat::Txt,
            ExportLyricsFormat::Lrc => export::ExportFormat::Lrc,
            ExportLyricsFormat::Embedded => export::ExportFormat::Embedded,
        }
    }
}

fn resolve_lrclib_lyrics_payload(
    lrclib_response: lrclib::get::RawResponse,
) -> Result<ResolvedLyricsPayload, String> {
    let provided_lyricsfile = lrclib_response
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty());

    if let Some(lyricsfile_content) = provided_lyricsfile {
        let parsed =
            lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
        let plain_lyrics = parsed.plain_lyrics.unwrap_or_default();
        let synced_lyrics = parsed.synced_lyrics.unwrap_or_default();
        let is_instrumental = parsed.is_instrumental;

        if !is_instrumental && plain_lyrics.trim().is_empty() && synced_lyrics.trim().is_empty() {
            return Err(LRCLIB_TRACK_NOT_FOUND.to_owned());
        }

        return Ok(ResolvedLyricsPayload {
            plain_lyrics,
            synced_lyrics,
            is_instrumental,
            provided_lyricsfile: Some(lyricsfile_content),
        });
    }

    match lrclib::get::Response::from_raw_response(lrclib_response) {
        lrclib::get::Response::SyncedLyrics(synced_lyrics, plain_lyrics) => {
            Ok(ResolvedLyricsPayload {
                plain_lyrics,
                synced_lyrics,
                is_instrumental: false,
                provided_lyricsfile: None,
            })
        }
        lrclib::get::Response::UnsyncedLyrics(plain_lyrics) => Ok(ResolvedLyricsPayload {
            plain_lyrics,
            synced_lyrics: String::new(),
            is_instrumental: false,
            provided_lyricsfile: None,
        }),
        lrclib::get::Response::IsInstrumental => Ok(ResolvedLyricsPayload {
            plain_lyrics: String::new(),
            synced_lyrics: lyricsfile::INSTRUMENTAL_LRC.to_owned(),
            is_instrumental: true,
            provided_lyricsfile: None,
        }),
        lrclib::get::Response::None => Err(LRCLIB_TRACK_NOT_FOUND.to_owned()),
    }
}

#[tauri::command]
async fn get_directories(app_state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let directories = db::get_directories(conn);
    match directories {
        Ok(directories) => Ok(directories),
        Err(error) => Err(format!(
            "Cannot get existing directories from database. Error: {}",
            error
        )),
    }
}

#[tauri::command]
async fn set_directories(
    directories: Vec<String>,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_directories(directories, conn).map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn get_init(app_state: State<'_, AppState>) -> Result<bool, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let init = library::get_init(conn).map_err(|err| err.to_string())?;

    Ok(init)
}

#[tauri::command]
async fn get_config(app_state: State<'_, AppState>) -> Result<PersistentConfig, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let config = db::get_config(conn).map_err(|err| err.to_string())?;

    Ok(config)
}

#[tauri::command]
async fn set_config(
    skip_tracks_with_synced_lyrics: bool,
    skip_tracks_with_plain_lyrics: bool,
    show_line_count: bool,
    try_embed_lyrics: bool,
    theme_mode: &str,
    lrclib_instance: &str,
    volume: f64,
    translation_auto_enabled: bool,
    translation_target_language: &str,
    translation_provider: &str,
    translation_export_mode: &str,
    translation_gemini_api_key: &str,
    translation_gemini_model: &str,
    translation_deepl_api_key: &str,
    translation_google_api_key: &str,
    translation_microsoft_api_key: &str,
    translation_microsoft_region: &str,
    translation_openai_base_url: &str,
    translation_openai_api_key: &str,
    translation_openai_model: &str,
    auto_sync_enabled: bool,
    auto_sync_backend: &str,
    auto_sync_model: &str,
    auto_sync_aligner_model: &str,
    auto_sync_save_policy: &str,
    auto_sync_confidence_threshold: f64,
    auto_sync_auto_download: bool,
    auto_sync_language_override: &str,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_config(
        skip_tracks_with_synced_lyrics,
        skip_tracks_with_plain_lyrics,
        show_line_count,
        try_embed_lyrics,
        theme_mode,
        lrclib_instance,
        volume,
        translation_auto_enabled,
        translation_target_language,
        translation_provider,
        translation_export_mode,
        translation_gemini_api_key,
        translation_gemini_model,
        translation_deepl_api_key,
        translation_google_api_key,
        translation_microsoft_api_key,
        translation_microsoft_region,
        translation_openai_base_url,
        translation_openai_api_key,
        translation_openai_model,
        auto_sync_enabled,
        auto_sync_backend,
        auto_sync_model,
        auto_sync_aligner_model,
        auto_sync_save_policy,
        auto_sync_confidence_threshold,
        auto_sync_auto_download,
        auto_sync_language_override,
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn get_translation_config(
    app_state: State<'_, AppState>,
) -> Result<PersistentConfig, String> {
    get_config(app_state).await
}

#[tauri::command]
async fn set_translation_config(
    translation_auto_enabled: bool,
    translation_target_language: &str,
    translation_provider: &str,
    translation_export_mode: &str,
    translation_gemini_api_key: &str,
    translation_gemini_model: &str,
    translation_deepl_api_key: &str,
    translation_google_api_key: &str,
    translation_microsoft_api_key: &str,
    translation_microsoft_region: &str,
    translation_openai_base_url: &str,
    translation_openai_api_key: &str,
    translation_openai_model: &str,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let config = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_config(conn).map_err(|err| err.to_string())?
    };

    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_config(
        config.skip_tracks_with_synced_lyrics,
        config.skip_tracks_with_plain_lyrics,
        config.show_line_count,
        config.try_embed_lyrics,
        &config.theme_mode,
        &config.lrclib_instance,
        config.volume,
        translation_auto_enabled,
        translation_target_language,
        translation_provider,
        translation_export_mode,
        translation_gemini_api_key,
        translation_gemini_model,
        translation_deepl_api_key,
        translation_google_api_key,
        translation_microsoft_api_key,
        translation_microsoft_region,
        translation_openai_base_url,
        translation_openai_api_key,
        translation_openai_model,
        config.auto_sync_enabled,
        &config.auto_sync_backend,
        &config.auto_sync_model,
        &config.auto_sync_aligner_model,
        &config.auto_sync_save_policy,
        config.auto_sync_confidence_threshold,
        config.auto_sync_auto_download,
        &config.auto_sync_language_override,
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn get_auto_sync_config(app_state: State<'_, AppState>) -> Result<PersistentConfig, String> {
    get_config(app_state).await
}

#[tauri::command]
async fn set_auto_sync_config(
    auto_sync_enabled: bool,
    auto_sync_backend: &str,
    auto_sync_model: &str,
    auto_sync_aligner_model: &str,
    auto_sync_save_policy: &str,
    auto_sync_confidence_threshold: f64,
    auto_sync_auto_download: bool,
    auto_sync_language_override: &str,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let config = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_config(conn).map_err(|err| err.to_string())?
    };

    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_config(
        config.skip_tracks_with_synced_lyrics,
        config.skip_tracks_with_plain_lyrics,
        config.show_line_count,
        config.try_embed_lyrics,
        &config.theme_mode,
        &config.lrclib_instance,
        config.volume,
        config.translation_auto_enabled,
        &config.translation_target_language,
        &config.translation_provider,
        &config.translation_export_mode,
        &config.translation_gemini_api_key,
        &config.translation_gemini_model,
        &config.translation_deepl_api_key,
        &config.translation_google_api_key,
        &config.translation_microsoft_api_key,
        &config.translation_microsoft_region,
        &config.translation_openai_base_url,
        &config.translation_openai_api_key,
        &config.translation_openai_model,
        auto_sync_enabled,
        auto_sync_backend,
        auto_sync_model,
        auto_sync_aligner_model,
        auto_sync_save_policy,
        auto_sync_confidence_threshold,
        auto_sync_auto_download,
        auto_sync_language_override,
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn uninitialize_library(app_state: State<'_, AppState>) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();

    library::uninitialize_library(conn).map_err(|err| err.to_string())?;

    Ok(())
}

/// Full wipe and rescan of the library.
/// Clears tracks, albums, artists tables, resets init flag, and performs a full rescan.
/// Associated lyricsfiles are preserved (track_id set to NULL) for potential reattachment.
#[tauri::command]
async fn full_scan_library(
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
    use_hash_detection: Option<bool>,
) -> Result<scanner::models::ScanResult, String> {
    // Step 1: Full wipe - clear all library data and reset init flag
    {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        library::full_wipe_library(conn).map_err(|err| err.to_string())?;
    }

    // Step 2: Get directories
    let directories = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_directories(conn).map_err(|err| err.to_string())?
    };

    // Determine detection method (default to Hash for reliability)
    let detection_method = if use_hash_detection.unwrap_or(true) {
        scanner::scan::DetectionMethod::Hash
    } else {
        scanner::scan::DetectionMethod::Metadata
    };

    // Clone app_handle for use in the closure
    let app_handle_clone = app_handle.clone();

    // Step 3: Run full scan
    let scan_result = tokio::task::block_in_place(|| {
        let mut conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_mut().unwrap();

        scanner::scan_library(
            &directories,
            conn,
            &|progress| {
                // Emit progress directly (synchronous)
                let _ = app_handle_clone.emit("scan-progress", progress);
            },
            detection_method,
        )
    })
    .map_err(|err| err.to_string())?;

    // Emit completion event
    let _ = app_handle.emit("scan-complete", &scan_result);

    Ok(scan_result)
}

#[tauri::command]
async fn scan_library(
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
    use_hash_detection: Option<bool>,
) -> Result<scanner::models::ScanResult, String> {
    // Get directories first (requires immutable access)
    let directories = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_directories(conn).map_err(|err| err.to_string())?
    };

    // Determine detection method (default to Hash for reliability)
    let detection_method = if use_hash_detection.unwrap_or(true) {
        scanner::scan::DetectionMethod::Hash
    } else {
        scanner::scan::DetectionMethod::Metadata
    };

    // Clone app_handle for use in the closure
    let app_handle_clone = app_handle.clone();

    // Run scan synchronously but use block_in_place to not block the runtime
    let scan_result = tokio::task::block_in_place(|| {
        let mut conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_mut().unwrap();

        scanner::scan_library(
            &directories,
            conn,
            &|progress| {
                // Emit progress directly (synchronous)
                let _ = app_handle_clone.emit("scan-progress", progress);
            },
            detection_method,
        )
    })
    .map_err(|err| err.to_string())?;

    // Emit completion event
    let _ = app_handle.emit("scan-complete", &scan_result);

    Ok(scan_result)
}

#[tauri::command]
async fn get_tracks(app_state: State<'_, AppState>) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_tracks(conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_track_ids(
    search_query: Option<String>,
    synced_lyrics_tracks: Option<bool>,
    plain_lyrics_tracks: Option<bool>,
    instrumental_tracks: Option<bool>,
    no_lyrics_tracks: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let search_query = search_query.filter(|s| !s.is_empty());
    let track_ids = library::get_track_ids(
        search_query,
        synced_lyrics_tracks.unwrap_or(true),
        plain_lyrics_tracks.unwrap_or(true),
        instrumental_tracks.unwrap_or(true),
        no_lyrics_tracks.unwrap_or(true),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn get_track(
    track_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentTrack, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track = library::get_track(track_id, conn).map_err(|err| err.to_string())?;

    Ok(track)
}

#[tauri::command]
async fn find_matching_tracks(
    title: String,
    album_name: String,
    artist_name: String,
    duration: Option<f64>,
    app_state: State<'_, AppState>,
) -> Result<Vec<MatchingTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();

    // First, try to find tracks with all criteria (strong match)
    let strong_matches = db::find_tracks_by_metadata(
        &title,
        Some(&artist_name),
        Some(&album_name),
        duration,
        conn,
    )
    .map_err(|err| err.to_string())?;

    // If we have strong matches, return them
    if !strong_matches.is_empty() {
        return Ok(strong_matches
            .into_iter()
            .map(|track| MatchingTrack {
                track,
                match_quality: MatchQuality::Strong,
            })
            .collect());
    }

    // Otherwise, search for partial matches (title only match with duration if provided)
    let partial_matches = db::find_tracks_by_metadata(&title, None, None, duration, conn)
        .map_err(|err| err.to_string())?;

    // Filter out tracks where artist or album don't match at all (still partial but relevant)
    let normalized_artist = utils::prepare_input(&artist_name);
    let normalized_album = utils::prepare_input(&album_name);

    let partial_results: Vec<MatchingTrack> = partial_matches
        .into_iter()
        .map(|track| {
            let track_artist_normalized = utils::prepare_input(&track.artist_name);
            let track_album_normalized = utils::prepare_input(&track.album_name);

            // Check if artist or album matches (case-insensitive via normalization)
            let _artist_matches = track_artist_normalized == normalized_artist;
            let _album_matches = track_album_normalized == normalized_album;

            // It's a partial match if title matches and at least one of artist/album matches
            // or if we have no duration filter and just title matches
            MatchingTrack {
                track,
                match_quality: MatchQuality::Partial,
            }
        })
        .collect();

    Ok(partial_results)
}

#[tauri::command]
async fn get_audio_metadata(file_path: String) -> Result<AudioMetadataResponse, String> {
    let path = std::path::Path::new(&file_path);

    let metadata =
        scanner::metadata::TrackMetadata::from_path(path).map_err(|err| err.to_string())?;

    Ok(AudioMetadataResponse {
        file_path: metadata.file_path,
        file_name: metadata.file_name,
        title: metadata.title,
        album: metadata.album,
        artist: metadata.artist,
        album_artist: metadata.album_artist,
        duration: metadata.duration,
        track_number: metadata.track_number,
    })
}

#[tauri::command]
async fn prepare_search_query(title: String) -> Result<String, String> {
    Ok(utils::prepare_search_input(&title))
}

#[tauri::command]
async fn get_albums(app_state: State<'_, AppState>) -> Result<Vec<PersistentAlbum>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let albums = library::get_albums(conn).map_err(|err| err.to_string())?;

    Ok(albums)
}

#[tauri::command]
async fn get_album_ids(app_state: State<'_, AppState>) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let album_ids = library::get_album_ids(conn).map_err(|err| err.to_string())?;

    Ok(album_ids)
}

#[tauri::command]
async fn get_album(
    album_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentAlbum, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let album = library::get_album(album_id, conn).map_err(|err| err.to_string())?;

    Ok(album)
}

#[tauri::command]
async fn get_artists(app_state: State<'_, AppState>) -> Result<Vec<PersistentArtist>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let artists = library::get_artists(conn).map_err(|err| err.to_string())?;

    Ok(artists)
}

#[tauri::command]
async fn get_artist_ids(app_state: State<'_, AppState>) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let artist_ids = library::get_artist_ids(conn).map_err(|err| err.to_string())?;

    Ok(artist_ids)
}

#[tauri::command]
async fn get_artist(
    artist_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentArtist, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let artist = library::get_artist(artist_id, conn).map_err(|err| err.to_string())?;

    Ok(artist)
}

#[tauri::command]
async fn get_album_tracks(
    album_id: i64,
    app_state: State<'_, AppState>,
) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_album_tracks(album_id, conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_artist_tracks(
    artist_id: i64,
    app_state: State<'_, AppState>,
) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_artist_tracks(artist_id, conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_album_track_ids(
    album_id: i64,
    without_plain_lyrics: Option<bool>,
    without_synced_lyrics: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = library::get_album_track_ids(
        album_id,
        without_plain_lyrics.unwrap_or(false),
        without_synced_lyrics.unwrap_or(false),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn get_artist_track_ids(
    artist_id: i64,
    without_plain_lyrics: Option<bool>,
    without_synced_lyrics: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = library::get_artist_track_ids(
        artist_id,
        without_plain_lyrics.unwrap_or(false),
        without_synced_lyrics.unwrap_or(false),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn list_track_translations(
    track_id: i64,
    app_handle: AppHandle,
) -> Result<Vec<LyricTranslation>, String> {
    app_handle
        .db(|db| db::list_lyric_translations_for_track(track_id, db))
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn get_track_ids_requiring_translation(app_handle: AppHandle) -> Result<Vec<i64>, String> {
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let provider = config.translation_provider.clone();
    let provider_model = translation::provider_model_from_config(&config);
    let target_language = translation::target_language_from_config(&config);
    let settings_hash = translation::settings_hash_from_config(&config);

    app_handle
        .db(|db| -> anyhow::Result<Vec<i64>> {
            let candidate_ids = db::get_track_ids_with_synced_lyrics(db)?;
            let mut track_ids = Vec::new();

            for track_id in candidate_ids {
                let track = db::get_track_by_id(track_id, db)?;
                let Some(lyricsfile_id) = track.lyricsfile_id else {
                    continue;
                };
                let Some(lyricsfile_content) = track
                    .lyricsfile
                    .as_deref()
                    .filter(|content| !content.trim().is_empty())
                else {
                    continue;
                };
                let source_hash = translation::lyrics_source_hash(lyricsfile_content);
                let existing = db::get_current_lyric_translation(
                    lyricsfile_id,
                    &source_hash,
                    &provider,
                    &provider_model,
                    &target_language,
                    &settings_hash,
                    db,
                )?;

                if existing.is_none() {
                    track_ids.push(track_id);
                }
            }

            Ok(track_ids)
        })
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn prepare_existing_lyrics_translation_queue(
    app_handle: AppHandle,
) -> Result<db::PreparedTranslationQueue, String> {
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let provider = config.translation_provider.clone();
    let provider_model = translation::provider_model_from_config(&config);
    let target_language = translation::target_language_from_config(&config);
    let settings_hash = translation::settings_hash_from_config(&config);

    let prepared = app_handle
        .db(|db| {
            db::prepare_existing_lyrics_translation_queue(
                &provider,
                &provider_model,
                &target_language,
                &settings_hash,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    for track_id in prepared.changed_track_ids.iter().copied() {
        let _ = app_handle.emit("reload-track-id", track_id);
    }

    Ok(prepared)
}

#[tauri::command]
async fn list_auto_sync_assets(
    app_handle: AppHandle,
) -> Result<Vec<autosync::AutoSyncAssetStatus>, String> {
    autosync::list_auto_sync_assets(&app_handle).map_err(|err| err.to_string())
}

#[tauri::command]
async fn download_auto_sync_asset(
    asset_id: String,
    app_handle: AppHandle,
) -> Result<autosync::AutoSyncAssetStatus, String> {
    autosync::download_auto_sync_asset(app_handle, asset_id)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn test_auto_sync_engine(app_handle: AppHandle) -> Result<String, String> {
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    autosync::test_qwen_engine(app_handle, config)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn prepare_auto_sync_queue(
    app_handle: AppHandle,
) -> Result<db::PreparedAutoSyncQueue, String> {
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let prepared = app_handle
        .db(|db| db::prepare_auto_sync_queue(&config, db))
        .map_err(|err| err.to_string())?;

    for track_id in prepared.changed_track_ids.iter().copied() {
        let _ = app_handle.emit("reload-track-id", track_id);
    }

    Ok(prepared)
}

#[tauri::command]
async fn list_track_sync_results(
    track_id: i64,
    app_handle: AppHandle,
) -> Result<Vec<autosync::LyricSync>, String> {
    app_handle
        .db(|db| db::list_lyric_syncs_for_track(track_id, db))
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn auto_sync_track_lyrics(
    track_id: i64,
    app_handle: AppHandle,
) -> Result<autosync::LyricSync, String> {
    auto_sync_track_lyrics_internal(track_id, app_handle).await
}

#[tauri::command]
async fn apply_sync_result_to_lyricsfile(
    sync_id: i64,
    app_handle: AppHandle,
) -> Result<autosync::LyricSync, String> {
    let sync = app_handle
        .db(|db| db::get_lyric_sync_by_id(sync_id, db))
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "Sync result not found".to_string())?;
    let generated_lrc = sync
        .generated_lrc
        .clone()
        .filter(|lrc| !lrc.trim().is_empty())
        .ok_or_else(|| "Sync result has no generated LRC".to_string())?;
    let track_id = sync
        .track_id
        .ok_or_else(|| "Sync result is not associated with a library track".to_string())?;

    let mut applied = sync.clone();
    applied.status = autosync::AUTO_SYNC_STATUS_SUCCEEDED.to_string();

    app_handle
        .db(|db| -> anyhow::Result<()> {
            let track = db::get_track_by_id(track_id, db)?;
            let parsed = lyricsfile::parse_lyricsfile(&sync.source_lyricsfile)?;
            let metadata = lyricsfile::LyricsfileTrackMetadata::from_persistent_track(&track);
            let updated = lyricsfile::build_lyricsfile(
                &metadata,
                parsed.plain_lyrics.as_deref(),
                Some(&generated_lrc),
            )
            .ok_or_else(|| anyhow::anyhow!("Failed to build lyricsfile from sync result"))?;
            db::upsert_lyricsfile_for_track(
                track.id,
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
                &updated,
                db,
            )?;
            let upsert = autosync::LyricSyncUpsert {
                lyricsfile_id: sync.lyricsfile_id,
                track_id: sync.track_id,
                source_hash: sync.source_hash.clone(),
                source_lyricsfile: sync.source_lyricsfile.clone(),
                audio_hash: sync.audio_hash.clone(),
                backend: sync.backend.clone(),
                model: sync.model.clone(),
                aligner_model: sync.aligner_model.clone(),
                language: sync.language.clone(),
                settings_hash: sync.settings_hash.clone(),
                status: autosync::AUTO_SYNC_STATUS_SUCCEEDED.to_string(),
                generated_lrc: sync.generated_lrc.clone(),
                generated_lines_json: sync.generated_lines_json.clone(),
                confidence: sync.confidence,
                metrics_json: sync.metrics_json.clone(),
                error_message: None,
                engine_metadata_json: sync.engine_metadata_json.clone(),
            };
            db::upsert_lyric_sync(&upsert, db)?;
            Ok(())
        })
        .map_err(|err| err.to_string())?;

    let _ = app_handle.emit("reload-track-id", track_id);
    Ok(applied)
}

#[tauri::command]
async fn translate_track_lyrics(
    track_id: i64,
    app_handle: AppHandle,
) -> Result<LyricTranslation, String> {
    translate_track_lyrics_internal(track_id, app_handle, true).await
}

#[tauri::command]
async fn test_translation_provider(app_handle: AppHandle) -> Result<String, String> {
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let source_lrc = "[00:01.00]Hello world".to_string();
    let request = TranslationRequest {
        title: "Provider test".to_string(),
        album_name: String::new(),
        artist_name: "LRCGET".to_string(),
        source_language: Some("English".to_string()),
        target_language: translation::target_language_from_config(&config),
        source_lrc: source_lrc.clone(),
    };
    let response_json = translation::request_translation(&config, &request)
        .await
        .map_err(|err| err.to_string())?;
    translation::validate_translation_lines(&source_lrc, &response_json)
        .map_err(|err| err.to_string())?;

    Ok("Provider test succeeded".to_string())
}

fn maybe_spawn_auto_translation(track_id: i64, app_handle: AppHandle) {
    let enabled = app_handle
        .db(|db| db::get_config(db).map(|config| config.translation_auto_enabled))
        .unwrap_or(false);

    if !enabled {
        return;
    }

    tauri::async_runtime::spawn(async move {
        if let Err(error) =
            translate_track_lyrics_internal(track_id, app_handle.clone(), false).await
        {
            eprintln!("Auto-translation failed for track {}: {}", track_id, error);
        }
        let _ = app_handle.emit("reload-track-id", track_id);
    });
}

fn maybe_spawn_auto_sync(track_id: i64, app_handle: AppHandle) {
    let enabled = app_handle
        .db(|db| db::get_config(db).map(|config| config.auto_sync_enabled))
        .unwrap_or(false);

    if !enabled {
        return;
    }

    tauri::async_runtime::spawn(async move {
        if let Err(error) = auto_sync_track_lyrics_internal(track_id, app_handle.clone()).await {
            eprintln!("Auto-sync failed for track {}: {}", track_id, error);
        }
        let _ = app_handle.emit("reload-track-id", track_id);
    });
}

async fn auto_sync_track_lyrics_internal(
    track_id: i64,
    app_handle: AppHandle,
) -> Result<autosync::LyricSync, String> {
    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let lyricsfile_id = track
        .lyricsfile_id
        .ok_or_else(|| "No lyricsfile is stored for this track".to_string())?;
    let lyricsfile_content = track
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "No lyrics available to sync".to_string())?;
    let parsed =
        lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;

    if parsed.is_instrumental {
        return Err("Instrumental tracks cannot be auto-synced".to_string());
    }
    if parsed
        .synced_lyrics
        .as_deref()
        .map(str::trim)
        .filter(|content| !content.is_empty())
        .is_some()
    {
        return Err("Track already has timestamped lyrics".to_string());
    }

    let plain_lyrics = parsed
        .plain_lyrics
        .clone()
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "No plain lyrics available to sync".to_string())?;

    let source_hash = translation::lyrics_source_hash(&lyricsfile_content);
    let audio_hash = autosync::audio_hash(std::path::Path::new(&track.file_path))
        .map_err(|err| format!("Failed to hash audio file: {err}"))?;
    let backend = autosync::provider_name_from_config(&config);
    let model = autosync::model_from_config(&config);
    let aligner_model = autosync::aligner_model_from_config(&config);
    let language = autosync::language_for_sync(&config, &plain_lyrics);
    let settings_hash = autosync::settings_hash_from_config(&config);

    let pending = autosync::LyricSyncUpsert {
        lyricsfile_id,
        track_id: Some(track_id),
        source_hash: source_hash.clone(),
        source_lyricsfile: lyricsfile_content.clone(),
        audio_hash: audio_hash.clone(),
        backend: backend.clone(),
        model: model.clone(),
        aligner_model: aligner_model.clone(),
        language: language.clone(),
        settings_hash: settings_hash.clone(),
        status: autosync::AUTO_SYNC_STATUS_PENDING.to_string(),
        generated_lrc: None,
        generated_lines_json: None,
        confidence: None,
        metrics_json: None,
        error_message: None,
        engine_metadata_json: None,
    };
    app_handle
        .db(|db| db::upsert_lyric_sync(&pending, db))
        .map_err(|err| err.to_string())?;
    let _ = app_handle.emit("reload-track-id", track_id);

    let run_result = run_auto_sync_provider(
        track.clone(),
        config.clone(),
        plain_lyrics.clone(),
        language.clone(),
        app_handle.clone(),
    )
    .await;

    let upsert = match run_result {
        Ok(generated) => {
            let generated_lines_json =
                serde_json::to_string(&generated.lines).map_err(|err| err.to_string())?;
            let metrics_json =
                serde_json::to_string(&generated.metrics).map_err(|err| err.to_string())?;
            let policy = autosync::save_policy_from_config(&config);
            let should_apply = autosync::should_auto_apply_sync_result(
                &policy,
                config.auto_sync_confidence_threshold,
                &generated.metrics,
            );
            if should_apply {
                let metadata = lyricsfile::LyricsfileTrackMetadata::from_persistent_track(&track);
                let updated = lyricsfile::build_lyricsfile(
                    &metadata,
                    Some(&plain_lyrics),
                    Some(&generated.synced_lrc),
                )
                .ok_or_else(|| "Failed to build auto-synced lyricsfile".to_string())?;
                app_handle
                    .db(|db| {
                        db::upsert_lyricsfile_for_track(
                            track.id,
                            &track.title,
                            &track.album_name,
                            &track.artist_name,
                            track.duration,
                            &updated,
                            db,
                        )
                    })
                    .map_err(|err| err.to_string())?;
            }

            autosync::LyricSyncUpsert {
                lyricsfile_id,
                track_id: Some(track_id),
                source_hash,
                source_lyricsfile: lyricsfile_content,
                audio_hash,
                backend,
                model,
                aligner_model,
                language: language.clone(),
                settings_hash,
                status: if should_apply {
                    autosync::AUTO_SYNC_STATUS_SUCCEEDED
                } else {
                    autosync::AUTO_SYNC_STATUS_NEEDS_REVIEW
                }
                .to_string(),
                generated_lrc: Some(generated.synced_lrc),
                generated_lines_json: Some(generated_lines_json),
                confidence: Some(generated.metrics.confidence),
                metrics_json: Some(metrics_json),
                error_message: None,
                engine_metadata_json: Some(
                    serde_json::json!({
                        "backend": "qwen3_asr_cpp",
                        "mode": "direct_align_or_transcribe_align",
                        "language": language,
                    })
                    .to_string(),
                ),
            }
        }
        Err(error) => autosync::LyricSyncUpsert {
            lyricsfile_id,
            track_id: Some(track_id),
            source_hash,
            source_lyricsfile: lyricsfile_content,
            audio_hash,
            backend,
            model,
            aligner_model,
            language,
            settings_hash,
            status: autosync::AUTO_SYNC_STATUS_FAILED.to_string(),
            generated_lrc: None,
            generated_lines_json: None,
            confidence: None,
            metrics_json: None,
            error_message: Some(error.to_string()),
            engine_metadata_json: None,
        },
    };

    let sync_id = app_handle
        .db(|db| db::upsert_lyric_sync(&upsert, db))
        .map_err(|err| err.to_string())?;
    let sync = app_handle
        .db(|db| db::get_lyric_sync_by_id(sync_id, db))
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "Auto-sync result disappeared after save".to_string())?;
    let _ = app_handle.emit("reload-track-id", track_id);
    Ok(sync)
}

async fn run_auto_sync_provider(
    track: PersistentTrack,
    config: PersistentConfig,
    plain_lyrics: String,
    language: String,
    app_handle: AppHandle,
) -> anyhow::Result<autosync::AutoSyncGenerated> {
    ensure_auto_sync_assets(&app_handle, &config).await?;
    let paths = autosync::qwen_engine_paths(&app_handle, &config)?;
    let (wav_path, output_path) = autosync::temp_sync_paths(&app_handle, track.id)?;
    let prepare_context = autosync::AutoSyncCommandContext {
        track_id: track.id,
        track_title: track.title.clone(),
        artist_name: track.artist_name.clone(),
        phase: "Preparing audio".to_string(),
    };
    let prepare_started_at = std::time::Instant::now();
    autosync::emit_auto_sync_engine_started(
        &app_handle,
        &prepare_context,
        "Decoding audio to 16 kHz mono WAV",
    );
    let input_path = std::path::PathBuf::from(&track.file_path);
    let wav_for_decode = wav_path.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        autosync::decode_audio_to_16khz_mono_wav(&input_path, &wav_for_decode)
    })
    .await
    .map_err(|err| anyhow::anyhow!("Audio preparation task failed: {err}"))?
    {
        autosync::emit_auto_sync_engine_failed(
            &app_handle,
            &prepare_context,
            &format!("Audio preparation failed: {error}"),
            prepare_started_at.elapsed(),
        );
        return Err(error);
    }
    autosync::emit_auto_sync_engine_finished(
        &app_handle,
        &prepare_context,
        "Audio prepared",
        prepare_started_at.elapsed(),
        None,
    );

    let _ = std::fs::remove_file(&output_path);
    let alignment_lyrics = autosync::normalize_plain_lyrics_for_alignment(&plain_lyrics);
    if alignment_lyrics.is_empty() {
        anyhow::bail!("Plain lyrics are empty after removing blank lines");
    }
    let direct_context = autosync::AutoSyncCommandContext {
        track_id: track.id,
        track_title: track.title.clone(),
        artist_name: track.artist_name.clone(),
        phase: "Direct align".to_string(),
    };
    let direct_args = autosync::build_qwen_direct_align_args(
        &paths,
        &wav_path,
        &alignment_lyrics,
        &language,
        &output_path,
    );
    let direct_result = autosync::run_qwen_alignment_command(
        &app_handle,
        direct_context,
        &paths.executable_path,
        &direct_args,
        &output_path,
    )
    .await;

    let command_output = match direct_result {
        Ok(command_output) => command_output,
        Err(direct_error) => {
            let _ = std::fs::remove_file(&output_path);
            let fallback_context = autosync::AutoSyncCommandContext {
                track_id: track.id,
                track_title: track.title.clone(),
                artist_name: track.artist_name.clone(),
                phase: "Fallback transcribe-align".to_string(),
            };
            let fallback_args =
                autosync::build_qwen_transcribe_align_args(&paths, &wav_path, &output_path);
            autosync::run_qwen_alignment_command(
                &app_handle,
                fallback_context,
                &paths.executable_path,
                &fallback_args,
                &output_path,
            )
            .await
            .with_context(|| format!("Direct alignment failed first: {direct_error}"))?
        }
    };

    let parse_context = autosync::AutoSyncCommandContext {
        track_id: track.id,
        track_title: track.title.clone(),
        artist_name: track.artist_name.clone(),
        phase: "Building LRC".to_string(),
    };
    autosync::emit_auto_sync_engine_started(
        &app_handle,
        &parse_context,
        "Parsing word timestamps and mapping them to lyric lines",
    );
    let started_at = std::time::Instant::now();
    let words =
        autosync::parse_qwen_alignment_json(&command_output.output_json).with_context(|| {
            let output = command_output.captured_output.trim();
            if output.is_empty() {
                "Failed to parse Qwen alignment output".to_string()
            } else {
                format!("Failed to parse Qwen alignment output. Engine output: {output}")
            }
        })?;
    let generated = autosync::generate_synced_lrc_from_words(&alignment_lyrics, &words)?;
    autosync::emit_auto_sync_engine_finished(
        &app_handle,
        &parse_context,
        "Generated synced lyrics",
        started_at.elapsed(),
        None,
    );
    Ok(generated)
}

async fn ensure_auto_sync_assets(
    app_handle: &AppHandle,
    config: &PersistentConfig,
) -> anyhow::Result<()> {
    let assets = autosync::list_auto_sync_assets(app_handle)?;
    let missing = assets
        .iter()
        .filter(|asset| !asset.installed)
        .map(|asset| asset.id.clone())
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Ok(());
    }

    if !config.auto_sync_auto_download {
        anyhow::bail!("Auto-sync assets are missing: {}", missing.join(", "));
    }

    for asset_id in missing {
        autosync::download_auto_sync_asset(app_handle.clone(), asset_id).await?;
    }

    Ok(())
}

async fn translate_track_lyrics_internal(
    track_id: i64,
    app_handle: AppHandle,
    force: bool,
) -> Result<LyricTranslation, String> {
    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lyricsfile_id = track
        .lyricsfile_id
        .ok_or_else(|| "No lyricsfile is stored for this track".to_string())?;
    let lyricsfile_content = track
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "No lyrics available to translate".to_string())?;
    let parsed =
        lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
    if parsed.is_instrumental {
        return Err("Instrumental tracks cannot be translated".to_string());
    }
    let source_lrc = parsed
        .synced_lyrics
        .clone()
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "Only synced lyrics can be auto-translated in this version".to_string())?;

    let provider = config.translation_provider.clone();
    let provider_model = translation::provider_model_from_config(&config);
    let target_language = translation::target_language_from_config(&config);
    let source_hash = translation::lyrics_source_hash(&lyricsfile_content);
    let settings_hash = translation::settings_hash_from_config(&config);

    if !force {
        if let Some(existing) = app_handle
            .db(|db| {
                db::get_current_lyric_translation(
                    lyricsfile_id,
                    &source_hash,
                    &provider,
                    &provider_model,
                    &target_language,
                    &settings_hash,
                    db,
                )
            })
            .map_err(|err| err.to_string())?
        {
            return Ok(existing);
        }
    }

    if let Some(skip) = translation::same_language_skip_decision(&source_lrc, &target_language)
        .map_err(|err| err.to_string())?
    {
        let message = format!(
            "Source lyrics are already {}; translation skipped.",
            skip.target_language
        );
        let upsert = LyricTranslationUpsert {
            lyricsfile_id,
            track_id: Some(track_id),
            source_hash,
            source_lyricsfile: lyricsfile_content,
            provider,
            provider_model,
            target_language,
            settings_hash,
            status: translation::TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE.to_string(),
            translated_lines_json: None,
            translated_lrc: None,
            error_message: Some(message),
            provider_metadata_json: Some(
                serde_json::json!({
                    "skipReason": skip.reason,
                    "detectedLanguage": skip.detected_language,
                    "detectedLanguageCode": skip.detected_language_code,
                    "targetLanguage": skip.target_language,
                    "targetLanguageCode": skip.target_language_code,
                    "confidence": skip.confidence,
                })
                .to_string(),
            ),
        };
        let id = app_handle
            .db(|db| db::upsert_lyric_translation(&upsert, db))
            .map_err(|err| err.to_string())?;
        let _ = app_handle.emit("reload-track-id", track_id);

        return Ok(LyricTranslation {
            id,
            lyricsfile_id: upsert.lyricsfile_id,
            track_id: upsert.track_id,
            source_hash: upsert.source_hash,
            source_lyricsfile: upsert.source_lyricsfile,
            provider: upsert.provider,
            provider_model: upsert.provider_model,
            target_language: upsert.target_language,
            settings_hash: upsert.settings_hash,
            status: upsert.status,
            translated_lines_json: upsert.translated_lines_json,
            translated_lrc: upsert.translated_lrc,
            error_message: upsert.error_message,
            provider_metadata_json: upsert.provider_metadata_json,
        });
    }

    let pending = LyricTranslationUpsert {
        lyricsfile_id,
        track_id: Some(track_id),
        source_hash: source_hash.clone(),
        source_lyricsfile: lyricsfile_content.clone(),
        provider: provider.clone(),
        provider_model: provider_model.clone(),
        target_language: target_language.clone(),
        settings_hash: settings_hash.clone(),
        status: translation::TRANSLATION_STATUS_PENDING.to_string(),
        translated_lines_json: None,
        translated_lrc: None,
        error_message: None,
        provider_metadata_json: None,
    };
    let _ = app_handle
        .db(|db| db::upsert_lyric_translation(&pending, db))
        .map_err(|err| err.to_string())?;
    let _ = app_handle.emit("reload-track-id", track_id);

    let request = TranslationRequest {
        title: track.title.clone(),
        album_name: track.album_name.clone(),
        artist_name: track.artist_name.clone(),
        source_language: None,
        target_language: target_language.clone(),
        source_lrc: source_lrc.clone(),
    };

    let mut attempt_reports = Vec::new();
    let mut result = None;

    for attempt in 1..=translation::TRANSLATION_PROVIDER_MAX_ATTEMPTS {
        let attempt_result = translation::request_translation(&config, &request)
            .await
            .and_then(|response_json| {
                translation::validate_translation_lines(&source_lrc, &response_json)?;
                let translated_lrc = translation::build_translated_lrc(
                    &source_lrc,
                    &response_json,
                    translation::TranslationExportMode::Translation,
                )?;
                Ok((response_json, translated_lrc))
            });

        match attempt_result {
            Ok(payload) => {
                result = Some(Ok(payload));
                break;
            }
            Err(error) => {
                let retryable = translation::should_retry_translation_error(&error);
                let error_report = translation::translation_error_report(&error);
                attempt_reports.push(translation::TranslationAttemptReport {
                    attempt,
                    retryable,
                    error: error_report.clone(),
                });

                if retryable && attempt < translation::TRANSLATION_PROVIDER_MAX_ATTEMPTS {
                    tokio::time::sleep(translation::translation_retry_delay(attempt)).await;
                    continue;
                }

                result = Some(Err(error_report));
                break;
            }
        }
    }

    let result = result.unwrap_or_else(|| Err("Translation did not run".to_string()));

    let upsert = match result {
        Ok((response_json, translated_lrc)) => LyricTranslationUpsert {
            status: translation::TRANSLATION_STATUS_SUCCEEDED.to_string(),
            translated_lines_json: Some(response_json),
            translated_lrc: Some(translated_lrc),
            error_message: None,
            provider_metadata_json: Some(
                translation::translation_provider_metadata_json(
                    &provider,
                    &provider_model,
                    &attempt_reports,
                    true,
                )
                .map_err(|err| err.to_string())?,
            ),
            ..pending
        },
        Err(error) => LyricTranslationUpsert {
            status: translation::TRANSLATION_STATUS_FAILED.to_string(),
            error_message: Some(if attempt_reports.len() > 1 {
                format!(
                    "Translation failed after {} attempts. Last error: {}",
                    attempt_reports.len(),
                    error
                )
            } else {
                error
            }),
            provider_metadata_json: Some(
                translation::translation_provider_metadata_json(
                    &provider,
                    &provider_model,
                    &attempt_reports,
                    false,
                )
                .map_err(|err| err.to_string())?,
            ),
            ..pending
        },
    };

    let id = app_handle
        .db(|db| db::upsert_lyric_translation(&upsert, db))
        .map_err(|err| err.to_string())?;
    let _ = app_handle.emit("reload-track-id", track_id);

    if upsert.status == translation::TRANSLATION_STATUS_FAILED {
        return Err(upsert
            .error_message
            .unwrap_or_else(|| "Translation failed".to_string()));
    }

    Ok(LyricTranslation {
        id,
        lyricsfile_id: upsert.lyricsfile_id,
        track_id: upsert.track_id,
        source_hash: upsert.source_hash,
        source_lyricsfile: upsert.source_lyricsfile,
        provider: upsert.provider,
        provider_model: upsert.provider_model,
        target_language: upsert.target_language,
        settings_hash: upsert.settings_hash,
        status: upsert.status,
        translated_lines_json: upsert.translated_lines_json,
        translated_lrc: upsert.translated_lrc,
        error_message: upsert.error_message,
        provider_metadata_json: upsert.provider_metadata_json,
    })
}

#[tauri::command]
async fn download_lyrics(track_id: i64, app_handle: AppHandle) -> Result<String, String> {
    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let lrclib_response = lrclib::get::request_raw(
        &track.title,
        &track.album_name,
        &track.artist_name,
        track.duration,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;
    let resolved = resolve_lrclib_lyrics_payload(lrclib_response)?;

    // Build lyricsfile content from the resolved response
    let lyricsfile_content = if let Some(ref provided) = resolved.provided_lyricsfile {
        provided.clone()
    } else {
        // Build a lyricsfile from the plain/synced lyrics
        crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new(
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
            ),
            Some(&resolved.plain_lyrics),
            Some(&resolved.synced_lyrics),
        )
        .ok_or_else(|| "Failed to build lyricsfile")?
    };

    // Upsert the lyricsfile record (handles presence fields automatically)
    app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_track(
                track.id,
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    app_handle.emit("reload-track-id", track_id).unwrap();
    maybe_spawn_auto_translation(track_id, app_handle.clone());
    if !resolved.is_instrumental
        && resolved.synced_lyrics.is_empty()
        && !resolved.plain_lyrics.is_empty()
    {
        maybe_spawn_auto_sync(track_id, app_handle.clone());
    }

    if resolved.is_instrumental {
        Ok("Marked track as instrumental".to_owned())
    } else if !resolved.synced_lyrics.is_empty() {
        Ok("Synced lyrics downloaded".to_owned())
    } else if !resolved.plain_lyrics.is_empty() {
        Ok("Plain lyrics downloaded".to_owned())
    } else {
        Err(LRCLIB_TRACK_NOT_FOUND.to_owned())
    }
}

#[tauri::command]
async fn apply_lyrics(
    track_id: i64,
    lrclib_response: lrclib::get::RawResponse,
    app_handle: AppHandle,
) -> Result<String, String> {
    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;

    let resolved = resolve_lrclib_lyrics_payload(lrclib_response)?;

    // Build lyricsfile content from the resolved response
    let lyricsfile_content = if let Some(ref provided) = resolved.provided_lyricsfile {
        provided.clone()
    } else {
        // Build a lyricsfile from the plain/synced lyrics
        crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new(
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
            ),
            Some(&resolved.plain_lyrics),
            Some(&resolved.synced_lyrics),
        )
        .ok_or_else(|| "Failed to build lyricsfile")?
    };

    // Upsert the lyricsfile record (handles presence fields automatically)
    app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_track(
                track.id,
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    std::thread::spawn({
        let app_handle = app_handle.clone();
        move || {
            app_handle.emit("reload-track-id", track_id).unwrap();
        }
    });
    maybe_spawn_auto_translation(track_id, app_handle.clone());
    if !resolved.is_instrumental
        && resolved.synced_lyrics.is_empty()
        && !resolved.plain_lyrics.is_empty()
    {
        maybe_spawn_auto_sync(track_id, app_handle.clone());
    }

    if resolved.is_instrumental {
        Ok("Marked track as instrumental".to_owned())
    } else if !resolved.synced_lyrics.is_empty() {
        Ok("Synced lyrics downloaded".to_owned())
    } else if !resolved.plain_lyrics.is_empty() {
        Ok("Plain lyrics downloaded".to_owned())
    } else {
        Err(LRCLIB_TRACK_NOT_FOUND.to_owned())
    }
}

#[tauri::command]
async fn retrieve_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    duration: f64,
    app_handle: AppHandle,
) -> Result<lrclib::get::RawResponse, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let response = lrclib::get::request_raw(
        &title,
        &album_name,
        &artist_name,
        duration,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;

    Ok(response)
}

#[tauri::command]
async fn retrieve_lyrics_by_id(
    id: i64,
    app_handle: AppHandle,
) -> Result<lrclib::get_by_id::RawResponse, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let response = lrclib::get_by_id::request_raw(id, &config.lrclib_instance)
        .await
        .map_err(|err| err.to_string())?;

    Ok(response)
}

#[tauri::command]
async fn search_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    q: String,
    app_handle: AppHandle,
) -> Result<lrclib::search::Response, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let response = lrclib::search::request(
        &title,
        &album_name,
        &artist_name,
        &q,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;

    Ok(response)
}

/// Result of preparing a lyricsfile from LRCLIB
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PrepareLyricsfileResult {
    pub lyricsfile_id: i64,
    pub lyricsfile: String,
    pub plain_lyrics: String,
    pub synced_lyrics: String,
    pub is_instrumental: bool,
    pub exists_in_db: bool,
}

#[tauri::command]
async fn prepare_lrclib_lyricsfile(
    lrclib_id: i64,
    app_handle: AppHandle,
) -> Result<PrepareLyricsfileResult, String> {
    // Get config for LRCLIB instance URL
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lrclib_instance = config.lrclib_instance;

    // Check if we already have this lyricsfile in the database
    let existing = app_handle
        .db(|db: &Connection| db::get_lyricsfile_by_lrclib(&lrclib_instance, lrclib_id, db))
        .map_err(|err| err.to_string())?;

    if let Some((lyricsfile_id, lyricsfile)) = existing {
        // Already exists, parse and return
        let parsed = lyricsfile::parse_lyricsfile(&lyricsfile).map_err(|e| e.to_string())?;
        return Ok(PrepareLyricsfileResult {
            lyricsfile_id,
            lyricsfile,
            plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
            synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
            is_instrumental: parsed.is_instrumental,
            exists_in_db: true,
        });
    }

    // Fetch from LRCLIB API
    let lrclib_response = lrclib::get_by_id::request_raw(lrclib_id, &lrclib_instance)
        .await
        .map_err(|err| err.to_string())?;

    // Extract metadata from LRCLIB response
    let title = lrclib_response.name.unwrap_or_default();
    let album_name = lrclib_response.album_name.unwrap_or_default();
    let artist_name = lrclib_response.artist_name.unwrap_or_default();
    let duration = lrclib_response.duration.unwrap_or(0.0);

    // Build or use existing lyricsfile content
    let lyricsfile_content = if let Some(lyricsfile) = lrclib_response.lyricsfile {
        // LRCLIB provided a lyricsfile, use it directly
        lyricsfile
    } else {
        // Need to build lyricsfile from plain/synced lyrics
        let metadata =
            lyricsfile::LyricsfileTrackMetadata::new(&title, &album_name, &artist_name, duration);

        let plain = lrclib_response.plain_lyrics.as_deref();
        let synced = lrclib_response.synced_lyrics.as_deref();

        lyricsfile::build_lyricsfile(&metadata, plain, synced)
            .ok_or("Failed to build lyricsfile from LRCLIB response")?
    };

    // Parse for return values
    let parsed = lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|e| e.to_string())?;

    // Save to database (track_id will be NULL)
    let lyricsfile_id = app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_lrclib(
                &lrclib_instance,
                lrclib_id,
                &title,
                &album_name,
                &artist_name,
                duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    Ok(PrepareLyricsfileResult {
        lyricsfile_id,
        lyricsfile: lyricsfile_content,
        plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
        synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
        is_instrumental: parsed.is_instrumental,
        exists_in_db: false,
    })
}

#[tauri::command]
async fn refresh_lrclib_lyricsfile(
    lrclib_id: i64,
    app_handle: AppHandle,
) -> Result<PrepareLyricsfileResult, String> {
    // Get config for LRCLIB instance URL
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lrclib_instance = config.lrclib_instance;

    // Fetch fresh data from LRCLIB API (always re-download)
    let lrclib_response = lrclib::get_by_id::request_raw(lrclib_id, &lrclib_instance)
        .await
        .map_err(|err| err.to_string())?;

    // Extract metadata from LRCLIB response
    let title = lrclib_response.name.unwrap_or_default();
    let album_name = lrclib_response.album_name.unwrap_or_default();
    let artist_name = lrclib_response.artist_name.unwrap_or_default();
    let duration = lrclib_response.duration.unwrap_or(0.0);

    // Build or use existing lyricsfile content
    let lyricsfile_content = if let Some(lyricsfile) = lrclib_response.lyricsfile {
        // LRCLIB provided a lyricsfile, use it directly
        lyricsfile
    } else {
        // Need to build lyricsfile from plain/synced lyrics
        let metadata =
            lyricsfile::LyricsfileTrackMetadata::new(&title, &album_name, &artist_name, duration);

        let plain = lrclib_response.plain_lyrics.as_deref();
        let synced = lrclib_response.synced_lyrics.as_deref();

        lyricsfile::build_lyricsfile(&metadata, plain, synced)
            .ok_or("Failed to build lyricsfile from LRCLIB response")?
    };

    // Parse for return values
    let parsed = lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|e| e.to_string())?;

    // Save to database (will update existing if lrclib_instance + lrclib_id match)
    let lyricsfile_id = app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_lrclib(
                &lrclib_instance,
                lrclib_id,
                &title,
                &album_name,
                &artist_name,
                duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    Ok(PrepareLyricsfileResult {
        lyricsfile_id,
        lyricsfile: lyricsfile_content,
        plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
        synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
        is_instrumental: parsed.is_instrumental,
        exists_in_db: true, // After refresh, it definitely exists
    })
}

#[tauri::command]
async fn save_lyrics(
    track_id: Option<i64>,
    lyricsfile_id: Option<i64>,
    lyricsfile: String,
    app_handle: AppHandle,
) -> Result<String, String> {
    let lyricsfile = lyricsfile.trim();

    // Parse the lyricsfile content to validate it
    let _parsed = lyricsfile::parse_lyricsfile(lyricsfile).map_err(|err| err.to_string())?;

    // If we have a track_id, this is a library track - update the lyricsfile record
    if let Some(id) = track_id {
        let track = app_handle
            .db(|db| db::get_track_by_id(id, db))
            .map_err(|err| err.to_string())?;

        // Update or create the lyricsfile record (presence fields are set automatically)
        app_handle
            .db(|db: &Connection| {
                db::upsert_lyricsfile_for_track(
                    track.id,
                    &track.title,
                    &track.album_name,
                    &track.artist_name,
                    track.duration,
                    lyricsfile,
                    db,
                )
            })
            .map_err(|err| err.to_string())?;

        app_handle.emit("reload-track-id", id).unwrap();
    } else if let Some(id) = lyricsfile_id {
        // Standalone lyricsfile update (LRCLIB flow)
        app_handle
            .db(|db: &Connection| db::update_lyricsfile_by_id(id, lyricsfile, db))
            .map_err(|err| err.to_string())?;
    } else {
        return Err("Either track_id or lyricsfile_id must be provided".to_string());
    }

    Ok("Lyrics saved successfully".to_owned())
}

#[tauri::command]
async fn publish_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    duration: f64,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    lyricsfile: Option<String>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let plain_lyrics = plain_lyrics.and_then(|lyrics| {
        let trimmed = lyrics.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });
    let synced_lyrics = synced_lyrics.and_then(|lyrics| {
        let trimmed = lyrics.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });
    let lyricsfile = lyricsfile.and_then(|content| {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(content)
        }
    });

    if plain_lyrics.is_none() && synced_lyrics.is_none() && lyricsfile.is_none() {
        return Err("No lyrics payload provided for publishing".to_owned());
    }

    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let mut progress = PublishLyricsProgress {
        request_challenge: "Pending".to_owned(),
        solve_challenge: "Pending".to_owned(),
        publish_lyrics: "Pending".to_owned(),
    };
    progress.request_challenge = "In Progress".to_owned();
    app_handle
        .emit("publish-lyrics-progress", &progress)
        .unwrap();
    let challenge_response = lrclib::request_challenge::request(&config.lrclib_instance)
        .await
        .map_err(|err| err.to_string())?;
    progress.request_challenge = "Done".to_owned();
    progress.solve_challenge = "In Progress".to_owned();
    app_handle
        .emit("publish-lyrics-progress", &progress)
        .unwrap();
    let nonce = lrclib::challenge_solver::solve_challenge(
        &challenge_response.prefix,
        &challenge_response.target,
    );
    progress.solve_challenge = "Done".to_owned();
    progress.publish_lyrics = "In Progress".to_owned();
    app_handle
        .emit("publish-lyrics-progress", &progress)
        .unwrap();
    let publish_token = format!("{}:{}", challenge_response.prefix, nonce);
    lrclib::publish::request(
        &title,
        &album_name,
        &artist_name,
        duration,
        plain_lyrics.as_deref(),
        synced_lyrics.as_deref(),
        lyricsfile.as_deref(),
        &publish_token,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;
    progress.publish_lyrics = "Done".to_owned();
    app_handle
        .emit("publish-lyrics-progress", &progress)
        .unwrap();
    Ok(())
}

#[tauri::command]
async fn flag_lyrics(
    track_id: i64,
    flag_reason: String,
    app_handle: AppHandle,
) -> Result<(), String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let mut progress = FlagLyricsProgress {
        request_challenge: "Pending".to_owned(),
        solve_challenge: "Pending".to_owned(),
        flag_lyrics: "Pending".to_owned(),
    };
    progress.request_challenge = "In Progress".to_owned();
    app_handle.emit("flag-lyrics-progress", &progress).unwrap();
    let challenge_response = lrclib::request_challenge::request(&config.lrclib_instance)
        .await
        .map_err(|err| err.to_string())?;
    progress.request_challenge = "Done".to_owned();
    progress.solve_challenge = "In Progress".to_owned();
    app_handle.emit("flag-lyrics-progress", &progress).unwrap();
    let nonce = lrclib::challenge_solver::solve_challenge(
        &challenge_response.prefix,
        &challenge_response.target,
    );
    progress.solve_challenge = "Done".to_owned();
    progress.flag_lyrics = "In Progress".to_owned();
    app_handle.emit("flag-lyrics-progress", &progress).unwrap();
    let publish_token = format!("{}:{}", challenge_response.prefix, nonce);
    lrclib::flag::request(
        track_id,
        &flag_reason,
        &publish_token,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;
    progress.flag_lyrics = "Done".to_owned();
    app_handle.emit("flag-lyrics-progress", &progress).unwrap();
    Ok(())
}

#[tauri::command]
async fn play_track(
    track_id: Option<i64>,
    file_path: Option<String>,
    title: Option<String>,
    album_name: Option<String>,
    artist_name: Option<String>,
    album_artist_name: Option<String>,
    duration: Option<f64>,
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let playable_track: PlayableTrack = if let Some(id) = track_id {
        // Database track - fetch and convert
        let db_track = app_handle
            .db(|db| db::get_track_by_id(id, db))
            .map_err(|err| err.to_string())?;
        PlayableTrack::from(db_track)
    } else if let Some(path) = file_path {
        // File-based track - create from metadata
        let path_obj = std::path::Path::new(&path);
        let file_name = path_obj
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        // Extract metadata from file if not provided
        let (resolved_title, resolved_duration) = if title.is_none() || duration.is_none() {
            match scanner::metadata::TrackMetadata::from_path(path_obj) {
                Ok(metadata) => (
                    title.unwrap_or(metadata.title),
                    duration.unwrap_or(metadata.duration),
                ),
                Err(_) => (
                    title.unwrap_or_else(|| file_name.clone()),
                    duration.unwrap_or(0.0),
                ),
            }
        } else {
            (title.unwrap(), duration.unwrap())
        };

        PlayableTrack {
            id: None,
            file_path: path,
            file_name,
            title: resolved_title,
            album_name: album_name.unwrap_or_default(),
            artist_name: artist_name.unwrap_or_default(),
            album_artist_name,
            image_path: None,
            track_number: None,
            duration: resolved_duration,
            instrumental: false,
            lyricsfile: None,
            lyricsfile_id: None,
            translation_status: "none".to_string(),
            translation_target_language: None,
            auto_sync_status: "none".to_string(),
            auto_sync_confidence: None,
        }
    } else {
        return Err("Either track_id or file_path must be provided".to_string());
    };

    let mut player_guard = app_state.player.lock().unwrap();
    let Some(ref mut player) = *player_guard else {
        return Err("Audio player is not initialized".to_string());
    };
    let direct_error = match player.play(playable_track.clone()) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    drop(player_guard);

    if !should_try_playback_wav_fallback(&playable_track.file_path) {
        return Err(direct_error.to_string());
    }

    let fallback_path = prepare_playback_wav_fallback(&app_handle, &playable_track.file_path)
        .map_err(|fallback_error| {
            format!(
                "Direct playback failed: {direct_error}. Fallback WAV preparation failed: {fallback_error}"
            )
        })?;
    let mut fallback_track = playable_track;
    fallback_track.file_path = fallback_path.to_string_lossy().to_string();

    let mut player_guard = app_state.player.lock().map_err(|error| error.to_string())?;
    let Some(ref mut player) = *player_guard else {
        return Err("Audio player is not initialized".to_string());
    };
    player.play(fallback_track).map_err(|fallback_error| {
        format!("Direct playback failed: {direct_error}. Fallback WAV playback failed: {fallback_error}")
    })?;

    Ok(())
}

fn should_try_playback_wav_fallback(file_path: &str) -> bool {
    !std::path::Path::new(file_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("wav"))
        .unwrap_or(false)
}

fn prepare_playback_wav_fallback(
    app_handle: &AppHandle,
    source_file_path: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let source_path = std::path::Path::new(source_file_path);
    let cache_path = playback_wav_fallback_path(app_handle, source_path)?;
    if cache_path.exists() {
        return Ok(cache_path);
    }

    let temp_path = cache_path.with_extension("wav.tmp");
    let _ = std::fs::remove_file(&temp_path);
    player::decode_audio_to_playback_wav(source_path, &temp_path)?;
    std::fs::rename(&temp_path, &cache_path).with_context(|| {
        format!(
            "Failed to move fallback WAV {} to {}",
            temp_path.display(),
            cache_path.display()
        )
    })?;
    Ok(cache_path)
}

fn playback_wav_fallback_path(
    app_handle: &AppHandle,
    source_path: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let metadata = std::fs::metadata(source_path)
        .with_context(|| format!("Failed to inspect audio file {}", source_path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let cache_key = xxhash_rust::xxh3::xxh3_64(
        format!("{}:{}:{modified}", source_path.display(), metadata.len()).as_bytes(),
    );
    let cache_dir = app_handle
        .path()
        .app_cache_dir()
        .context("Failed to resolve app cache directory")?
        .join("playback");
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join(format!("{cache_key:016x}.wav")))
}

#[tauri::command]
async fn export_lyrics(
    track_id: i64,
    formats: Vec<ExportLyricsFormat>,
    lyricsfile: Option<String>,
    app_handle: AppHandle,
) -> Result<Vec<export::ExportResult>, String> {
    if formats.is_empty() {
        return Err("Select at least one export format".to_owned());
    }

    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;

    let lyricsfile_content = lyricsfile
        .filter(|content| !content.trim().is_empty())
        .or_else(|| {
            track
                .lyricsfile
                .clone()
                .filter(|content| !content.trim().is_empty())
        })
        .ok_or_else(|| "No lyrics available for export".to_owned())?;

    let parsed =
        lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
    let export_formats = formats.into_iter().map(Into::into).collect::<Vec<_>>();

    Ok(export::export_track(&track, &parsed, &export_formats))
}

fn resolve_configured_export_lyrics(
    track: &PersistentTrack,
    config: &PersistentConfig,
    db: &Connection,
) -> Result<Option<lyricsfile::ParsedLyricsfile>, String> {
    let lyricsfile_content = track
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "No lyrics available for export".to_owned())?;
    let parsed_original =
        lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
    let export_mode = translation::export_mode_from_str(&config.translation_export_mode);

    if export_mode == translation::TranslationExportMode::Original
        || parsed_original.is_instrumental
    {
        return Ok(Some(parsed_original));
    }

    let Some(source_lrc) = parsed_original
        .synced_lyrics
        .clone()
        .filter(|content| !content.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(lyricsfile_id) = track.lyricsfile_id else {
        return Ok(None);
    };
    let source_hash = translation::lyrics_source_hash(&lyricsfile_content);
    let provider_model = translation::provider_model_from_config(config);
    let settings_hash = translation::settings_hash_from_config(config);

    let translation = db::get_current_lyric_translation(
        lyricsfile_id,
        &source_hash,
        &config.translation_provider,
        &provider_model,
        &translation::target_language_from_config(config),
        &settings_hash,
        db,
    )
    .map_err(|err| err.to_string())?;

    let translation = match translation {
        Some(translation) => translation,
        None => return Ok(None),
    };
    let lrc = translation::build_export_lrc_for_translation_status(
        &source_lrc,
        &translation.status,
        translation.translated_lines_json.as_deref(),
        export_mode,
    )
    .map_err(|err| err.to_string())?;

    Ok(Some(lyricsfile::ParsedLyricsfile {
        plain_lyrics: Some(utils::strip_timestamp(&lrc)),
        synced_lyrics: Some(lrc),
        is_instrumental: false,
    }))
}

#[cfg(test)]
mod export_resolution_tests {
    use super::*;

    fn test_config(export_mode: &str) -> PersistentConfig {
        PersistentConfig {
            skip_tracks_with_synced_lyrics: false,
            skip_tracks_with_plain_lyrics: false,
            show_line_count: true,
            try_embed_lyrics: false,
            theme_mode: "auto".to_string(),
            lrclib_instance: "https://lrclib.net".to_string(),
            volume: 1.0,
            translation_auto_enabled: false,
            translation_target_language: "English".to_string(),
            translation_provider: "gemini".to_string(),
            translation_export_mode: export_mode.to_string(),
            translation_gemini_api_key: String::new(),
            translation_gemini_model: "gemini-flash-latest".to_string(),
            translation_deepl_api_key: String::new(),
            translation_google_api_key: String::new(),
            translation_microsoft_api_key: String::new(),
            translation_microsoft_region: String::new(),
            translation_openai_base_url: String::new(),
            translation_openai_api_key: String::new(),
            translation_openai_model: String::new(),
            auto_sync_enabled: false,
            auto_sync_backend: "qwen3_asr_cpp".to_string(),
            auto_sync_model: autosync::DEFAULT_AUTO_SYNC_MODEL.to_string(),
            auto_sync_aligner_model: autosync::DEFAULT_AUTO_SYNC_ALIGNER_MODEL.to_string(),
            auto_sync_save_policy: autosync::DEFAULT_AUTO_SYNC_SAVE_POLICY.to_string(),
            auto_sync_confidence_threshold: autosync::DEFAULT_AUTO_SYNC_CONFIDENCE_THRESHOLD,
            auto_sync_auto_download: true,
            auto_sync_language_override: String::new(),
        }
    }

    fn test_track(lyricsfile: String) -> PersistentTrack {
        PersistentTrack {
            id: 42,
            file_path: "C:/music/plain.flac".to_string(),
            file_name: "plain.flac".to_string(),
            title: "Plain".to_string(),
            album_name: "Album".to_string(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Artist".to_string(),
            artist_id: 1,
            image_path: None,
            track_number: None,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: Some(lyricsfile),
            lyricsfile_id: Some(7),
            duration: 180.0,
            instrumental: false,
            translation_status: "none".to_string(),
            translation_target_language: None,
            auto_sync_status: "none".to_string(),
            auto_sync_confidence: None,
        }
    }

    #[test]
    fn translated_export_resolution_skips_plain_only_lyrics_without_error() {
        let lyricsfile = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Plain", "Album", "Artist", 180.0),
            Some("Hello world"),
            None,
        )
        .unwrap();
        let db = Connection::open_in_memory().unwrap();

        let resolved = resolve_configured_export_lyrics(
            &test_track(lyricsfile),
            &test_config("translation"),
            &db,
        )
        .unwrap();

        assert!(resolved.is_none());
    }
}

/// Detail for a single format export result
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportFormatDetail {
    pub format: String,
    pub status: export::ExportStatus,
}

/// Result summary for track export (used by mass export)
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackExportSummary {
    pub success: bool,
    pub exported: i32,
    pub skipped: i32,
    pub errors: i32,
    pub message: String,
    pub details: Vec<ExportFormatDetail>,
}

#[tauri::command]
async fn export_track_lyrics(
    track_id: i64,
    formats: Vec<ExportLyricsFormat>,
    app_handle: AppHandle,
) -> Result<TrackExportSummary, String> {
    if formats.is_empty() {
        return Ok(TrackExportSummary {
            success: true,
            exported: 0,
            skipped: 0,
            errors: 0,
            message: "No formats selected".to_owned(),
            details: vec![],
        });
    }

    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;
    let config = app_handle
        .db(|db| db::get_config(db))
        .map_err(|err| err.to_string())?;

    if track.lyricsfile.is_none() {
        return Ok(TrackExportSummary {
            success: true,
            exported: 0,
            skipped: 1,
            errors: 0,
            message: "No lyrics available for this track".to_owned(),
            details: vec![],
        });
    }

    let parsed = app_handle
        .db(|db| resolve_configured_export_lyrics(&track, &config, db))
        .map_err(|err| err.to_string())?;

    let parsed = match parsed {
        Some(parsed) => parsed,
        None => {
            return Ok(TrackExportSummary {
                success: true,
                exported: 0,
                skipped: formats.len() as i32,
                errors: 0,
                message: "No matching translation available for selected export mode".to_owned(),
                details: vec![],
            });
        }
    };

    let export_formats = formats.into_iter().map(Into::into).collect::<Vec<_>>();

    let results = export::export_track(&track, &parsed, &export_formats);

    // Count results based on status
    let exported = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Success))
        .count() as i32;
    let skipped = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Skipped(_)))
        .count() as i32;
    let errors = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Error(_)))
        .count() as i32;

    // Build detailed results with status info
    let details: Vec<ExportFormatDetail> = results
        .iter()
        .map(|r| ExportFormatDetail {
            format: format!("{:?}", r.format).to_lowercase(),
            status: r.status.clone(),
        })
        .collect();

    // Build message
    let message = if errors > 0 {
        let error_details: Vec<String> = results
            .iter()
            .filter(|r| matches!(r.status, export::ExportStatus::Error(_)))
            .map(|r| {
                let msg = match &r.status {
                    export::ExportStatus::Error(msg) => msg.clone(),
                    _ => String::new(),
                };
                format!("{:?}: {}", r.format, msg)
            })
            .collect();
        format!(
            "Exported {}, skipped {}, {} error(s) - {}",
            exported,
            skipped,
            errors,
            error_details.join("; ")
        )
    } else if exported > 0 {
        if skipped > 0 {
            format!("Exported {}, skipped {}", exported, skipped)
        } else {
            format!("Exported {} format(s)", exported)
        }
    } else if skipped > 0 {
        format!("Skipped {} format(s)", skipped)
    } else {
        "No formats exported".to_owned()
    };

    Ok(TrackExportSummary {
        success: errors == 0,
        exported,
        skipped,
        errors,
        message,
        details,
    })
}

#[tauri::command]
async fn get_track_ids_with_lyrics(app_state: State<'_, AppState>) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = db::get_track_ids_with_lyrics(conn).map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
fn pause_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.pause();
    }

    Ok(())
}

#[tauri::command]
fn resume_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.resume();
    }

    Ok(())
}

#[tauri::command]
fn seek_track(position: f64, app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.seek(position);
    }

    Ok(())
}

#[tauri::command]
fn stop_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.stop();
    }

    Ok(())
}

#[tauri::command]
fn set_volume(
    volume: f64,
    app_state: tauri::State<AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.set_volume(volume);
    }
    drop(player_guard);

    // Persist volume to config
    app_handle
        .db(|db| db::set_volume_config(volume, db))
        .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
fn open_devtools(app_handle: AppHandle) {
    app_handle
        .get_webview_window("main")
        .unwrap()
        .open_devtools();
}

#[tauri::command]
fn drain_notifications(app_state: tauri::State<AppState>) -> Vec<Notify> {
    let mut queued_notifications = app_state.queued_notifications.lock().unwrap();
    let notifications = queued_notifications.drain(..).collect();
    notifications
}

#[tauri::command]
async fn read_text_file(file_path: String) -> Result<String, String> {
    std::fs::read_to_string(&file_path).map_err(|err| format!("Failed to read file: {}", err))
}

#[tokio::main]
async fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState {
            db: Default::default(),
            player: Default::default(),
            queued_notifications: std::sync::Mutex::new(Vec::new()),
        })
        .setup(|app| {
            let handle = app.handle();

            let app_state: State<AppState> = handle.state();
            let db = db::initialize_database(&handle).expect("Database initialize should succeed");
            *app_state.db.lock().unwrap() = Some(db);

            // Load config to get initial volume
            let initial_volume = handle
                .db(|db| db::get_config(db))
                .map(|config| config.volume)
                .unwrap_or(1.0);

            let maybe_player = Player::new(initial_volume);
            match maybe_player {
                Ok(player) => {
                    *app_state.player.lock().unwrap() = Some(player);
                }
                Err(e) => {
                    eprintln!("Failed to initialize audio player: {}", e);
                    let mut buf = app_state.queued_notifications.lock().unwrap();
                    buf.push(Notify {
                        message: format!("Failed to initialize audio player: {}", e),
                        notify_type: NotifyType::Error,
                    });
                }
            }

            let handle_clone = handle.clone();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(40));
                loop {
                    interval.tick().await;
                    {
                        let app_state: State<AppState> = handle_clone.state();
                        let player_guard = app_state.player.lock();

                        match player_guard {
                            Ok(mut player_guard) => {
                                if let Some(ref mut player) = *player_guard {
                                    player.renew_state();

                                    let emit_player_state =
                                        handle_clone.emit("player-state", &player);

                                    if let Err(e) = emit_player_state {
                                        eprintln!("Failed to emit player state: {}", e);
                                    }
                                }
                            }
                            Err(e) => eprintln!("Failed to lock player: {}", e),
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_directories,
            set_directories,
            get_init,
            get_config,
            set_config,
            get_translation_config,
            set_translation_config,
            get_auto_sync_config,
            set_auto_sync_config,
            uninitialize_library,
            full_scan_library,
            scan_library,
            get_tracks,
            get_track_ids,
            get_track,
            get_albums,
            get_album_ids,
            get_album,
            get_artists,
            get_artist_ids,
            get_artist,
            get_album_tracks,
            get_artist_tracks,
            get_album_track_ids,
            get_artist_track_ids,
            list_track_translations,
            get_track_ids_requiring_translation,
            prepare_existing_lyrics_translation_queue,
            translate_track_lyrics,
            test_translation_provider,
            list_auto_sync_assets,
            download_auto_sync_asset,
            test_auto_sync_engine,
            prepare_auto_sync_queue,
            auto_sync_track_lyrics,
            list_track_sync_results,
            apply_sync_result_to_lyricsfile,
            download_lyrics,
            apply_lyrics,
            retrieve_lyrics,
            retrieve_lyrics_by_id,
            search_lyrics,
            save_lyrics,
            publish_lyrics,
            export_lyrics,
            export_track_lyrics,
            get_track_ids_with_lyrics,
            flag_lyrics,
            play_track,
            pause_track,
            resume_track,
            seek_track,
            stop_track,
            set_volume,
            open_devtools,
            drain_notifications,
            find_matching_tracks,
            get_audio_metadata,
            prepare_search_query,
            prepare_lrclib_lyricsfile,
            refresh_lrclib_lyricsfile,
            read_text_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
