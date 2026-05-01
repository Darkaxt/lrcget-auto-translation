use crate::lyricsfile::{lyrics_presence_from_lyricsfile, parse_lyricsfile, LyricsPresence};
use crate::persistent_entities::{
    PersistentAlbum, PersistentArtist, PersistentConfig, PersistentTrack,
};
use crate::scanner::models::DbTrack;
use crate::translation::{
    self, LyricTranslation, LyricTranslationUpsert, TRANSLATION_STATUS_PENDING,
    TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE, TRANSLATION_STATUS_SUCCEEDED,
};
use crate::utils::prepare_input;
use anyhow::Result;
use include_dir::{include_dir, Dir};
use indoc::indoc;
use rusqlite::{named_params, params, Connection, OptionalExtension};
use rusqlite_migration::Migrations;
use serde::Serialize;
use std::fs;
use tauri::{AppHandle, Manager};

static MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// Initializes the database connection, creating the .sqlite file if needed, and upgrading the
/// database if it's out of date.
pub fn initialize_database(app_handle: &AppHandle) -> Result<Connection, rusqlite::Error> {
    let app_dir = app_handle
        .path()
        .app_data_dir()
        .expect("The app data directory should exist.");
    fs::create_dir_all(&app_dir).expect("The app data directory should be created.");
    let sqlite_path = app_dir.join("db.sqlite3");

    println!("Database file path: {}", sqlite_path.display());

    let mut db = Connection::open(sqlite_path)?;

    db.pragma_update(None, "journal_mode", "WAL")?;

    let current_version: i64 = db
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap_or(-1);
    println!("[DB Init] Current user_version: {}", current_version);

    let migrations = Migrations::from_directory(&MIGRATIONS_DIR)
        .expect("Failed to load migrations from directory");

    match migrations.to_latest(&mut db) {
        Ok(()) => {
            let new_version: i64 = db
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap_or(-1);
            println!(
                "[DB Init] Migrations applied successfully. New user_version: {}",
                new_version
            );
        }
        Err(e) => {
            eprintln!("[DB Init] Failed to run database migrations: {:?}", e);
            panic!("Failed to run database migrations: {}", e);
        }
    }

    if let Err(error) = backfill_track_lyrics_presence(&db) {
        eprintln!("Failed to backfill track lyrics presence flags: {}", error);
    }
    if let Err(error) = repair_same_language_translation_rows(&db) {
        eprintln!("Failed to repair same-language translation rows: {}", error);
    }
    if let Err(error) = delete_stale_incomplete_translation_attempts(&db) {
        eprintln!(
            "Failed to delete stale incomplete translation rows: {}",
            error
        );
    }

    Ok(db)
}

// Backfill lyrics presence data from lyricsfile content
// This updates lyricsfiles records that may not have presence fields set
pub fn backfill_track_lyrics_presence(db: &Connection) -> Result<()> {
    let mut select_statement = db.prepare(indoc! {"
      SELECT
        lyricsfiles.id,
        lyricsfiles.lyricsfile
      FROM lyricsfiles
      WHERE lyricsfiles.has_plain_lyrics = 0
         AND lyricsfiles.has_synced_lyrics = 0
         AND lyricsfiles.has_word_synced_lyrics = 0
    "})?;

    let mut rows = select_statement.query([])?;
    let mut updates: Vec<(i64, LyricsPresence)> = Vec::new();

    while let Some(row) = rows.next()? {
        let lyricsfile_id: i64 = row.get("id")?;
        let lyricsfile_content: Option<String> = row.get("lyricsfile")?;

        let presence = match lyricsfile_content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        {
            Some(content) => lyrics_presence_from_lyricsfile(content).unwrap_or_default(),
            None => LyricsPresence::default(),
        };

        updates.push((lyricsfile_id, presence));
    }

    // Update lyricsfiles records with calculated presence
    for (lyricsfile_id, presence) in updates {
        db.execute(
            "UPDATE lyricsfiles SET has_plain_lyrics = ?, has_synced_lyrics = ?, has_word_synced_lyrics = ?, instrumental = ? WHERE id = ?",
            (
                presence.has_plain_lyrics,
                presence.has_synced_lyrics,
                presence.has_word_synced_lyrics,
                presence.is_instrumental,
                lyricsfile_id,
            ),
        )?;
    }

    Ok(())
}

pub fn repair_same_language_translation_rows(db: &Connection) -> Result<usize> {
    let mut statement = db.prepare(indoc! {"
        SELECT
            id,
            source_lyricsfile,
            target_language
        FROM lyric_translations
        WHERE status IN ('succeeded', 'failed', 'pending')
    "})?;
    let mut rows = statement.query([])?;
    let mut repairs = Vec::new();

    while let Some(row) = rows.next()? {
        let id: i64 = row.get("id")?;
        let source_lyricsfile: String = row.get("source_lyricsfile")?;
        let target_language: String = row.get("target_language")?;
        let Ok(parsed) = parse_lyricsfile(&source_lyricsfile) else {
            continue;
        };
        if parsed.is_instrumental {
            continue;
        }
        let Some(source_lrc) = parsed
            .synced_lyrics
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        else {
            continue;
        };
        let Some(skip) = translation::same_language_skip_decision(source_lrc, &target_language)?
        else {
            continue;
        };

        let error_message = format!(
            "Source lyrics are already {}; translation skipped.",
            skip.target_language
        );
        let metadata = serde_json::json!({
            "skipReason": skip.reason,
            "detectedLanguage": skip.detected_language,
            "detectedLanguageCode": skip.detected_language_code,
            "targetLanguage": skip.target_language,
            "targetLanguageCode": skip.target_language_code,
            "confidence": skip.confidence,
            "repairReason": "existing_translation_row_same_language"
        })
        .to_string();
        repairs.push((id, error_message, metadata));
    }
    drop(rows);
    drop(statement);

    for (id, error_message, metadata) in repairs.iter() {
        db.execute(
            indoc! {"
                UPDATE lyric_translations
                SET status = ?,
                    translated_lines_json = NULL,
                    translated_lrc = NULL,
                    error_message = ?,
                    provider_metadata_json = ?,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
            "},
            (
                TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE,
                error_message,
                metadata,
                id,
            ),
        )?;
    }

    Ok(repairs.len())
}

pub fn delete_stale_incomplete_translation_attempts(db: &Connection) -> Result<usize> {
    db.execute(
        indoc! {"
            DELETE FROM lyric_translations
            WHERE (
                status = ?
            )
            OR (
                status = 'failed'
                AND error_message = 'Previous translation attempt did not finish.'
                AND translated_lines_json IS NULL
                AND translated_lrc IS NULL
            )
        "},
        [TRANSLATION_STATUS_PENDING],
    )
    .map_err(Into::into)
}

pub fn get_directories(db: &Connection) -> Result<Vec<String>> {
    let mut statement = db.prepare("SELECT * FROM directories")?;
    let mut rows = statement.query([])?;
    let mut directories: Vec<String> = Vec::new();
    while let Some(row) = rows.next()? {
        let path: String = row.get("path")?;

        directories.push(path);
    }

    Ok(directories)
}

pub fn set_directories(directories: Vec<String>, db: &Connection) -> Result<()> {
    db.execute("DELETE FROM directories WHERE 1", ())?;
    let mut statement = db.prepare("INSERT INTO directories (path) VALUES (@path)")?;
    for directory in directories.iter() {
        statement.execute(named_params! { "@path": directory })?;
    }

    Ok(())
}

pub fn get_init(db: &Connection) -> Result<bool> {
    let mut statement = db.prepare("SELECT init FROM library_data LIMIT 1")?;
    let init: bool = statement.query_row([], |r| r.get(0))?;
    Ok(init)
}

pub fn set_init(init: bool, db: &Connection) -> Result<()> {
    let mut statement = db.prepare("UPDATE library_data SET init = ? WHERE 1")?;
    statement.execute([init])?;
    Ok(())
}

pub fn get_config(db: &Connection) -> Result<PersistentConfig> {
    let mut statement = db.prepare(indoc! {"
      SELECT
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
        translation_openai_model
      FROM config_data
      LIMIT 1
    "})?;
    let row = statement.query_row([], |r| {
        Ok(PersistentConfig {
            skip_tracks_with_synced_lyrics: r.get("skip_tracks_with_synced_lyrics")?,
            skip_tracks_with_plain_lyrics: r.get("skip_tracks_with_plain_lyrics")?,
            show_line_count: r.get("show_line_count")?,
            try_embed_lyrics: r.get("try_embed_lyrics")?,
            theme_mode: r.get("theme_mode")?,
            lrclib_instance: r.get("lrclib_instance")?,
            volume: r.get("volume")?,
            translation_auto_enabled: r.get("translation_auto_enabled")?,
            translation_target_language: r.get("translation_target_language")?,
            translation_provider: r.get("translation_provider")?,
            translation_export_mode: r.get("translation_export_mode")?,
            translation_gemini_api_key: r.get("translation_gemini_api_key")?,
            translation_gemini_model: r.get("translation_gemini_model")?,
            translation_deepl_api_key: r.get("translation_deepl_api_key")?,
            translation_google_api_key: r.get("translation_google_api_key")?,
            translation_microsoft_api_key: r.get("translation_microsoft_api_key")?,
            translation_microsoft_region: r.get("translation_microsoft_region")?,
            translation_openai_base_url: r.get("translation_openai_base_url")?,
            translation_openai_api_key: r.get("translation_openai_api_key")?,
            translation_openai_model: r.get("translation_openai_model")?,
        })
    })?;
    Ok(row)
}

pub fn set_config(
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
    db: &Connection,
) -> Result<()> {
    let mut statement = db.prepare(indoc! {"
      UPDATE config_data
      SET
        skip_tracks_with_synced_lyrics = ?,
        skip_tracks_with_plain_lyrics = ?,
        show_line_count = ?,
        try_embed_lyrics = ?,
        theme_mode = ?,
        lrclib_instance = ?,
        volume = ?,
        translation_auto_enabled = ?,
        translation_target_language = ?,
        translation_provider = ?,
        translation_export_mode = ?,
        translation_gemini_api_key = ?,
        translation_gemini_model = ?,
        translation_deepl_api_key = ?,
        translation_google_api_key = ?,
        translation_microsoft_api_key = ?,
        translation_microsoft_region = ?,
        translation_openai_base_url = ?,
        translation_openai_api_key = ?,
        translation_openai_model = ?
      WHERE 1
    "})?;
    statement.execute(params![
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
    ])?;
    Ok(())
}

pub fn set_volume_config(volume: f64, db: &Connection) -> Result<()> {
    let mut statement = db.prepare("UPDATE config_data SET volume = ? WHERE 1")?;
    statement.execute([volume])?;
    Ok(())
}

pub fn upsert_lyric_translation(
    translation: &LyricTranslationUpsert,
    db: &Connection,
) -> Result<i64> {
    db.execute(
        indoc! {"
            INSERT INTO lyric_translations (
                lyricsfile_id,
                track_id,
                source_hash,
                source_lyricsfile,
                provider,
                provider_model,
                target_language,
                settings_hash,
                status,
                translated_lines_json,
                translated_lrc,
                error_message,
                provider_metadata_json,
                created_at,
                updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(lyricsfile_id, source_hash, provider, provider_model, target_language, settings_hash)
            DO UPDATE SET
                track_id = excluded.track_id,
                source_lyricsfile = excluded.source_lyricsfile,
                status = excluded.status,
                translated_lines_json = excluded.translated_lines_json,
                translated_lrc = excluded.translated_lrc,
                error_message = excluded.error_message,
                provider_metadata_json = excluded.provider_metadata_json,
                updated_at = CURRENT_TIMESTAMP
        "},
        (
            translation.lyricsfile_id,
            translation.track_id,
            &translation.source_hash,
            &translation.source_lyricsfile,
            &translation.provider,
            &translation.provider_model,
            &translation.target_language,
            &translation.settings_hash,
            &translation.status,
            &translation.translated_lines_json,
            &translation.translated_lrc,
            &translation.error_message,
            &translation.provider_metadata_json,
        ),
    )?;

    let id = db.query_row(
        indoc! {"
            SELECT id
            FROM lyric_translations
            WHERE lyricsfile_id = ?
              AND source_hash = ?
              AND provider = ?
              AND provider_model = ?
              AND target_language = ?
              AND settings_hash = ?
            LIMIT 1
        "},
        (
            translation.lyricsfile_id,
            &translation.source_hash,
            &translation.provider,
            &translation.provider_model,
            &translation.target_language,
            &translation.settings_hash,
        ),
        |row| row.get(0),
    )?;

    Ok(id)
}

pub fn get_current_lyric_translation(
    lyricsfile_id: i64,
    source_hash: &str,
    provider: &str,
    provider_model: &str,
    target_language: &str,
    settings_hash: &str,
    db: &Connection,
) -> Result<Option<LyricTranslation>> {
    db.query_row(
        indoc! {"
            SELECT
                id,
                lyricsfile_id,
                track_id,
                source_hash,
                source_lyricsfile,
                provider,
                provider_model,
                target_language,
                settings_hash,
                status,
                translated_lines_json,
                translated_lrc,
                error_message,
                provider_metadata_json
            FROM lyric_translations
            WHERE lyricsfile_id = ?
              AND source_hash = ?
              AND provider = ?
              AND provider_model = ?
              AND target_language = ?
              AND settings_hash = ?
              AND status IN ('succeeded', 'skipped_same_language')
            LIMIT 1
        "},
        (
            lyricsfile_id,
            source_hash,
            provider,
            provider_model,
            target_language,
            settings_hash,
        ),
        |row| {
            Ok(LyricTranslation {
                id: row.get("id")?,
                lyricsfile_id: row.get("lyricsfile_id")?,
                track_id: row.get("track_id")?,
                source_hash: row.get("source_hash")?,
                source_lyricsfile: row.get("source_lyricsfile")?,
                provider: row.get("provider")?,
                provider_model: row.get("provider_model")?,
                target_language: row.get("target_language")?,
                settings_hash: row.get("settings_hash")?,
                status: row.get("status")?,
                translated_lines_json: row.get("translated_lines_json")?,
                translated_lrc: row.get("translated_lrc")?,
                error_message: row.get("error_message")?,
                provider_metadata_json: row.get("provider_metadata_json")?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedTranslationQueue {
    pub queued_track_ids: Vec<i64>,
    pub queued_count: usize,
    pub skipped_same_language_count: usize,
    pub already_current_count: usize,
    #[serde(skip_serializing)]
    pub changed_track_ids: Vec<i64>,
}

pub fn prepare_existing_lyrics_translation_queue(
    provider: &str,
    provider_model: &str,
    target_language: &str,
    settings_hash: &str,
    db: &Connection,
) -> Result<PreparedTranslationQueue> {
    let candidate_ids = get_track_ids_with_synced_lyrics(db)?;
    let mut prepared = PreparedTranslationQueue {
        queued_track_ids: Vec::new(),
        queued_count: 0,
        skipped_same_language_count: 0,
        already_current_count: 0,
        changed_track_ids: Vec::new(),
    };

    for track_id in candidate_ids {
        let track = get_track_by_id(track_id, db)?;
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
        let parsed = match parse_lyricsfile(lyricsfile_content) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if parsed.is_instrumental {
            continue;
        }
        let Some(source_lrc) = parsed
            .synced_lyrics
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        else {
            continue;
        };

        let source_hash = translation::lyrics_source_hash(lyricsfile_content);
        let existing = get_current_lyric_translation(
            lyricsfile_id,
            &source_hash,
            provider,
            provider_model,
            target_language,
            settings_hash,
            db,
        )?;

        if existing.is_some() {
            prepared.already_current_count += 1;
            continue;
        }

        if let Some(skip) = translation::same_language_skip_decision(source_lrc, target_language)? {
            let message = format!(
                "Source lyrics are already {}; translation skipped.",
                skip.target_language
            );
            let skipped = LyricTranslationUpsert {
                lyricsfile_id,
                track_id: Some(track_id),
                source_hash,
                source_lyricsfile: lyricsfile_content.to_string(),
                provider: provider.to_string(),
                provider_model: provider_model.to_string(),
                target_language: target_language.to_string(),
                settings_hash: settings_hash.to_string(),
                status: TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE.to_string(),
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
                        "prepareReason": "source_language_matches_target"
                    })
                    .to_string(),
                ),
            };
            upsert_lyric_translation(&skipped, db)?;
            prepared.skipped_same_language_count += 1;
            prepared.changed_track_ids.push(track_id);
            continue;
        }

        let pending = LyricTranslationUpsert {
            lyricsfile_id,
            track_id: Some(track_id),
            source_hash,
            source_lyricsfile: lyricsfile_content.to_string(),
            provider: provider.to_string(),
            provider_model: provider_model.to_string(),
            target_language: target_language.to_string(),
            settings_hash: settings_hash.to_string(),
            status: TRANSLATION_STATUS_PENDING.to_string(),
            translated_lines_json: None,
            translated_lrc: None,
            error_message: None,
            provider_metadata_json: None,
        };
        upsert_lyric_translation(&pending, db)?;
        prepared.queued_track_ids.push(track_id);
        prepared.queued_count += 1;
        prepared.changed_track_ids.push(track_id);
    }

    Ok(prepared)
}

pub fn get_track_translation_status(track_id: i64, db: &Connection) -> Result<String> {
    let status = db
        .query_row(
            indoc! {"
                SELECT status
                FROM lyric_translations
                WHERE track_id = ?
                ORDER BY updated_at DESC, id DESC
                LIMIT 1
            "},
            [track_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    Ok(match status.as_deref() {
        Some(TRANSLATION_STATUS_SUCCEEDED) => "translated".to_string(),
        Some(TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE) => "already_target_language".to_string(),
        Some("pending") => "pending".to_string(),
        Some("failed") => "failed".to_string(),
        Some(_) => "none".to_string(),
        None => "none".to_string(),
    })
}

pub fn list_lyric_translations_for_track(
    track_id: i64,
    db: &Connection,
) -> Result<Vec<LyricTranslation>> {
    let mut statement = db.prepare(indoc! {"
        SELECT
            id,
            lyricsfile_id,
            track_id,
            source_hash,
            source_lyricsfile,
            provider,
            provider_model,
            target_language,
            settings_hash,
            status,
            translated_lines_json,
            translated_lrc,
            error_message,
            provider_metadata_json
        FROM lyric_translations
        WHERE track_id = ?
        ORDER BY updated_at DESC, id DESC
    "})?;
    let mut rows = statement.query([track_id])?;
    let mut translations = Vec::new();

    while let Some(row) = rows.next()? {
        translations.push(LyricTranslation {
            id: row.get("id")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            track_id: row.get("track_id")?,
            source_hash: row.get("source_hash")?,
            source_lyricsfile: row.get("source_lyricsfile")?,
            provider: row.get("provider")?,
            provider_model: row.get("provider_model")?,
            target_language: row.get("target_language")?,
            settings_hash: row.get("settings_hash")?,
            status: row.get("status")?,
            translated_lines_json: row.get("translated_lines_json")?,
            translated_lrc: row.get("translated_lrc")?,
            error_message: row.get("error_message")?,
            provider_metadata_json: row.get("provider_metadata_json")?,
        });
    }

    Ok(translations)
}

pub fn find_artist(name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("SELECT id FROM artists WHERE name = ?")?;
    let id: i64 = statement.query_row([name], |r| r.get(0))?;
    Ok(id)
}

pub fn add_artist(name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("INSERT INTO artists (name, name_lower) VALUES (?, ?)")?;
    let row_id = statement.insert((name, prepare_input(name)))?;
    Ok(row_id)
}

pub fn find_album(name: &str, album_artist_name: &str, db: &Connection) -> Result<i64> {
    let mut statement =
        db.prepare("SELECT id FROM albums WHERE name = ? AND album_artist_name = ?")?;
    let id: i64 = statement.query_row((name, album_artist_name), |r| r.get(0))?;
    Ok(id)
}

pub fn add_album(name: &str, album_artist_name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("INSERT INTO albums (name, name_lower, album_artist_name, album_artist_name_lower) VALUES (?, ?, ?, ?)")?;
    let row_id = statement.insert((
        name,
        prepare_input(name),
        album_artist_name,
        prepare_input(album_artist_name),
    ))?;
    Ok(row_id)
}

pub fn get_track_by_id(id: i64, db: &Connection) -> Result<PersistentTrack> {
    let query = indoc! {"
    SELECT
      tracks.id,
      tracks.file_path,
      tracks.file_name,
      tracks.title,
      artists.name AS artist_name,
      tracks.artist_id,
      albums.name AS album_name,
      albums.album_artist_name,
      tracks.album_id,
      tracks.duration,
      tracks.track_number,
      albums.image_path,
      lyricsfiles.id AS lyricsfile_id,
      lyricsfiles.lyricsfile,
      COALESCE(lyricsfiles.instrumental, 0) AS instrumental,
      COALESCE((
        SELECT CASE status
          WHEN 'succeeded' THEN 'translated'
          WHEN 'skipped_same_language' THEN 'already_target_language'
          WHEN 'pending' THEN 'pending'
          WHEN 'failed' THEN 'failed'
          ELSE 'none'
        END
        FROM lyric_translations
        WHERE lyric_translations.track_id = tracks.id
        ORDER BY updated_at DESC, id DESC
        LIMIT 1
      ), 'none') AS translation_status,
      (
        SELECT target_language
        FROM lyric_translations
        WHERE lyric_translations.track_id = tracks.id
        ORDER BY updated_at DESC, id DESC
        LIMIT 1
      ) AS translation_target_language
    FROM tracks
    JOIN albums ON tracks.album_id = albums.id
    JOIN artists ON tracks.artist_id = artists.id
    LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    WHERE tracks.id = ?
    LIMIT 1
  "};

    let mut statement = db.prepare(query)?;
    let row = statement.query_row([id], |row| {
        let is_instrumental: bool = row.get("instrumental")?;

        Ok(PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
            translation_status: row.get("translation_status")?,
            translation_target_language: row.get("translation_target_language")?,
        })
    })?;
    Ok(row)
}

pub fn upsert_lyricsfile_for_track(
    track_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<()> {
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            track_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(track_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            track_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    Ok(())
}

pub fn upsert_lyricsfile_for_track_tx(
    track_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    tx.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            track_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(track_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            track_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    Ok(())
}

pub fn delete_lyricsfile_by_track_id(track_id: i64, db: &Connection) -> Result<()> {
    db.execute("DELETE FROM lyricsfiles WHERE track_id = ?", [track_id])?;
    Ok(())
}

/// Get lyricsfile by LRCLIB instance and ID
/// Returns (lyricsfile_id, lyricsfile_content) if found
pub fn get_lyricsfile_by_lrclib(
    lrclib_instance: &str,
    lrclib_id: i64,
    db: &Connection,
) -> Result<Option<(i64, String)>> {
    let result = db
        .query_row(
            "SELECT id, lyricsfile FROM lyricsfiles WHERE lrclib_instance = ? AND lrclib_id = ?",
            [lrclib_instance, &lrclib_id.to_string()],
            |row| {
                let id: i64 = row.get(0)?;
                let lyricsfile: String = row.get(1)?;
                Ok((id, lyricsfile))
            },
        )
        .optional()?;
    Ok(result)
}

/// Upsert lyricsfile for LRCLIB track (standalone, no track association)
/// Returns the lyricsfile_id
pub fn upsert_lyricsfile_for_lrclib(
    lrclib_instance: &str,
    lrclib_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<i64> {
    // Calculate presence fields from lyricsfile content
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            lrclib_instance,
            lrclib_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(lrclib_instance, lrclib_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            lrclib_instance,
            lrclib_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    // Get the inserted/updated row ID
    let lyricsfile_id = db.query_row(
        "SELECT id FROM lyricsfiles WHERE lrclib_instance = ? AND lrclib_id = ?",
        [lrclib_instance, &lrclib_id.to_string()],
        |row| row.get(0),
    )?;

    Ok(lyricsfile_id)
}

/// Update lyricsfile content by ID (for standalone lyricsfiles without track association)
pub fn update_lyricsfile_by_id(
    lyricsfile_id: i64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<()> {
    // Calculate presence fields from lyricsfile content
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        "UPDATE lyricsfiles SET lyricsfile = ?, has_plain_lyrics = ?, has_synced_lyrics = ?, has_word_synced_lyrics = ?, instrumental = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        (
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
            lyricsfile_id,
        ),
    )?;
    Ok(())
}

/// Get lyricsfile by ID
/// Returns (lyricsfile_id, track_id, lyricsfile_content) if found
pub fn get_lyricsfile_by_id(
    lyricsfile_id: i64,
    db: &Connection,
) -> Result<Option<(i64, Option<i64>, String)>> {
    let result = db
        .query_row(
            "SELECT id, track_id, lyricsfile FROM lyricsfiles WHERE id = ?",
            [lyricsfile_id],
            |row| {
                let id: i64 = row.get(0)?;
                let track_id: Option<i64> = row.get(1)?;
                let lyricsfile: String = row.get(2)?;
                Ok((id, track_id, lyricsfile))
            },
        )
        .optional()?;
    Ok(result)
}

pub fn get_tracks(db: &Connection) -> Result<Vec<PersistentTrack>> {
    let query = indoc! {"
      SELECT
          tracks.id, tracks.file_path, tracks.file_name, tracks.title,
          artists.name AS artist_name, tracks.artist_id,
          albums.name AS album_name, albums.album_artist_name, tracks.album_id, tracks.duration, tracks.track_number,
          albums.image_path, lyricsfiles.id AS lyricsfile_id, lyricsfiles.lyricsfile, COALESCE(lyricsfiles.instrumental, 0) AS instrumental
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      ORDER BY tracks.title_lower ASC
  "};
    let mut statement = db.prepare(query)?;
    let mut rows = statement.query([])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
            translation_status: "none".to_string(),
            translation_target_language: None,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_track_ids(
    synced_lyrics: bool,
    plain_lyrics: bool,
    instrumental: bool,
    no_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    // Join with lyricsfiles table and use COALESCE to handle tracks without lyricsfiles
    let base_query = indoc! {"
        SELECT tracks.id 
        FROM tracks 
        LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    "};

    let mut included_categories: Vec<String> = Vec::new();
    if synced_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_synced_lyrics, 0) = 1 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if plain_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 1 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if instrumental {
        included_categories.push("COALESCE(lyricsfiles.instrumental, 0) = 1".to_string());
    }
    if no_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }

    let where_clause = if included_categories.len() == 4 {
        String::new()
    } else if included_categories.is_empty() {
        " WHERE 0".to_string()
    } else {
        format!(" WHERE ({})", included_categories.join(" OR "))
    };

    let full_query = format!(
        "{}{} ORDER BY tracks.title_lower ASC",
        base_query, where_clause
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

pub fn get_search_track_ids(
    query_str: &String,
    synced_lyrics: bool,
    plain_lyrics: bool,
    instrumental: bool,
    no_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let base_query = indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN artists ON tracks.artist_id = artists.id
      JOIN albums ON tracks.album_id = albums.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE (artists.name_lower LIKE ?
      OR albums.name_lower LIKE ?
      OR tracks.title_lower LIKE ?)
    "};

    let mut included_categories: Vec<String> = Vec::new();
    if synced_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_synced_lyrics, 0) = 1 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if plain_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 1 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if instrumental {
        included_categories.push("COALESCE(lyricsfiles.instrumental, 0) = 1".to_string());
    }
    if no_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }

    let where_clause = if included_categories.len() == 4 {
        String::new()
    } else if included_categories.is_empty() {
        " AND 0".to_string()
    } else {
        format!(" AND ({})", included_categories.join(" OR "))
    };

    let full_query = format!(
        "{}{} ORDER BY tracks.title_lower ASC",
        base_query, where_clause
    );

    let mut statement = db.prepare(&full_query)?;
    let formatted_query_str = format!("%{}%", prepare_input(query_str));
    let mut rows = statement.query(params![
        formatted_query_str,
        formatted_query_str,
        formatted_query_str
    ])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

pub fn get_albums(db: &Connection) -> Result<Vec<PersistentAlbum>> {
    let mut statement = db.prepare(indoc! {"
      SELECT albums.id, albums.name, albums.album_artist_name AS album_artist_name, albums.album_artist_name,
          COUNT(tracks.id) AS tracks_count
      FROM albums
      JOIN tracks ON tracks.album_id = albums.id
      GROUP BY albums.id, albums.name, albums.album_artist_name
      ORDER BY albums.name_lower ASC
  "})?;
    let mut rows = statement.query([])?;
    let mut albums: Vec<PersistentAlbum> = Vec::new();

    while let Some(row) = rows.next()? {
        let album = PersistentAlbum {
            id: row.get("id")?,
            name: row.get("name")?,
            image_path: row.get("image_path")?,
            artist_name: row.get("album_artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            tracks_count: row.get("tracks_count")?,
        };

        albums.push(album);
    }

    Ok(albums)
}

pub fn get_album_by_id(id: i64, db: &Connection) -> Result<PersistentAlbum> {
    let mut statement = db.prepare(indoc! {"
    SELECT
      albums.id,
      albums.name,
      albums.album_artist_name,
      COUNT(tracks.id) AS tracks_count
    FROM albums
    JOIN tracks ON tracks.album_id = albums.id
    WHERE albums.id = ?
    GROUP BY
      albums.id,
      albums.name,
      albums.album_artist_name
    LIMIT 1
  "})?;
    let row = statement.query_row([id], |row| {
        Ok(PersistentAlbum {
            id: row.get("id")?,
            name: row.get("name")?,
            image_path: None,
            artist_name: row.get("album_artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            tracks_count: row.get("tracks_count")?,
        })
    })?;
    Ok(row)
}

pub fn get_album_ids(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare("SELECT id FROM albums ORDER BY name_lower ASC")?;
    let mut rows = statement.query([])?;
    let mut album_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        album_ids.push(row.get("id")?);
    }

    Ok(album_ids)
}

pub fn get_artists(db: &Connection) -> Result<Vec<PersistentArtist>> {
    let mut statement = db.prepare(indoc! {"
    SELECT artists.id, artists.name AS name, COUNT(tracks.id) AS tracks_count
    FROM artists
    JOIN tracks ON tracks.artist_id = artists.id
    GROUP BY artists.id, artists.name
    ORDER BY artists.name_lower ASC
  "})?;
    let mut rows = statement.query([])?;
    let mut artists: Vec<PersistentArtist> = Vec::new();

    while let Some(row) = rows.next()? {
        let artist = PersistentArtist {
            id: row.get("id")?,
            name: row.get("name")?,
            // albums_count: row.get("albums_count")?,
            tracks_count: row.get("tracks_count")?,
        };

        artists.push(artist);
    }

    Ok(artists)
}

pub fn get_artist_by_id(id: i64, db: &Connection) -> Result<PersistentArtist> {
    let mut statement = db.prepare(indoc! {"
    SELECT artists.id,
      artists.name AS name,
      COUNT(tracks.id) AS tracks_count
    FROM artists
    JOIN tracks ON tracks.artist_id = artists.id
    WHERE artists.id = ?
    GROUP BY artists.id, artists.name
    LIMIT 1
  "})?;
    let row = statement.query_row([id], |row| {
        Ok(PersistentArtist {
            id: row.get("id")?,
            name: row.get("name")?,
            // albums_count: row.get("albums_count")?,
            tracks_count: row.get("tracks_count")?,
        })
    })?;
    Ok(row)
}

pub fn get_artist_ids(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare("SELECT id FROM artists ORDER BY name_lower ASC")?;
    let mut rows = statement.query([])?;
    let mut artist_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        artist_ids.push(row.get("id")?);
    }

    Ok(artist_ids)
}

pub fn get_album_tracks(album_id: i64, db: &Connection) -> Result<Vec<PersistentTrack>> {
    let mut statement = db.prepare(indoc! {"
    SELECT
      tracks.id,
      tracks.file_path,
      tracks.file_name,
      tracks.title,
      artists.name AS artist_name,
      tracks.artist_id,
      albums.name AS album_name,
      albums.album_artist_name,
      tracks.album_id,
      tracks.duration,
      tracks.track_number,
      albums.image_path,
      lyricsfiles.id AS lyricsfile_id,
      lyricsfiles.lyricsfile,
      COALESCE(lyricsfiles.instrumental, 0) AS instrumental
    FROM tracks
    JOIN albums ON tracks.album_id = albums.id
    JOIN artists ON tracks.artist_id = artists.id
    LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    WHERE tracks.album_id = ?
    ORDER BY tracks.track_number ASC
  "})?;
    let mut rows = statement.query([album_id])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_name: row.get("album_name")?,
            album_id: row.get("album_id")?,
            artist_id: row.get("artist_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
            translation_status: "none".to_string(),
            translation_target_language: None,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_album_track_ids(
    album_id: i64,
    without_plain_lyrics: bool,
    without_synced_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let base_query = indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.album_id = ?"};

    let lyrics_conditions = match (without_plain_lyrics, without_synced_lyrics) {
        (true, true) => {
            " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0"
        }
        (true, false) => " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, true) => " AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, false) => "",
    };

    let full_query = format!(
        "{}{} ORDER BY tracks.track_number ASC",
        base_query, lyrics_conditions
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([album_id])?;
    let mut tracks: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(row.get("id")?);
    }

    Ok(tracks)
}

pub fn get_artist_tracks(artist_id: i64, db: &Connection) -> Result<Vec<PersistentTrack>> {
    let mut statement = db.prepare(indoc! {"
      SELECT tracks.id, tracks.file_path, tracks.file_name, tracks.title, artists.name AS artist_name,
        tracks.artist_id, albums.name AS album_name, albums.album_artist_name, tracks.album_id, tracks.duration, tracks.track_number,
        albums.image_path, lyricsfiles.id AS lyricsfile_id, lyricsfiles.lyricsfile, COALESCE(lyricsfiles.instrumental, 0) AS instrumental
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.artist_id = ?
      ORDER BY albums.name_lower ASC, tracks.track_number ASC
  "})?;
    let mut rows = statement.query([artist_id])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
            translation_status: "none".to_string(),
            translation_target_language: None,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_artist_track_ids(
    artist_id: i64,
    without_plain_lyrics: bool,
    without_synced_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let base_query = indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.artist_id = ?"};

    let lyrics_conditions = match (without_plain_lyrics, without_synced_lyrics) {
        (true, true) => {
            " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0"
        }
        (true, false) => " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, true) => " AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, false) => "",
    };

    let full_query = format!(
        "{}{} ORDER BY albums.name_lower ASC, tracks.track_number ASC",
        base_query, lyrics_conditions
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([artist_id])?;
    let mut tracks: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(row.get("id")?);
    }

    Ok(tracks)
}

pub fn clean_library(db: &Connection) -> Result<()> {
    db.execute("DELETE FROM tracks WHERE 1", ())?;
    db.execute("DELETE FROM albums WHERE 1", ())?;
    db.execute("DELETE FROM artists WHERE 1", ())?;
    Ok(())
}

/// Get all tracks with their fingerprint data for comparison
pub fn get_tracks_with_fingerprints(db: &Connection) -> Result<Vec<DbTrack>> {
    let mut statement =
        db.prepare("SELECT id, file_path, file_size, modified_time, content_hash FROM tracks")?;
    let mut rows = statement.query([])?;
    let mut tracks = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(DbTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_size: row.get("file_size")?,
            modified_time: row.get("modified_time")?,
            content_hash: row.get("content_hash")?,
        });
    }

    Ok(tracks)
}

/// Delete tracks by their IDs (batch operation)
pub fn delete_tracks_by_ids(ids: &[i64], conn: &Connection) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    // Create placeholders for the IN clause
    let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
    let query = format!(
        "DELETE FROM tracks WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut statement = conn.prepare(&query)?;
    let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
    statement.execute(rusqlite::params_from_iter(params.iter()))?;

    Ok(())
}

/// Update a track's file path (for move/rename detection)
pub fn update_track_path(
    track_id: i64,
    new_path: &std::path::Path,
    conn: &Connection,
) -> Result<()> {
    let new_path_str = new_path.to_string_lossy().to_string();
    let file_name = new_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    conn.execute(
        "UPDATE tracks SET file_path = ?, file_name = ? WHERE id = ?",
        (new_path_str, file_name, track_id),
    )?;

    Ok(())
}

// ============================================================================
// Scan-related database operations (transaction-based)
// ============================================================================

const SCAN_STATUS_PENDING: i32 = 0;
const SCAN_STATUS_PROCESSED: i32 = 1;

/// Track info for scan operations
#[derive(Debug)]
pub struct ScanTrackInfo {
    pub id: i64,
    pub file_path: String,
}

/// Find track by fingerprint (mtime + size) - for scan operations
pub fn find_track_by_fingerprint_tx(
    modified_time: i64,
    file_size: i64,
    tx: &rusqlite::Transaction,
) -> Result<Option<ScanTrackInfo>> {
    let mut stmt = tx.prepare(
        "SELECT id, file_path FROM tracks WHERE modified_time = ? AND file_size = ? LIMIT 1",
    )?;

    let result = stmt
        .query_row([modified_time, file_size], |row| {
            Ok(ScanTrackInfo {
                id: row.get(0)?,
                file_path: row.get(1)?,
            })
        })
        .optional()?;

    Ok(result)
}

/// Find track by content hash - for scan operations
pub fn find_track_by_hash_tx(
    hash: &str,
    tx: &rusqlite::Transaction,
) -> Result<Option<ScanTrackInfo>> {
    let mut stmt = tx.prepare("SELECT id, file_path FROM tracks WHERE content_hash = ? LIMIT 1")?;

    let result = stmt
        .query_row([hash], |row| {
            Ok(ScanTrackInfo {
                id: row.get(0)?,
                file_path: row.get(1)?,
            })
        })
        .optional()?;

    Ok(result)
}

/// Mark track as processed during scan
pub fn mark_track_processed_tx(track_id: i64, tx: &rusqlite::Transaction) -> Result<()> {
    tx.execute(
        "UPDATE tracks SET scan_status = ? WHERE id = ?",
        [SCAN_STATUS_PROCESSED, track_id as i32],
    )?;
    Ok(())
}

/// Update track path after move (fingerprint already matches)
pub fn update_track_path_tx(
    track_id: i64,
    new_path: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let file_name = std::path::Path::new(new_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    tx.execute(
        "UPDATE tracks SET file_path = ?, file_name = ?, scan_status = ? WHERE id = ?",
        (new_path, file_name, SCAN_STATUS_PROCESSED, track_id),
    )?;

    Ok(())
}

/// Update track path and fingerprint after move
pub fn update_track_path_and_fingerprint_tx(
    track_id: i64,
    new_path: &str,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let file_name = std::path::Path::new(new_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    tx.execute(
        "UPDATE tracks SET file_path = ?, file_name = ?, file_size = ?, modified_time = ?, content_hash = ?, scan_status = ? WHERE id = ?",
        (new_path, file_name, file_size, modified_time, content_hash, SCAN_STATUS_PROCESSED, track_id),
    )?;

    Ok(())
}

/// Insert a new track from metadata during scan
/// Note: Lyrics are stored separately in the lyricsfiles table via upsert_lyricsfile_for_track_tx
pub fn insert_track_from_metadata_tx(
    metadata: &crate::scanner::metadata::TrackMetadata,
    _lyrics: &crate::scanner::metadata::LyricsInfo,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    artist_id: i64,
    album_id: i64,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    use crate::utils::prepare_input;

    tx.execute(
        "INSERT INTO tracks (file_path, file_name, title, title_lower, album_id, artist_id, duration, track_number, file_size, modified_time, content_hash, scan_status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &metadata.file_path,
            &metadata.file_name,
            &metadata.title,
            prepare_input(&metadata.title),
            album_id,
            artist_id,
            metadata.duration,
            metadata.track_number,
            file_size,
            modified_time,
            content_hash,
            SCAN_STATUS_PROCESSED,
        ],
    )?;

    Ok(tx.last_insert_rowid())
}

/// Delete tracks that weren't processed during scan and clean up orphaned albums/artists
pub fn delete_unprocessed_tracks(conn: &mut Connection) -> Result<usize> {
    let tx = conn.transaction()?;

    // Delete tracks that weren't seen during this scan
    let deleted_count = tx.execute(
        "DELETE FROM tracks WHERE scan_status = ?",
        [SCAN_STATUS_PENDING],
    )?;

    // Clean up orphaned albums and artists
    tx.execute(
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks)",
        [],
    )?;

    tx.execute(
        "DELETE FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM tracks)",
        [],
    )?;

    tx.commit()?;

    Ok(deleted_count)
}

/// Mark all tracks as pending before scan
pub fn mark_all_tracks_pending(conn: &mut Connection) -> Result<()> {
    conn.execute("UPDATE tracks SET scan_status = ?", [SCAN_STATUS_PENDING])?;
    Ok(())
}

// Transaction-based versions of artist/album functions for scan operations

/// Find artist by name (transaction version)
pub fn find_artist_tx(name: &str, tx: &rusqlite::Transaction) -> Result<i64> {
    let mut statement = tx.prepare("SELECT id FROM artists WHERE name = ?")?;
    let id: i64 = statement.query_row([name], |r| r.get(0))?;
    Ok(id)
}

/// Add new artist (transaction version)
pub fn add_artist_tx(name: &str, tx: &rusqlite::Transaction) -> Result<i64> {
    let mut statement = tx.prepare("INSERT INTO artists (name, name_lower) VALUES (?, ?)")?;
    let row_id = statement.insert((name, prepare_input(name)))?;
    Ok(row_id)
}

/// Find album by name and artist (transaction version)
pub fn find_album_tx(
    name: &str,
    album_artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    let mut statement =
        tx.prepare("SELECT id FROM albums WHERE name = ? AND album_artist_name = ?")?;
    let id: i64 = statement.query_row((name, album_artist_name), |r| r.get(0))?;
    Ok(id)
}

/// Add new album (transaction version)
pub fn add_album_tx(
    name: &str,
    album_artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    let mut statement = tx.prepare("INSERT INTO albums (name, name_lower, album_artist_name, album_artist_name_lower) VALUES (?, ?, ?, ?)")?;
    let row_id = statement.insert((
        name,
        prepare_input(name),
        album_artist_name,
        prepare_input(album_artist_name),
    ))?;
    Ok(row_id)
}

/// Get track IDs that have lyrics (for mass export)
pub fn get_track_ids_with_lyrics(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare(indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE lyricsfiles.has_plain_lyrics = 1
         OR lyricsfiles.has_synced_lyrics = 1
      ORDER BY tracks.artist_id ASC, tracks.album_id ASC, tracks.track_number ASC NULLS LAST
    "})?;

    let mut rows = statement.query([])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

/// Get track IDs with stored synced lyrics that can be translated from the local database.
pub fn get_track_ids_with_synced_lyrics(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare(indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE lyricsfiles.has_synced_lyrics = 1
        AND lyricsfiles.instrumental = 0
      ORDER BY tracks.artist_id ASC, tracks.album_id ASC, tracks.track_number ASC NULLS LAST
    "})?;

    let mut rows = statement.query([])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

/// Find orphaned lyricsfiles by track metadata (for reattachment during scan)
/// Returns the lyricsfile_id if found, None otherwise
/// Matches on normalized title, artist, album, and duration within ±2 seconds
pub fn find_orphaned_lyricsfile_tx(
    title: &str,
    artist_name: &str,
    album_name: &str,
    duration: f64,
    tx: &rusqlite::Transaction,
) -> Result<Option<i64>> {
    let result = tx
        .query_row(
            indoc! {"
                SELECT id FROM lyricsfiles
                WHERE track_id IS NULL
                  AND track_title_lower = ?
                  AND track_artist_name_lower = ?
                  AND track_album_name_lower = ?
                  AND ABS(track_duration - ?) <= 2.0
                LIMIT 1
            "},
            [
                prepare_input(title),
                prepare_input(artist_name),
                prepare_input(album_name),
                duration.to_string(),
            ],
            |row| {
                let id: i64 = row.get(0)?;
                Ok(id)
            },
        )
        .optional()?;
    Ok(result)
}

/// Reattach an orphaned lyricsfile to a track
pub fn reattach_lyricsfile_to_track_tx(
    lyricsfile_id: i64,
    track_id: i64,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "UPDATE lyricsfiles SET track_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        (track_id, lyricsfile_id),
    )?;
    Ok(())
}

/// Find tracks by metadata for matching against LRCLIB tracks
pub fn find_tracks_by_metadata(
    title: &str,
    artist_name: Option<&str>,
    album_name: Option<&str>,
    duration: Option<f64>,
    db: &Connection,
) -> Result<Vec<PersistentTrack>> {
    let normalized_title = prepare_input(title);
    let normalized_artist = artist_name.map(prepare_input);
    let normalized_album = album_name.map(prepare_input);

    // Build the query based on available metadata
    // We search using normalized (name_lower) fields for case-insensitive matching
    let mut conditions = vec!["tracks.title_lower = ?"];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(normalized_title.clone())];

    if let Some(ref artist) = normalized_artist {
        conditions.push("artists.name_lower = ?");
        params.push(Box::new(artist.clone()));
    }

    if let Some(ref album) = normalized_album {
        conditions.push("albums.name_lower = ?");
        params.push(Box::new(album.clone()));
    }

    if let Some(dur) = duration {
        conditions.push("ABS(tracks.duration - ?) <= 2.0");
        params.push(Box::new(dur));
    }

    let where_clause = conditions.join(" AND ");

    let query = format!(
        indoc! {r#"
        SELECT
            tracks.id,
            tracks.file_path,
            tracks.file_name,
            tracks.title,
            artists.name AS artist_name,
            tracks.artist_id,
            albums.name AS album_name,
            albums.album_artist_name,
            tracks.album_id,
            tracks.duration,
            tracks.track_number,
            albums.image_path,
            lyricsfiles.id AS lyricsfile_id,
            lyricsfiles.lyricsfile,
            COALESCE(lyricsfiles.instrumental, 0) AS instrumental
        FROM tracks
        JOIN albums ON tracks.album_id = albums.id
        JOIN artists ON tracks.artist_id = artists.id
        LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
        WHERE {}
        ORDER BY tracks.title_lower ASC
    "#},
        where_clause
    );

    let mut statement = db.prepare(&query)?;

    // Convert params to rusqlite params
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut rows = statement.query(param_refs.as_slice())?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
            translation_status: "none".to_string(),
            translation_target_language: None,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

#[cfg(test)]
mod translation_db_tests {
    use super::*;
    use crate::lyricsfile::{build_lyricsfile, LyricsfileTrackMetadata};

    fn setup_translation_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(indoc! {"
            CREATE TABLE lyric_translations (
                id INTEGER PRIMARY KEY,
                lyricsfile_id INTEGER NOT NULL,
                track_id INTEGER,
                source_hash TEXT NOT NULL,
                source_lyricsfile TEXT NOT NULL,
                provider TEXT NOT NULL,
                provider_model TEXT NOT NULL,
                target_language TEXT NOT NULL,
                settings_hash TEXT NOT NULL,
                status TEXT NOT NULL,
                translated_lines_json TEXT,
                translated_lrc TEXT,
                error_message TEXT,
                provider_metadata_json TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(lyricsfile_id, source_hash, provider, provider_model, target_language, settings_hash)
            );
        "})
        .unwrap();
        db
    }

    fn setup_db() -> Connection {
        setup_translation_db()
    }

    fn setup_library_db() -> Connection {
        let db = setup_translation_db();
        db.execute_batch(indoc! {"
            CREATE TABLE artists (
                id INTEGER PRIMARY KEY,
                name TEXT,
                name_lower TEXT
            );
            CREATE TABLE albums (
                id INTEGER PRIMARY KEY,
                name TEXT,
                name_lower TEXT,
                artist_id INTEGER,
                image_path TEXT,
                album_artist_name TEXT,
                album_artist_name_lower TEXT
            );
            CREATE TABLE tracks (
                id INTEGER PRIMARY KEY,
                file_path TEXT,
                file_name TEXT,
                title TEXT,
                title_lower TEXT,
                album_id INTEGER,
                artist_id INTEGER,
                duration FLOAT,
                track_number INTEGER,
                file_size INTEGER,
                modified_time INTEGER,
                content_hash TEXT,
                scan_status INTEGER DEFAULT 1
            );
            CREATE TABLE lyricsfiles (
                id INTEGER PRIMARY KEY,
                track_id INTEGER UNIQUE,
                track_title TEXT,
                track_title_lower TEXT,
                track_album_name TEXT,
                track_album_name_lower TEXT,
                track_artist_name TEXT,
                track_artist_name_lower TEXT,
                track_duration FLOAT,
                lyricsfile TEXT,
                has_plain_lyrics BOOLEAN NOT NULL DEFAULT 0,
                has_synced_lyrics BOOLEAN NOT NULL DEFAULT 0,
                has_word_synced_lyrics BOOLEAN NOT NULL DEFAULT 0,
                instrumental BOOLEAN NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO artists (id, name, name_lower) VALUES (1, 'Test Artist', 'test artist');
            INSERT INTO albums (
                id,
                name,
                name_lower,
                artist_id,
                image_path,
                album_artist_name,
                album_artist_name_lower
            ) VALUES (1, 'Test Album', 'test album', 1, NULL, 'Test Artist', 'test artist');
        "})
            .unwrap();
        db
    }

    fn successful_translation(source_hash: &str) -> LyricTranslationUpsert {
        LyricTranslationUpsert {
            lyricsfile_id: 7,
            track_id: Some(42),
            source_hash: source_hash.to_string(),
            source_lyricsfile: "version: '1.0'\nlines: []".to_string(),
            provider: "gemini".to_string(),
            provider_model: "gemini-flash-latest".to_string(),
            target_language: "English".to_string(),
            settings_hash: "settings-a".to_string(),
            status: "succeeded".to_string(),
            translated_lines_json: Some(
                r#"{"lines":[{"source_index":0,"translated_text":"Hello"}]}"#.to_string(),
            ),
            translated_lrc: Some("[00:01.00]Hello".to_string()),
            error_message: None,
            provider_metadata_json: Some(r#"{"requestedModel":"gemini-flash-latest"}"#.to_string()),
        }
    }

    fn lyricsfile_from_lrc(lrc: &str) -> String {
        build_lyricsfile(
            &LyricsfileTrackMetadata::new("Test Track", "Test Album", "Test Artist", 180.0),
            None,
            Some(lrc),
        )
        .unwrap()
    }

    fn insert_synced_track(
        db: &Connection,
        track_id: i64,
        lyricsfile_id: i64,
        title: &str,
        lrc: &str,
    ) -> String {
        let lyricsfile = lyricsfile_from_lrc(lrc);
        db.execute(
            indoc! {"
                INSERT INTO tracks (
                    id,
                    file_path,
                    file_name,
                    title,
                    title_lower,
                    album_id,
                    artist_id,
                    duration,
                    track_number,
                    scan_status
                ) VALUES (?, ?, ?, ?, ?, 1, 1, 180.0, ?, 1)
            "},
            (
                track_id,
                format!("C:/music/{}.flac", title),
                format!("{}.flac", title),
                title,
                title.to_lowercase(),
                track_id,
            ),
        )
        .unwrap();
        db.execute(
            indoc! {"
                INSERT INTO lyricsfiles (
                    id,
                    track_id,
                    track_title,
                    track_title_lower,
                    track_album_name,
                    track_album_name_lower,
                    track_artist_name,
                    track_artist_name_lower,
                    track_duration,
                    lyricsfile,
                    has_plain_lyrics,
                    has_synced_lyrics,
                    has_word_synced_lyrics,
                    instrumental
                ) VALUES (?, ?, ?, ?, 'Test Album', 'test album', 'Test Artist', 'test artist', 180.0, ?, 1, 1, 0, 0)
            "},
            (
                lyricsfile_id,
                track_id,
                title,
                title.to_lowercase(),
                &lyricsfile,
            ),
        )
        .unwrap();
        lyricsfile
    }

    #[test]
    fn upserts_and_finds_current_successful_translation() {
        let db = setup_db();
        let input = successful_translation("source-a");

        let id = upsert_lyric_translation(&input, &db).unwrap();
        let found = get_current_lyric_translation(
            7,
            "source-a",
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap()
        .unwrap();

        assert_eq!(found.id, id);
        assert_eq!(found.status, "succeeded");
        assert_eq!(found.translated_lrc.as_deref(), Some("[00:01.00]Hello"));
    }

    #[test]
    fn does_not_return_stale_translation_for_changed_source_hash() {
        let db = setup_db();
        upsert_lyric_translation(&successful_translation("source-a"), &db).unwrap();

        let found = get_current_lyric_translation(
            7,
            "source-b",
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap();

        assert!(found.is_none());
    }

    #[test]
    fn reports_track_translation_status_from_latest_attempt() {
        let db = setup_db();
        let mut failed = successful_translation("source-a");
        failed.status = "failed".to_string();
        failed.error_message = Some("provider error".to_string());
        failed.translated_lines_json = None;
        failed.translated_lrc = None;

        upsert_lyric_translation(&failed, &db).unwrap();
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "failed");

        upsert_lyric_translation(&successful_translation("source-a"), &db).unwrap();
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "translated");
    }

    #[test]
    fn treats_same_language_skip_as_current_usable_translation() {
        let db = setup_db();
        let mut skipped = successful_translation("source-a");
        skipped.status = "skipped_same_language".to_string();
        skipped.translated_lines_json = None;
        skipped.translated_lrc = None;
        skipped.error_message = Some("Source lyrics are already English".to_string());
        skipped.provider_metadata_json = Some(
            r#"{"skipReason":"source_language_matches_target","detectedLanguage":"English","detectedLanguageCode":"en","targetLanguage":"English","targetLanguageCode":"en","confidence":0.99}"#
                .to_string(),
        );

        upsert_lyric_translation(&skipped, &db).unwrap();

        let found = get_current_lyric_translation(
            7,
            "source-a",
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap()
        .unwrap();

        assert_eq!(found.status, "skipped_same_language");
        assert_eq!(
            get_track_translation_status(42, &db).unwrap(),
            "already_target_language"
        );
    }

    #[test]
    fn prepare_translation_queue_marks_english_as_skipped_without_queueing_provider() {
        let db = setup_library_db();
        let lrc = "[00:01.00]I wake again\n[00:02.00]You got me up to my old ways again\n[00:03.00]One by one, two by two\n[00:04.00]My walls come falling down\n[00:05.00]Was lost and now I'm found";
        let lyricsfile = insert_synced_track(&db, 42, 7, "English Track", lrc);

        let prepared = prepare_existing_lyrics_translation_queue(
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap();

        assert!(prepared.queued_track_ids.is_empty());
        assert_eq!(prepared.queued_count, 0);
        assert_eq!(prepared.skipped_same_language_count, 1);
        assert_eq!(prepared.already_current_count, 0);
        assert_eq!(prepared.changed_track_ids, vec![42]);
        assert_eq!(
            get_track_translation_status(42, &db).unwrap(),
            "already_target_language"
        );

        let found = get_current_lyric_translation(
            7,
            &translation::lyrics_source_hash(&lyricsfile),
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.status, "skipped_same_language");
    }

    #[test]
    fn prepare_translation_queue_marks_korean_lyrics_pending_and_returns_track_id() {
        let db = setup_library_db();
        insert_synced_track(
            &db,
            42,
            7,
            "Korean Track",
            "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:02.00]When I open my eyes in the morning\n[00:03.00]햇살에 눈 비비고 일어나고",
        );

        let prepared = prepare_existing_lyrics_translation_queue(
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap();

        assert_eq!(prepared.queued_track_ids, vec![42]);
        assert_eq!(prepared.queued_count, 1);
        assert_eq!(prepared.skipped_same_language_count, 0);
        assert_eq!(prepared.already_current_count, 0);
        assert_eq!(prepared.changed_track_ids, vec![42]);
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "pending");
    }

    #[test]
    fn prepare_translation_queue_does_not_requeue_current_success_or_skip_rows() {
        let db = setup_library_db();
        let lyricsfile = insert_synced_track(
            &db,
            42,
            7,
            "Already Done",
            "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:02.00]햇살에 눈 비비고 일어나고",
        );
        let mut existing = successful_translation(&translation::lyrics_source_hash(&lyricsfile));
        existing.lyricsfile_id = 7;
        existing.track_id = Some(42);
        upsert_lyric_translation(&existing, &db).unwrap();

        let prepared = prepare_existing_lyrics_translation_queue(
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap();

        assert!(prepared.queued_track_ids.is_empty());
        assert_eq!(prepared.queued_count, 0);
        assert_eq!(prepared.skipped_same_language_count, 0);
        assert_eq!(prepared.already_current_count, 1);
        assert!(prepared.changed_track_ids.is_empty());
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "translated");
    }

    #[test]
    fn prepare_translation_queue_re_evaluates_failed_rows() {
        let db = setup_library_db();
        let korean_lyricsfile = insert_synced_track(
            &db,
            42,
            7,
            "Failed Korean",
            "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:02.00]햇살에 눈 비비고 일어나고",
        );
        let english_lyricsfile = insert_synced_track(
            &db,
            43,
            8,
            "Failed English",
            "[00:01.00]I wake again\n[00:02.00]You got me up to my old ways again\n[00:03.00]One by one, two by two\n[00:04.00]My walls come falling down\n[00:05.00]Was lost and now I'm found",
        );
        let mut failed_korean =
            successful_translation(&translation::lyrics_source_hash(&korean_lyricsfile));
        failed_korean.lyricsfile_id = 7;
        failed_korean.track_id = Some(42);
        failed_korean.status = "failed".to_string();
        failed_korean.translated_lines_json = None;
        failed_korean.translated_lrc = None;
        failed_korean.error_message = Some("provider error".to_string());
        upsert_lyric_translation(&failed_korean, &db).unwrap();

        let mut failed_english =
            successful_translation(&translation::lyrics_source_hash(&english_lyricsfile));
        failed_english.lyricsfile_id = 8;
        failed_english.track_id = Some(43);
        failed_english.status = "failed".to_string();
        failed_english.translated_lines_json = None;
        failed_english.translated_lrc = None;
        failed_english.error_message = Some("provider error".to_string());
        upsert_lyric_translation(&failed_english, &db).unwrap();

        let prepared = prepare_existing_lyrics_translation_queue(
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap();

        assert_eq!(prepared.queued_track_ids, vec![42]);
        assert_eq!(prepared.queued_count, 1);
        assert_eq!(prepared.skipped_same_language_count, 1);
        assert_eq!(prepared.already_current_count, 0);
        assert_eq!(prepared.changed_track_ids, vec![42, 43]);
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "pending");
        assert_eq!(
            get_track_translation_status(43, &db).unwrap(),
            "already_target_language"
        );
    }

    #[test]
    fn get_track_by_id_reports_translation_status_badges() {
        let db = setup_library_db();
        let lyricsfile = insert_synced_track(
            &db,
            42,
            7,
            "Badge Track",
            "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:02.00]햇살에 눈 비비고 일어나고",
        );
        let source_hash = translation::lyrics_source_hash(&lyricsfile);
        let mut row = successful_translation(&source_hash);
        row.lyricsfile_id = 7;
        row.track_id = Some(42);

        row.status = "pending".to_string();
        row.translated_lines_json = None;
        row.translated_lrc = None;
        upsert_lyric_translation(&row, &db).unwrap();
        assert_eq!(
            get_track_by_id(42, &db).unwrap().translation_status,
            "pending"
        );

        row.status = "succeeded".to_string();
        row.translated_lines_json = Some(
            r#"{"lines":[{"source_index":0,"translated_text":"Morning sunlight"}]}"#.to_string(),
        );
        row.translated_lrc = Some("[00:01.00]Morning sunlight".to_string());
        upsert_lyric_translation(&row, &db).unwrap();
        assert_eq!(
            get_track_by_id(42, &db).unwrap().translation_status,
            "translated"
        );

        row.status = "skipped_same_language".to_string();
        row.translated_lines_json = None;
        row.translated_lrc = None;
        row.error_message = Some("Source lyrics are already English".to_string());
        upsert_lyric_translation(&row, &db).unwrap();
        assert_eq!(
            get_track_by_id(42, &db).unwrap().translation_status,
            "already_target_language"
        );

        row.status = "failed".to_string();
        row.error_message = Some("provider error".to_string());
        upsert_lyric_translation(&row, &db).unwrap();
        assert_eq!(
            get_track_by_id(42, &db).unwrap().translation_status,
            "failed"
        );
    }

    #[test]
    fn repairs_existing_same_language_rows_to_skipped_status() {
        let db = setup_db();
        let source_lrc = "[00:01.00]I wake again\n[00:02.00]You got me up to my old ways again\n[00:03.00]One by one, two by two\n[00:04.00]My walls come falling down\n[00:05.00]Was lost and now I'm found";
        let mut old_failed = successful_translation("source-a");
        old_failed.status = "failed".to_string();
        old_failed.source_lyricsfile = lyricsfile_from_lrc(source_lrc);
        old_failed.translated_lines_json = None;
        old_failed.translated_lrc = None;
        old_failed.error_message = Some("HTTP status client error (400 Bad Request)".to_string());

        upsert_lyric_translation(&old_failed, &db).unwrap();

        assert_eq!(repair_same_language_translation_rows(&db).unwrap(), 1);

        let found = get_current_lyric_translation(
            7,
            "source-a",
            "gemini",
            "gemini-flash-latest",
            "English",
            "settings-a",
            &db,
        )
        .unwrap()
        .unwrap();

        assert_eq!(found.status, "skipped_same_language");
        assert_eq!(found.translated_lines_json, None);
        assert_eq!(found.translated_lrc, None);
        assert_eq!(
            get_track_translation_status(42, &db).unwrap(),
            "already_target_language"
        );
    }

    #[test]
    fn repair_does_not_reclassify_mixed_language_rows() {
        let db = setup_db();
        let source_lrc = "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:02.00]When I open my eyes in the morning\n[00:03.00]햇살에 눈 비비고 일어나고\n[00:04.00]A clean street, as if someone has swept it";
        let mut old_success = successful_translation("source-a");
        old_success.source_lyricsfile = lyricsfile_from_lrc(source_lrc);

        upsert_lyric_translation(&old_success, &db).unwrap();

        assert_eq!(repair_same_language_translation_rows(&db).unwrap(), 0);
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "translated");
    }

    #[test]
    fn deletes_pending_translation_rows_on_startup() {
        let db = setup_db();
        let mut pending = successful_translation("source-a");
        pending.status = "pending".to_string();
        pending.translated_lines_json = None;
        pending.translated_lrc = None;
        pending.error_message = None;

        upsert_lyric_translation(&pending, &db).unwrap();

        assert_eq!(
            delete_stale_incomplete_translation_attempts(&db).unwrap(),
            1
        );
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "none");
    }

    #[test]
    fn deletes_interrupted_failure_rows_from_previous_cleanup() {
        let db = setup_db();
        let mut failed = successful_translation("source-a");
        failed.status = "failed".to_string();
        failed.translated_lines_json = None;
        failed.translated_lrc = None;
        failed.error_message = Some("Previous translation attempt did not finish.".to_string());

        upsert_lyric_translation(&failed, &db).unwrap();

        assert_eq!(
            delete_stale_incomplete_translation_attempts(&db).unwrap(),
            1
        );
        assert_eq!(get_track_translation_status(42, &db).unwrap(), "none");
    }

    #[test]
    fn config_includes_translation_defaults() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(indoc! {"
            CREATE TABLE config_data (
                id INTEGER PRIMARY KEY,
                skip_tracks_with_synced_lyrics BOOLEAN DEFAULT 0,
                skip_tracks_with_plain_lyrics BOOLEAN DEFAULT 0,
                show_line_count BOOLEAN DEFAULT 1,
                try_embed_lyrics BOOLEAN DEFAULT 0,
                theme_mode TEXT DEFAULT 'auto',
                lrclib_instance TEXT DEFAULT 'https://lrclib.net',
                volume REAL DEFAULT 1.0,
                translation_auto_enabled BOOLEAN DEFAULT 0,
                translation_target_language TEXT DEFAULT 'English',
                translation_provider TEXT DEFAULT 'gemini',
                translation_export_mode TEXT DEFAULT 'original',
                translation_gemini_api_key TEXT DEFAULT '',
                translation_gemini_model TEXT DEFAULT 'gemini-flash-latest',
                translation_deepl_api_key TEXT DEFAULT '',
                translation_google_api_key TEXT DEFAULT '',
                translation_microsoft_api_key TEXT DEFAULT '',
                translation_microsoft_region TEXT DEFAULT '',
                translation_openai_base_url TEXT DEFAULT '',
                translation_openai_api_key TEXT DEFAULT '',
                translation_openai_model TEXT DEFAULT ''
            );
            INSERT INTO config_data (id) VALUES (1);
        "})
            .unwrap();

        let config = get_config(&db).unwrap();

        assert!(!config.translation_auto_enabled);
        assert_eq!(config.translation_target_language, "English");
        assert_eq!(config.translation_provider, "gemini");
        assert_eq!(config.translation_export_mode, "original");
        assert_eq!(config.translation_gemini_model, "gemini-flash-latest");
    }
}
