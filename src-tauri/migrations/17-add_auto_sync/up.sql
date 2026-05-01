ALTER TABLE config_data ADD COLUMN auto_sync_enabled BOOLEAN DEFAULT 0;
ALTER TABLE config_data ADD COLUMN auto_sync_backend TEXT DEFAULT 'qwen3_asr_cpp';
ALTER TABLE config_data ADD COLUMN auto_sync_model TEXT DEFAULT 'qwen3-asr-0.6b-q8_0.gguf';
ALTER TABLE config_data ADD COLUMN auto_sync_aligner_model TEXT DEFAULT 'qwen3-forced-aligner-0.6b-q4_k_m.gguf';
ALTER TABLE config_data ADD COLUMN auto_sync_save_policy TEXT DEFAULT 'auto_high_confidence';
ALTER TABLE config_data ADD COLUMN auto_sync_confidence_threshold REAL DEFAULT 0.82;
ALTER TABLE config_data ADD COLUMN auto_sync_auto_download BOOLEAN DEFAULT 1;
ALTER TABLE config_data ADD COLUMN auto_sync_language_override TEXT DEFAULT '';

CREATE TABLE lyric_syncs (
    id INTEGER PRIMARY KEY,
    lyricsfile_id INTEGER NOT NULL,
    track_id INTEGER,
    source_hash TEXT NOT NULL,
    source_lyricsfile TEXT NOT NULL,
    audio_hash TEXT NOT NULL,
    backend TEXT NOT NULL,
    model TEXT NOT NULL,
    aligner_model TEXT NOT NULL,
    language TEXT NOT NULL,
    settings_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    generated_lrc TEXT,
    generated_lines_json TEXT,
    confidence REAL,
    metrics_json TEXT,
    error_message TEXT,
    engine_metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(lyricsfile_id) REFERENCES lyricsfiles(id) ON DELETE CASCADE,
    FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE SET NULL,
    UNIQUE(
        lyricsfile_id,
        source_hash,
        audio_hash,
        backend,
        model,
        aligner_model,
        language,
        settings_hash
    )
);

CREATE INDEX idx_lyric_syncs_track_id ON lyric_syncs(track_id);
CREATE INDEX idx_lyric_syncs_lyricsfile_id ON lyric_syncs(lyricsfile_id);
CREATE INDEX idx_lyric_syncs_status ON lyric_syncs(status);
CREATE INDEX idx_lyric_syncs_lookup ON lyric_syncs(
    lyricsfile_id,
    source_hash,
    audio_hash,
    backend,
    model,
    aligner_model,
    language,
    settings_hash,
    status
);
