use crate::parser::lrc::format_timestamp;
use crate::persistent_entities::PersistentConfig;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use whatlang::{detect, Lang};
use xxhash_rust::xxh3::xxh3_64;

pub const AUTO_SYNC_BACKEND_QWEN3_ASR_CPP: &str = "qwen3_asr_cpp";
pub const DEFAULT_AUTO_SYNC_MODEL: &str = "qwen3-asr-0.6b-q8_0.gguf";
pub const DEFAULT_AUTO_SYNC_ALIGNER_MODEL: &str = "qwen3-forced-aligner-0.6b-q4_k_m.gguf";
pub const DEFAULT_AUTO_SYNC_SAVE_POLICY: &str = "auto_high_confidence";
pub const DEFAULT_AUTO_SYNC_CONFIDENCE_THRESHOLD: f64 = 0.82;

pub const AUTO_SYNC_STATUS_PENDING: &str = "pending";
pub const AUTO_SYNC_STATUS_SUCCEEDED: &str = "succeeded";
pub const AUTO_SYNC_STATUS_NEEDS_REVIEW: &str = "needs_review";
pub const AUTO_SYNC_STATUS_FAILED: &str = "failed";

const TARGET_SAMPLE_RATE: u32 = 16_000;
const DIRECT_ALIGN_TIMEOUT_SECONDS: u64 = 30 * 60;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimedWord {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncLine {
    pub index: usize,
    pub text: String,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub matched_words: usize,
    pub confidence: f64,
    pub interpolated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncMetrics {
    pub confidence: f64,
    pub matched_line_ratio: f64,
    pub average_word_similarity: f64,
    pub interpolated_line_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncGenerated {
    pub synced_lrc: String,
    pub lines: Vec<AutoSyncLine>,
    pub metrics: AutoSyncMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricSyncUpsert {
    pub lyricsfile_id: i64,
    pub track_id: Option<i64>,
    pub source_hash: String,
    pub source_lyricsfile: String,
    pub audio_hash: String,
    pub backend: String,
    pub model: String,
    pub aligner_model: String,
    pub language: String,
    pub settings_hash: String,
    pub status: String,
    pub generated_lrc: Option<String>,
    pub generated_lines_json: Option<String>,
    pub confidence: Option<f64>,
    pub metrics_json: Option<String>,
    pub error_message: Option<String>,
    pub engine_metadata_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricSync {
    pub id: i64,
    pub lyricsfile_id: i64,
    pub track_id: Option<i64>,
    pub source_hash: String,
    pub source_lyricsfile: String,
    pub audio_hash: String,
    pub backend: String,
    pub model: String,
    pub aligner_model: String,
    pub language: String,
    pub settings_hash: String,
    pub status: String,
    pub generated_lrc: Option<String>,
    pub generated_lines_json: Option<String>,
    pub confidence: Option<f64>,
    pub metrics_json: Option<String>,
    pub error_message: Option<String>,
    pub engine_metadata_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncAssetStatus {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub file_name: String,
    pub url: String,
    pub expected_size: u64,
    pub expected_sha256: String,
    pub installed: bool,
    pub path: String,
    pub bytes_on_disk: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncAssetProgress {
    pub asset_id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub struct QwenEnginePaths {
    pub executable_path: PathBuf,
    pub model_path: PathBuf,
    pub aligner_model_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AutoSyncCommandContext {
    pub track_id: i64,
    pub track_title: String,
    pub artist_name: String,
    pub phase: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncEngineEvent {
    pub track_id: i64,
    pub track_title: String,
    pub artist_name: String,
    pub phase: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

pub struct QwenCommandOutput {
    pub output_json: String,
    pub captured_output: String,
}

#[derive(Debug, Clone)]
struct AutoSyncAssetSpec {
    id: &'static str,
    kind: &'static str,
    name: &'static str,
    file_name: &'static str,
    url: &'static str,
    expected_size: u64,
    expected_sha256: &'static str,
    archive: bool,
}

pub fn provider_name_from_config(config: &PersistentConfig) -> String {
    let backend = config.auto_sync_backend.trim();
    if backend.is_empty() {
        AUTO_SYNC_BACKEND_QWEN3_ASR_CPP.to_string()
    } else {
        backend.to_string()
    }
}

pub fn model_from_config(config: &PersistentConfig) -> String {
    let model = config.auto_sync_model.trim();
    if model.is_empty() {
        DEFAULT_AUTO_SYNC_MODEL.to_string()
    } else {
        model.to_string()
    }
}

pub fn aligner_model_from_config(config: &PersistentConfig) -> String {
    let model = config.auto_sync_aligner_model.trim();
    if model.is_empty() {
        DEFAULT_AUTO_SYNC_ALIGNER_MODEL.to_string()
    } else {
        model.to_string()
    }
}

pub fn save_policy_from_config(config: &PersistentConfig) -> String {
    let policy = config.auto_sync_save_policy.trim();
    if policy.is_empty() {
        DEFAULT_AUTO_SYNC_SAVE_POLICY.to_string()
    } else {
        policy.to_string()
    }
}

pub fn settings_hash_from_config(config: &PersistentConfig) -> String {
    let settings = serde_json::json!({
        "backend": provider_name_from_config(config),
        "model": model_from_config(config),
        "alignerModel": aligner_model_from_config(config),
        "savePolicy": save_policy_from_config(config),
        "confidenceThreshold": config.auto_sync_confidence_threshold,
        "languageOverride": config.auto_sync_language_override,
    });
    format!("{:016x}", xxh3_64(settings.to_string().as_bytes()))
}

pub fn language_for_sync(config: &PersistentConfig, lyrics: &str) -> String {
    let override_language = config.auto_sync_language_override.trim();
    if !override_language.is_empty() {
        return override_language.to_string();
    }

    detect(lyrics)
        .map(|info| language_code(info.lang()).to_string())
        .unwrap_or_else(|| "auto".to_string())
}

fn language_code(lang: Lang) -> &'static str {
    match lang {
        Lang::Eng => "en",
        Lang::Kor => "ko",
        Lang::Jpn => "ja",
        Lang::Cmn => "zh",
        Lang::Deu => "de",
        Lang::Fra => "fr",
        Lang::Spa => "es",
        Lang::Ita => "it",
        Lang::Por => "pt",
        Lang::Rus => "ru",
        Lang::Ara => "ar",
        Lang::Hin => "hi",
        Lang::Tha => "th",
        Lang::Vie => "vi",
        Lang::Ind => "id",
        Lang::Tur => "tr",
        Lang::Pol => "pl",
        Lang::Nld => "nl",
        Lang::Swe => "sv",
        Lang::Nob => "no",
        Lang::Dan => "da",
        Lang::Fin => "fi",
        Lang::Ell => "el",
        Lang::Ces => "cs",
        Lang::Hun => "hu",
        Lang::Ron => "ro",
        Lang::Ukr => "uk",
        Lang::Heb => "he",
        _ => "auto",
    }
}

pub fn should_auto_apply_sync_result(
    policy: &str,
    threshold: f64,
    metrics: &AutoSyncMetrics,
) -> bool {
    match policy {
        "always" => true,
        "preview" => false,
        _ => {
            metrics.confidence >= threshold
                && metrics.matched_line_ratio >= 0.85
                && metrics.interpolated_line_ratio <= 0.20
        }
    }
}

pub fn parse_qwen_alignment_json(output: &str) -> Result<Vec<TimedWord>> {
    let value: Value = serde_json::from_str(output).context("Qwen alignment output is not JSON")?;
    let words = collect_word_values(&value);
    if words.is_empty() {
        bail!("Qwen alignment output did not contain any word timestamps");
    }

    let mut timed_words = Vec::with_capacity(words.len());
    for word in words {
        let text = word
            .get("word")
            .or_else(|| word.get("text"))
            .or_else(|| word.get("token"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }

        let (start_ms, end_ms) = read_word_timestamps(word)?;
        timed_words.push(TimedWord {
            text,
            start_ms,
            end_ms,
            probability: word
                .get("probability")
                .or_else(|| word.get("prob"))
                .or_else(|| word.get("score"))
                .and_then(Value::as_f64),
        });
    }

    if timed_words.is_empty() {
        bail!("Qwen alignment output contained only empty word timestamps");
    }

    timed_words.sort_by_key(|word| word.start_ms);
    Ok(timed_words)
}

fn collect_word_values(value: &Value) -> Vec<&Value> {
    if let Some(words) = value.get("words").and_then(Value::as_array) {
        return words.iter().collect();
    }

    if let Some(array) = value.as_array() {
        return array.iter().collect();
    }

    let mut words = Vec::new();
    if let Some(segments) = value.get("segments").and_then(Value::as_array) {
        for segment in segments {
            if let Some(segment_words) = segment.get("words").and_then(Value::as_array) {
                words.extend(segment_words.iter());
            }
        }
    }
    words
}

fn usable_alignment_words(words: &[TimedWord]) -> Result<&[TimedWord]> {
    let Some(first_non_zero_point) = words
        .iter()
        .position(|word| word.start_ms != 0 || word.end_ms != 0)
    else {
        bail!("Qwen alignment output did not contain usable non-zero timestamps");
    };

    Ok(&words[first_non_zero_point..])
}

fn read_word_timestamps(word: &Value) -> Result<(i64, i64)> {
    if let Some(timestamp) = word.get("timestamp").and_then(Value::as_array) {
        if timestamp.len() >= 2 {
            let start = timestamp[0]
                .as_f64()
                .ok_or_else(|| anyhow!("timestamp start is not numeric"))?;
            let end = timestamp[1]
                .as_f64()
                .ok_or_else(|| anyhow!("timestamp end is not numeric"))?;
            return Ok((timestamp_value_to_ms(start), timestamp_value_to_ms(end)));
        }
    }

    let start = read_numeric_timestamp(word, &["start_ms", "start", "begin", "start_time"])
        .ok_or_else(|| anyhow!("word timestamp is missing start"))?;
    let end = read_numeric_timestamp(word, &["end_ms", "end", "end_time"])
        .ok_or_else(|| anyhow!("word timestamp is missing end"))?;

    Ok((start, end))
}

fn read_numeric_timestamp(word: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        let Some(value) = word.get(*key).and_then(Value::as_f64) else {
            continue;
        };
        return Some(if key.ends_with("_ms") {
            value.round() as i64
        } else {
            timestamp_value_to_ms(value)
        });
    }
    None
}

fn timestamp_value_to_ms(value: f64) -> i64 {
    if value > 10_000.0 {
        value.round() as i64
    } else {
        (value * 1000.0).round() as i64
    }
}

pub fn generate_synced_lrc_from_words(
    plain_lyrics: &str,
    words: &[TimedWord],
) -> Result<AutoSyncGenerated> {
    let words = usable_alignment_words(words)?;
    let source_lines = plain_lyrics
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if source_lines.is_empty() {
        bail!("Plain lyrics are empty");
    }
    if words.is_empty() {
        bail!("No word timestamps were provided");
    }

    let mut script_tokens = Vec::new();
    for (line_index, line) in source_lines.iter().enumerate() {
        for token in tokenize(line) {
            script_tokens.push(ScriptToken { line_index, token });
        }
    }

    if script_tokens.is_empty() {
        bail!("Plain lyrics did not contain alignable words");
    }

    let aligned = align_script_tokens(&script_tokens, words);
    let mut line_matches: HashMap<usize, Vec<TokenMatch>> = HashMap::new();
    for token_match in aligned {
        line_matches
            .entry(token_match.line_index)
            .or_default()
            .push(token_match);
    }

    let mut lines = Vec::with_capacity(source_lines.len());
    let mut matched_lines = 0usize;
    let mut total_similarity = 0.0f64;
    let mut similarity_count = 0usize;

    for (index, text) in source_lines.iter().enumerate() {
        if let Some(matches) = line_matches
            .get(&index)
            .filter(|matches| !matches.is_empty())
        {
            matched_lines += 1;
            let start_ms = matches
                .iter()
                .map(|word_match| word_match.start_ms)
                .min()
                .unwrap_or(0);
            let end_ms = matches
                .iter()
                .map(|word_match| word_match.end_ms)
                .max()
                .unwrap_or(start_ms + 1_500);
            for word_match in matches {
                total_similarity += word_match.similarity;
                similarity_count += 1;
            }
            lines.push(AutoSyncLine {
                index,
                text: text.clone(),
                start_ms,
                end_ms: Some(end_ms),
                matched_words: matches.len(),
                confidence: matches.iter().map(|m| m.similarity).sum::<f64>()
                    / matches.len() as f64,
                interpolated: false,
            });
        } else {
            lines.push(AutoSyncLine {
                index,
                text: text.clone(),
                start_ms: -1,
                end_ms: None,
                matched_words: 0,
                confidence: 0.0,
                interpolated: true,
            });
        }
    }

    interpolate_missing_lines(&mut lines, words);
    clamp_line_timings(&mut lines);

    let average_word_similarity = if similarity_count == 0 {
        0.0
    } else {
        total_similarity / similarity_count as f64
    };
    let matched_line_ratio = matched_lines as f64 / lines.len() as f64;
    let interpolated_line_ratio =
        lines.iter().filter(|line| line.interpolated).count() as f64 / lines.len() as f64;
    let confidence = (0.55 * matched_line_ratio
        + 0.35 * average_word_similarity
        + 0.10 * (1.0 - interpolated_line_ratio))
        .clamp(0.0, 1.0);

    let synced_lrc = lines
        .iter()
        .map(|line| format!("{}{}", format_timestamp(line.start_ms), line.text))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(AutoSyncGenerated {
        synced_lrc,
        lines,
        metrics: AutoSyncMetrics {
            confidence,
            matched_line_ratio,
            average_word_similarity,
            interpolated_line_ratio,
        },
    })
}

#[derive(Debug, Clone)]
struct ScriptToken {
    line_index: usize,
    token: String,
}

#[derive(Debug, Clone)]
struct TokenMatch {
    line_index: usize,
    start_ms: i64,
    end_ms: i64,
    similarity: f64,
}

fn align_script_tokens(script_tokens: &[ScriptToken], words: &[TimedWord]) -> Vec<TokenMatch> {
    let mut matches = Vec::new();
    let mut word_cursor = 0usize;
    let lookahead = 80usize;

    for token in script_tokens {
        let mut best: Option<(usize, f64)> = None;
        let search_end = (word_cursor + lookahead).min(words.len());
        for (word_index, word) in words.iter().enumerate().take(search_end).skip(word_cursor) {
            let similarity = word_similarity(&token.token, &word.text);
            if similarity < 0.45 {
                continue;
            }
            match best {
                Some((_, best_score)) if best_score >= similarity => {}
                _ => best = Some((word_index, similarity)),
            }
        }

        if let Some((word_index, similarity)) = best {
            let word = &words[word_index];
            matches.push(TokenMatch {
                line_index: token.line_index,
                start_ms: word.start_ms,
                end_ms: word.end_ms,
                similarity,
            });
            word_cursor = word_index + 1;
        }
    }

    matches
}

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for c in input.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(current.clone());
            current.clear();
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn word_similarity(left: &str, right: &str) -> f64 {
    let left = normalize_token(left);
    let right = normalize_token(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    if left.contains(&right) || right.contains(&left) {
        let min = left.chars().count().min(right.chars().count()) as f64;
        let max = left.chars().count().max(right.chars().count()) as f64;
        return (min / max).max(0.72);
    }

    let max_len = left.chars().count().max(right.chars().count()) as f64;
    let distance = levenshtein(&left, &right) as f64;
    (1.0 - distance / max_len).clamp(0.0, 1.0)
}

fn normalize_token(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];

    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != right_char);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right.len()]
}

fn interpolate_missing_lines(lines: &mut [AutoSyncLine], words: &[TimedWord]) {
    let first_start = words.first().map(|word| word.start_ms).unwrap_or(0);
    let last_end = words
        .last()
        .map(|word| word.end_ms)
        .unwrap_or(first_start + 1_500);

    for index in 0..lines.len() {
        if lines[index].start_ms >= 0 {
            continue;
        }

        let previous = (0..index).rev().find(|&i| lines[i].start_ms >= 0);
        let next = ((index + 1)..lines.len()).find(|&i| lines[i].start_ms >= 0);

        let interpolated_start = match (previous, next) {
            (Some(prev), Some(next)) => {
                let prev_start = lines[prev].start_ms;
                let next_start = lines[next].start_ms;
                let slots = (next - prev) as i64;
                prev_start + ((next_start - prev_start) * (index - prev) as i64 / slots.max(1))
            }
            (Some(prev), None) => lines[prev].end_ms.unwrap_or(lines[prev].start_ms + 1_500),
            (None, Some(next)) => {
                let next_start = lines[next].start_ms;
                let gap = ((next_start - first_start).max(1_000)) / (next as i64 + 1);
                first_start + gap * index as i64
            }
            (None, None) => first_start + index as i64 * 1_500,
        };

        lines[index].start_ms = interpolated_start.clamp(0, last_end.max(interpolated_start));
        lines[index].end_ms =
            Some((lines[index].start_ms + 1_500).min(last_end.max(lines[index].start_ms + 1)));
    }
}

fn clamp_line_timings(lines: &mut [AutoSyncLine]) {
    for index in 0..lines.len() {
        let min_start = if index == 0 {
            0
        } else {
            lines[index - 1].start_ms + 20
        };
        if lines[index].start_ms < min_start {
            lines[index].start_ms = min_start;
        }

        let next_start = lines.get(index + 1).map(|line| line.start_ms);
        let mut end_ms = lines[index].end_ms.unwrap_or(lines[index].start_ms + 1_500);
        if let Some(next_start) = next_start {
            end_ms = end_ms.min(next_start.saturating_sub(20));
        }
        if end_ms <= lines[index].start_ms {
            end_ms = lines[index].start_ms + 300;
        }
        lines[index].end_ms = Some(end_ms);
    }
}

pub fn decode_audio_to_16khz_mono_wav(input_path: &Path, output_path: &Path) -> Result<()> {
    let file = File::open(input_path)
        .with_context(|| format!("Failed to open audio file {}", input_path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = input_path
        .extension()
        .and_then(|extension| extension.to_str())
    {
        hint.with_extension(extension);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Failed to probe audio format")?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("Audio file has no default track"))?;
    if track.codec_params.codec == CODEC_TYPE_NULL {
        bail!("Audio file codec is not supported");
    }
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("Failed to create audio decoder")?;

    let mut source_sample_rate = track.codec_params.sample_rate.unwrap_or(TARGET_SAMPLE_RATE);
    let mut mono_samples = Vec::<f32>::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => continue,
            Err(error) => return Err(anyhow!(error)).context("Failed to read audio packet"),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(anyhow!(error)).context("Failed to decode audio packet"),
        };

        source_sample_rate = decoded.spec().rate;
        let channels = decoded.spec().channels.count().max(1);
        let mut sample_buffer =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buffer.copy_interleaved_ref(decoded);
        for frame in sample_buffer.samples().chunks(channels) {
            let sum = frame.iter().copied().sum::<f32>();
            mono_samples.push(sum / frame.len().max(1) as f32);
        }
    }

    if mono_samples.is_empty() {
        bail!("Audio decoder did not produce any samples");
    }

    let resampled = resample_linear(&mono_samples, source_sample_rate, TARGET_SAMPLE_RATE);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(output_path, spec)
        .with_context(|| format!("Failed to create temporary WAV {}", output_path.display()))?;
    for sample in resampled {
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == 0 || source_rate == target_rate {
        return samples.to_vec();
    }

    let target_len = ((samples.len() as f64) * target_rate as f64 / source_rate as f64)
        .round()
        .max(1.0) as usize;
    let ratio = source_rate as f64 / target_rate as f64;
    let mut output = Vec::with_capacity(target_len);
    for index in 0..target_len {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        let right = (left + 1).min(samples.len().saturating_sub(1));
        let frac = (position - left as f64) as f32;
        let sample = samples[left] * (1.0 - frac) + samples[right] * frac;
        output.push(sample);
    }
    output
}

pub fn audio_hash(path: &Path) -> Result<String> {
    sha256_file(path)
}

pub fn build_qwen_direct_align_args(
    paths: &QwenEnginePaths,
    wav_path: &Path,
    plain_lyrics: &str,
    language: &str,
    output_path: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-m".to_string(),
        paths.aligner_model_path.to_string_lossy().to_string(),
        "-f".to_string(),
        wav_path.to_string_lossy().to_string(),
        "--align".to_string(),
        "--text".to_string(),
        normalize_plain_lyrics_for_alignment(plain_lyrics),
        "-o".to_string(),
        output_path.to_string_lossy().to_string(),
    ];

    if !language.trim().is_empty() && language != "auto" {
        args.push("--lang".to_string());
        args.push(language.to_string());
    }

    args
}

pub fn normalize_plain_lyrics_for_alignment(plain_lyrics: &str) -> String {
    plain_lyrics
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_qwen_transcribe_align_args(
    paths: &QwenEnginePaths,
    wav_path: &Path,
    output_path: &Path,
) -> Vec<String> {
    vec![
        "-m".to_string(),
        paths.model_path.to_string_lossy().to_string(),
        "--aligner-model".to_string(),
        paths.aligner_model_path.to_string_lossy().to_string(),
        "-f".to_string(),
        wav_path.to_string_lossy().to_string(),
        "--transcribe-align".to_string(),
        "-o".to_string(),
        output_path.to_string_lossy().to_string(),
    ]
}

pub async fn run_qwen_alignment_command(
    app_handle: &AppHandle,
    context: AutoSyncCommandContext,
    executable_path: &Path,
    args: &[String],
    output_path: &Path,
) -> Result<QwenCommandOutput> {
    emit_auto_sync_engine_started(app_handle, &context, "Starting Qwen engine");
    let started_at = Instant::now();
    let mut command = Command::new(executable_path);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    hide_child_process_window(&mut command);

    let mut child = command
        .spawn()
        .context("Failed to start Qwen alignment engine")?;
    emit_auto_sync_engine_log(
        app_handle,
        &context,
        "status",
        "Qwen process started; waiting for alignment output",
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Qwen stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Qwen stderr"))?;

    let stdout_reader = tokio::spawn(read_process_stream(
        stdout,
        app_handle.clone(),
        context.clone(),
        "stdout",
    ));
    let stderr_reader = tokio::spawn(read_process_stream(
        stderr,
        app_handle.clone(),
        context.clone(),
        "stderr",
    ));

    let progress_app_handle = app_handle.clone();
    let progress_context = context.clone();
    let progress_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            emit_auto_sync_engine_log(
                &progress_app_handle,
                &progress_context,
                "status",
                &qwen_progress_message(started_at.elapsed()),
            );
        }
    });

    let status = match tokio::time::timeout(
        Duration::from_secs(DIRECT_ALIGN_TIMEOUT_SECONDS),
        child.wait(),
    )
    .await
    {
        Ok(result) => {
            progress_task.abort();
            result.context("Failed to wait for Qwen alignment engine")?
        }
        Err(_) => {
            progress_task.abort();
            let _ = child.kill().await;
            let message = format!("Qwen alignment timed out after {DIRECT_ALIGN_TIMEOUT_SECONDS}s");
            emit_auto_sync_engine_failed(app_handle, &context, &message, started_at.elapsed());
            bail!(message);
        }
    };

    let stdout = stdout_reader
        .await
        .context("Failed to join Qwen stdout reader")??;
    let stderr = stderr_reader
        .await
        .context("Failed to join Qwen stderr reader")??;
    let captured_output = combine_process_output(&stdout, &stderr);

    if !status.success() {
        let message = format!(
            "Qwen alignment failed with status {}: {}",
            status,
            captured_output.trim()
        );
        emit_auto_sync_engine_failed(app_handle, &context, &message, started_at.elapsed());
        bail!(
            "Qwen alignment failed with status {}: {}",
            status,
            captured_output.trim()
        );
    }

    emit_auto_sync_engine_finished(
        app_handle,
        &context,
        "Qwen engine finished",
        started_at.elapsed(),
        status.code(),
    );

    let output_json = if output_path.exists() {
        fs::read_to_string(output_path)
            .with_context(|| format!("Failed to read Qwen output {}", output_path.display()))
    } else {
        Ok(stdout)
    }?;

    Ok(QwenCommandOutput {
        output_json,
        captured_output,
    })
}

fn qwen_progress_message(elapsed: Duration) -> String {
    format!("Qwen engine still running ({}s elapsed)", elapsed.as_secs())
}

pub fn qwen_engine_paths(
    app_handle: &AppHandle,
    config: &PersistentConfig,
) -> Result<QwenEnginePaths> {
    let base = qwen_base_dir(app_handle)?;
    Ok(QwenEnginePaths {
        executable_path: engine_executable_path(&base),
        model_path: base.join("models").join(model_from_config(config)),
        aligner_model_path: base.join("models").join(aligner_model_from_config(config)),
    })
}

pub fn list_auto_sync_assets(app_handle: &AppHandle) -> Result<Vec<AutoSyncAssetStatus>> {
    let base = qwen_base_dir(app_handle)?;
    asset_specs()
        .into_iter()
        .map(|spec| asset_status(&base, spec))
        .collect()
}

pub async fn download_auto_sync_asset(
    app_handle: AppHandle,
    asset_id: String,
) -> Result<AutoSyncAssetStatus> {
    let base = qwen_base_dir(&app_handle)?;
    let spec = asset_specs()
        .into_iter()
        .find(|spec| spec.id == asset_id)
        .ok_or_else(|| anyhow!("Unknown auto-sync asset: {asset_id}"))?;

    fs::create_dir_all(&base)?;
    fs::create_dir_all(base.join("downloads"))?;
    let download_path = base
        .join("downloads")
        .join(format!("{}.download", spec.file_name));

    let client = reqwest::Client::new();
    let response = client
        .get(spec.url)
        .send()
        .await
        .with_context(|| format!("Failed to download {}", spec.name))?
        .error_for_status()
        .with_context(|| format!("Failed to download {}", spec.name))?;
    let total_bytes = response.content_length().or(Some(spec.expected_size));
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(&download_path).await?;
    let mut downloaded_bytes = 0u64;

    let _ = app_handle.emit(
        "auto-sync-asset-progress",
        AutoSyncAssetProgress {
            asset_id: spec.id.to_string(),
            downloaded_bytes,
            total_bytes,
            done: false,
        },
    );

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        downloaded_bytes += chunk.len() as u64;
        let _ = app_handle.emit(
            "auto-sync-asset-progress",
            AutoSyncAssetProgress {
                asset_id: spec.id.to_string(),
                downloaded_bytes,
                total_bytes,
                done: false,
            },
        );
    }
    file.flush().await?;
    drop(file);

    verify_downloaded_file(&download_path, spec.expected_size, spec.expected_sha256)?;

    if spec.archive {
        extract_zip_asset(&download_path, &base)?;
    } else {
        fs::create_dir_all(base.join("models"))?;
        fs::rename(&download_path, base.join("models").join(spec.file_name))?;
    }

    let _ = app_handle.emit(
        "auto-sync-asset-progress",
        AutoSyncAssetProgress {
            asset_id: spec.id.to_string(),
            downloaded_bytes,
            total_bytes,
            done: true,
        },
    );

    asset_status(&base, spec)
}

pub async fn test_qwen_engine(app_handle: AppHandle, config: PersistentConfig) -> Result<String> {
    let paths = qwen_engine_paths(&app_handle, &config)?;
    for path in [
        &paths.executable_path,
        &paths.model_path,
        &paths.aligner_model_path,
    ] {
        if !path.exists() {
            bail!("Missing auto-sync asset: {}", path.display());
        }
    }

    let mut command = Command::new(&paths.executable_path);
    command.arg("--help");
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    hide_child_process_window(&mut command);

    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| anyhow!("Qwen engine help check timed out"))?
        .context("Failed to run Qwen engine")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = if !stdout.trim().is_empty() {
        stdout
            .trim()
            .lines()
            .next()
            .unwrap_or("Qwen engine responded")
    } else if !stderr.trim().is_empty() {
        stderr
            .trim()
            .lines()
            .next()
            .unwrap_or("Qwen engine responded")
    } else {
        "Qwen engine responded"
    };

    Ok(summary.to_string())
}

fn hide_child_process_window(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

pub fn emit_auto_sync_engine_started(
    app_handle: &AppHandle,
    context: &AutoSyncCommandContext,
    message: &str,
) {
    let _ = app_handle.emit(
        "auto-sync-engine-started",
        engine_event(context, message, None, None, None),
    );
}

pub fn emit_auto_sync_engine_log(
    app_handle: &AppHandle,
    context: &AutoSyncCommandContext,
    stream: &str,
    message: &str,
) {
    let _ = app_handle.emit(
        "auto-sync-engine-log",
        engine_event(context, message, Some(stream), None, None),
    );
}

pub fn emit_auto_sync_engine_finished(
    app_handle: &AppHandle,
    context: &AutoSyncCommandContext,
    message: &str,
    elapsed: Duration,
    exit_code: Option<i32>,
) {
    let _ = app_handle.emit(
        "auto-sync-engine-finished",
        engine_event(context, message, None, Some(elapsed.as_millis()), exit_code),
    );
}

pub fn emit_auto_sync_engine_failed(
    app_handle: &AppHandle,
    context: &AutoSyncCommandContext,
    message: &str,
    elapsed: Duration,
) {
    let _ = app_handle.emit(
        "auto-sync-engine-failed",
        engine_event(context, message, None, Some(elapsed.as_millis()), None),
    );
}

fn engine_event(
    context: &AutoSyncCommandContext,
    message: &str,
    stream: Option<&str>,
    elapsed_ms: Option<u128>,
    exit_code: Option<i32>,
) -> AutoSyncEngineEvent {
    AutoSyncEngineEvent {
        track_id: context.track_id,
        track_title: context.track_title.clone(),
        artist_name: context.artist_name.clone(),
        phase: context.phase.clone(),
        message: message.to_string(),
        stream: stream.map(ToOwned::to_owned),
        elapsed_ms,
        exit_code,
    }
}

async fn read_process_stream<R>(
    reader: R,
    app_handle: AppHandle,
    context: AutoSyncCommandContext,
    stream: &'static str,
) -> Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut output = String::new();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        output.push_str(&line);
        output.push('\n');
        if let Some(line) = visible_process_log_line(&line) {
            emit_auto_sync_engine_log(&app_handle, &context, stream, line);
        }
    }
    Ok(output)
}

fn visible_process_log_line(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn combine_process_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("stdout:\n{}\nstderr:\n{}", stdout.trim(), stderr.trim()),
    }
}

pub fn temp_sync_paths(app_handle: &AppHandle, track_id: i64) -> Result<(PathBuf, PathBuf)> {
    let temp_dir = app_handle
        .path()
        .app_cache_dir()
        .context("Failed to resolve app cache directory")?
        .join("autosync");
    fs::create_dir_all(&temp_dir)?;
    Ok((
        temp_dir.join(format!("track-{track_id}.16khz.wav")),
        temp_dir.join(format!("track-{track_id}.alignment.json")),
    ))
}

fn asset_specs() -> Vec<AutoSyncAssetSpec> {
    let engine = AutoSyncAssetSpec {
        id: "qwen3-asr-cpp-engine",
        kind: "engine",
        name: "Qwen3 ASR CPP engine",
        file_name: platform_engine_archive_name(),
        url: platform_engine_archive_url(),
        expected_size: platform_engine_archive_size(),
        expected_sha256: platform_engine_archive_sha256(),
        archive: true,
    };

    vec![
        engine,
        AutoSyncAssetSpec {
            id: "qwen3-asr-0.6b-q8_0",
            kind: "model",
            name: "Qwen3 ASR 0.6B Q8_0",
            file_name: DEFAULT_AUTO_SYNC_MODEL,
            url: "https://huggingface.co/OpenVoiceOS/qwen3-asr-0.6b-q8-0/resolve/main/qwen3-asr-0.6b-q8_0.gguf",
            expected_size: 1_354_082_624,
            expected_sha256: "e777dacf2c23e4a3eafbec64e4e9d522b662c693c5189d4fd38ff39b92c9a334",
            archive: false,
        },
        AutoSyncAssetSpec {
            id: "qwen3-forced-aligner-0.6b-q4_k_m",
            kind: "model",
            name: "Qwen3 ForcedAligner 0.6B Q4_K_M",
            file_name: DEFAULT_AUTO_SYNC_ALIGNER_MODEL,
            url: "https://huggingface.co/OpenVoiceOS/qwen3-forced-aligner-0.6b-q4-k-m/resolve/main/qwen3-forced-aligner-0.6b-q4_k_m.gguf",
            expected_size: 615_667_968,
            expected_sha256: "542687c8ddb39f9f6510dd7db99697f70ed1148cd3cd7ddbae22097cce73dd6e",
            archive: false,
        },
    ]
}

fn platform_engine_archive_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "qwen3-asr-cpp-win64.zip"
    } else if cfg!(target_os = "macos") {
        "qwen3-asr-cpp-mac.zip"
    } else {
        "qwen3-asr-cpp-linux.zip"
    }
}

fn platform_engine_archive_url() -> &'static str {
    if cfg!(target_os = "windows") {
        "https://github.com/SubtitleEdit/support-files/releases/download/qwen3-asr-cpp-2026-3/qwen3-asr-cpp-win64.zip"
    } else if cfg!(target_os = "macos") {
        "https://github.com/SubtitleEdit/support-files/releases/download/qwen3-asr-cpp-2026-3/qwen3-asr-cpp-mac.zip"
    } else {
        "https://github.com/SubtitleEdit/support-files/releases/download/qwen3-asr-cpp-2026-3/qwen3-asr-cpp-linux.zip"
    }
}

fn platform_engine_archive_size() -> u64 {
    if cfg!(target_os = "windows") {
        909_748
    } else if cfg!(target_os = "macos") {
        179_292
    } else {
        1_777_008
    }
}

fn platform_engine_archive_sha256() -> &'static str {
    if cfg!(target_os = "windows") {
        "d1d69ac85529912ebd1fc041ae4a7ca50d406ffc31c4610b02138e208fe10cf4"
    } else if cfg!(target_os = "macos") {
        "366c0b3d1134690b58dfd74f5ccacb6f29454c9ce7b68dc034bc732ac1d334ac"
    } else {
        "ce5bae4c7c2b41f90edcec1777d2b3911668b2d7c9a5ee3a6ea4960767f35723"
    }
}

fn qwen_base_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    Ok(app_handle
        .path()
        .app_data_dir()
        .context("Failed to resolve app data directory")?
        .join("autosync")
        .join("qwen3_asr_cpp"))
}

fn engine_executable_path(base: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        base.join("qwen3-asr-cli.exe")
    } else {
        base.join("qwen3-asr-cli")
    }
}

fn asset_path(base: &Path, spec: &AutoSyncAssetSpec) -> PathBuf {
    if spec.archive {
        engine_executable_path(base)
    } else {
        base.join("models").join(spec.file_name)
    }
}

fn asset_status(base: &Path, spec: AutoSyncAssetSpec) -> Result<AutoSyncAssetStatus> {
    let path = asset_path(base, &spec);
    let metadata = fs::metadata(&path).ok();
    let installed = if spec.archive {
        metadata.as_ref().map(|m| m.is_file()).unwrap_or(false)
    } else {
        metadata
            .as_ref()
            .map(|m| m.is_file() && m.len() == spec.expected_size)
            .unwrap_or(false)
            && sha256_file(&path)
                .map(|hash| hash.eq_ignore_ascii_case(spec.expected_sha256))
                .unwrap_or(false)
    };

    Ok(AutoSyncAssetStatus {
        id: spec.id.to_string(),
        kind: spec.kind.to_string(),
        name: spec.name.to_string(),
        file_name: spec.file_name.to_string(),
        url: spec.url.to_string(),
        expected_size: spec.expected_size,
        expected_sha256: spec.expected_sha256.to_string(),
        installed,
        path: path.to_string_lossy().to_string(),
        bytes_on_disk: metadata.map(|m| m.len()),
    })
}

fn verify_downloaded_file(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Downloaded file is missing: {}", path.display()))?;
    if metadata.len() != expected_size {
        bail!(
            "Downloaded file size mismatch for {}: expected {}, got {}",
            path.display(),
            expected_size,
            metadata.len()
        );
    }
    let hash = sha256_file(path)?;
    if !hash.eq_ignore_ascii_case(expected_sha256) {
        bail!(
            "Downloaded file hash mismatch for {}: expected {}, got {}",
            path.display(),
            expected_sha256,
            hash
        );
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_zip_asset(zip_path: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let out_path = destination.join(enclosed_name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&out_path)?;
        std::io::copy(&mut entry, &mut output)?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qwen_word_timestamp_json() {
        let output = r#"{
          "words": [
            {"word": "Hello", "start": 0.25, "end": 0.72},
            {"word": "world", "start_ms": 720, "end_ms": 1010}
          ]
        }"#;

        let words = parse_qwen_alignment_json(output).unwrap();

        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].start_ms, 250);
        assert_eq!(words[1].text, "world");
        assert_eq!(words[1].end_ms, 1010);
    }

    #[test]
    fn aligns_repeated_lines_to_distinct_word_timestamps() {
        let lyrics = "Hello world\nHello again";
        let words = vec![
            TimedWord {
                text: "Hello".to_string(),
                start_ms: 1_000,
                end_ms: 1_200,
                probability: None,
            },
            TimedWord {
                text: "world".to_string(),
                start_ms: 1_250,
                end_ms: 1_600,
                probability: None,
            },
            TimedWord {
                text: "Hello".to_string(),
                start_ms: 3_000,
                end_ms: 3_200,
                probability: None,
            },
            TimedWord {
                text: "again".to_string(),
                start_ms: 3_250,
                end_ms: 3_650,
                probability: None,
            },
        ];

        let generated = generate_synced_lrc_from_words(lyrics, &words).unwrap();

        assert_eq!(generated.lines.len(), 2);
        assert_eq!(generated.lines[0].start_ms, 1_000);
        assert_eq!(generated.lines[1].start_ms, 3_000);
        assert!(generated.metrics.confidence > 0.95);
        assert!(generated.synced_lrc.contains("[00:01.00]Hello world"));
        assert!(generated.synced_lrc.contains("[00:03.00]Hello again"));
    }

    #[test]
    fn ignores_leading_zero_point_alignment_placeholders() {
        let lyrics = "Hello world\nHello again";
        let words = vec![
            TimedWord {
                text: "Hello".to_string(),
                start_ms: 0,
                end_ms: 0,
                probability: None,
            },
            TimedWord {
                text: "world".to_string(),
                start_ms: 0,
                end_ms: 0,
                probability: None,
            },
            TimedWord {
                text: "Hello".to_string(),
                start_ms: 33_760,
                end_ms: 33_840,
                probability: None,
            },
            TimedWord {
                text: "world".to_string(),
                start_ms: 34_080,
                end_ms: 34_160,
                probability: None,
            },
            TimedWord {
                text: "Hello".to_string(),
                start_ms: 35_000,
                end_ms: 35_080,
                probability: None,
            },
            TimedWord {
                text: "again".to_string(),
                start_ms: 35_200,
                end_ms: 35_360,
                probability: None,
            },
        ];

        let generated = generate_synced_lrc_from_words(lyrics, &words).unwrap();

        assert_eq!(generated.lines[0].start_ms, 33_760);
        assert!(generated.synced_lrc.starts_with("[00:33.76]Hello world"));
    }

    #[test]
    fn low_confidence_result_does_not_auto_apply_with_default_policy() {
        let metrics = AutoSyncMetrics {
            confidence: 0.70,
            matched_line_ratio: 0.80,
            average_word_similarity: 0.88,
            interpolated_line_ratio: 0.25,
        };

        assert!(!should_auto_apply_sync_result(
            DEFAULT_AUTO_SYNC_SAVE_POLICY,
            DEFAULT_AUTO_SYNC_CONFIDENCE_THRESHOLD,
            &metrics
        ));
    }

    #[test]
    fn command_builder_keeps_windows_paths_and_long_text_as_arguments() {
        let paths = QwenEnginePaths {
            executable_path: PathBuf::from(r"C:\Users\Test\AppData\Local\LRCGET\qwen3-asr-cli.exe"),
            model_path: PathBuf::from(r"C:\models\qwen3-asr-0.6b-q8_0.gguf"),
            aligner_model_path: PathBuf::from(r"C:\models\qwen3-forced-aligner-0.6b-q4_k_m.gguf"),
        };
        let lyrics = "line one\nline two with spaces";
        let args = build_qwen_direct_align_args(
            &paths,
            Path::new(r"C:\Temp Files\track.wav"),
            lyrics,
            "en",
            Path::new(r"C:\Temp Files\alignment.json"),
        );

        assert!(args.iter().any(|arg| arg == lyrics));
        assert!(args.iter().any(|arg| arg == r"C:\Temp Files\track.wav"));
        assert_eq!(args.iter().filter(|arg| *arg == "--text").count(), 1);
    }

    #[test]
    fn direct_align_text_argument_skips_blank_lyric_lines() {
        let paths = QwenEnginePaths {
            executable_path: PathBuf::from(r"C:\Users\Test\AppData\Local\LRCGET\qwen3-asr-cli.exe"),
            model_path: PathBuf::from(r"C:\models\qwen3-asr-0.6b-q8_0.gguf"),
            aligner_model_path: PathBuf::from(r"C:\models\qwen3-forced-aligner-0.6b-q4_k_m.gguf"),
        };
        let lyrics = "line one\r\n\r\n  \nline two with spaces\n\nline three";
        let args = build_qwen_direct_align_args(
            &paths,
            Path::new(r"C:\Temp Files\track.wav"),
            lyrics,
            "en",
            Path::new(r"C:\Temp Files\alignment.json"),
        );

        let text_arg_index = args.iter().position(|arg| arg == "--text").unwrap() + 1;
        assert_eq!(
            args[text_arg_index],
            "line one\nline two with spaces\nline three"
        );
    }

    #[test]
    fn process_log_lines_hide_blank_output() {
        assert_eq!(visible_process_log_line(""), None);
        assert_eq!(visible_process_log_line("   "), None);
        assert_eq!(
            visible_process_log_line("  Model loaded. Running alignment...  "),
            Some("Model loaded. Running alignment...")
        );
    }

    #[test]
    fn qwen_progress_message_reports_elapsed_seconds() {
        assert_eq!(
            qwen_progress_message(Duration::from_millis(12_345)),
            "Qwen engine still running (12s elapsed)"
        );
    }
}
