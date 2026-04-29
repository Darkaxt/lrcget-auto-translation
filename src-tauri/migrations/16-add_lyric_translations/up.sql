ALTER TABLE config_data ADD COLUMN translation_auto_enabled BOOLEAN DEFAULT 0;
ALTER TABLE config_data ADD COLUMN translation_target_language TEXT DEFAULT 'English';
ALTER TABLE config_data ADD COLUMN translation_provider TEXT DEFAULT 'gemini';
ALTER TABLE config_data ADD COLUMN translation_export_mode TEXT DEFAULT 'original';
ALTER TABLE config_data ADD COLUMN translation_gemini_api_key TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_gemini_model TEXT DEFAULT 'gemini-flash-latest';
ALTER TABLE config_data ADD COLUMN translation_deepl_api_key TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_google_api_key TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_microsoft_api_key TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_microsoft_region TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_openai_base_url TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_openai_api_key TEXT DEFAULT '';
ALTER TABLE config_data ADD COLUMN translation_openai_model TEXT DEFAULT '';

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
    FOREIGN KEY(lyricsfile_id) REFERENCES lyricsfiles(id) ON DELETE CASCADE,
    FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE SET NULL,
    UNIQUE(lyricsfile_id, source_hash, provider, provider_model, target_language, settings_hash)
);

CREATE INDEX idx_lyric_translations_track_id ON lyric_translations(track_id);
CREATE INDEX idx_lyric_translations_lyricsfile_id ON lyric_translations(lyricsfile_id);
CREATE INDEX idx_lyric_translations_status ON lyric_translations(status);
CREATE INDEX idx_lyric_translations_lookup ON lyric_translations(
    lyricsfile_id,
    source_hash,
    provider,
    provider_model,
    target_language,
    settings_hash,
    status
);
