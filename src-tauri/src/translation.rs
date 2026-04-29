use crate::parser::lrc::{format_timestamp, parse_lrc};
use crate::persistent_entities::PersistentConfig;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use xxhash_rust::xxh3::xxh3_64;

pub const DEFAULT_TARGET_LANGUAGE: &str = "English";
pub const DEFAULT_TRANSLATION_PROVIDER: &str = "gemini";
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-flash-latest";
pub const DEFAULT_TRANSLATION_EXPORT_MODE: &str = "original";
pub const TRANSLATION_STATUS_SUCCEEDED: &str = "succeeded";
pub const TRANSLATION_STATUS_PENDING: &str = "pending";
pub const TRANSLATION_STATUS_FAILED: &str = "failed";
pub const TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE: &str = "skipped_same_language";
pub const SAME_LANGUAGE_SKIP_CONFIDENCE: f64 = 0.85;
const MIN_LANGUAGE_DETECTION_CHARS: usize = 40;
const CHUNK_LANGUAGE_DETECTION_CHARS: usize = 80;
const CHUNK_LANGUAGE_DETECTION_CONFIDENCE: f64 = 0.70;
const LATIN_TARGET_NON_LATIN_CONFLICT_RATIO: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationExportMode {
    Original,
    Translation,
    Dual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationLine {
    pub source_index: usize,
    pub translated_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricTranslationUpsert {
    pub lyricsfile_id: i64,
    pub track_id: Option<i64>,
    pub source_hash: String,
    pub source_lyricsfile: String,
    pub provider: String,
    pub provider_model: String,
    pub target_language: String,
    pub settings_hash: String,
    pub status: String,
    pub translated_lines_json: Option<String>,
    pub translated_lrc: Option<String>,
    pub error_message: Option<String>,
    pub provider_metadata_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricTranslation {
    pub id: i64,
    pub lyricsfile_id: i64,
    pub track_id: Option<i64>,
    pub source_hash: String,
    pub source_lyricsfile: String,
    pub provider: String,
    pub provider_model: String,
    pub target_language: String,
    pub settings_hash: String,
    pub status: String,
    pub translated_lines_json: Option<String>,
    pub translated_lrc: Option<String>,
    pub error_message: Option<String>,
    pub provider_metadata_json: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TranslationLinesResponse {
    lines: Vec<TranslationLine>,
}

#[derive(Debug, Clone)]
struct SourceLine {
    timestamp_ms: i64,
    text: String,
}

#[derive(Debug, Clone, Default)]
struct ScriptCounts {
    latin: usize,
    hangul: usize,
    kana: usize,
    cjk: usize,
    other_alpha: usize,
}

impl ScriptCounts {
    fn total_alpha(&self) -> usize {
        self.latin + self.hangul + self.kana + self.cjk + self.other_alpha
    }

    fn non_latin(&self) -> usize {
        self.hangul + self.kana + self.cjk + self.other_alpha
    }
}

#[derive(Debug, Clone)]
pub struct TranslationRequest {
    pub title: String,
    pub album_name: String,
    pub artist_name: String,
    pub source_language: Option<String>,
    pub target_language: String,
    pub source_lrc: String,
}

#[derive(Debug, Clone)]
pub struct SameLanguageSkipDecision {
    pub detected_language: String,
    pub detected_language_code: String,
    pub target_language: String,
    pub target_language_code: String,
    pub confidence: f64,
    pub reason: String,
}

pub fn lyrics_source_hash(lyricsfile: &str) -> String {
    format!("{:016x}", xxh3_64(lyricsfile.as_bytes()))
}

pub fn translation_settings_hash(
    provider: &str,
    model: &str,
    target_language: &str,
    _export_mode: TranslationExportMode,
) -> String {
    let payload = format!(
        "{}\n{}\n{}",
        provider.trim().to_lowercase(),
        model.trim(),
        target_language.trim().to_lowercase()
    );
    format!("{:016x}", xxh3_64(payload.as_bytes()))
}

pub fn export_mode_from_str(value: &str) -> TranslationExportMode {
    match value {
        "translation" => TranslationExportMode::Translation,
        "dual" => TranslationExportMode::Dual,
        _ => TranslationExportMode::Original,
    }
}

pub fn provider_model_from_config(config: &PersistentConfig) -> String {
    match config.translation_provider.as_str() {
        "gemini" => non_empty_or_default(&config.translation_gemini_model, DEFAULT_GEMINI_MODEL),
        "openai_compatible" => config.translation_openai_model.trim().to_string(),
        "deepl" => "deepl".to_string(),
        "google" => "google-cloud-translate".to_string(),
        "microsoft" => "microsoft-translator".to_string(),
        value => value.to_string(),
    }
}

pub fn target_language_from_config(config: &PersistentConfig) -> String {
    let target_language = config.translation_target_language.trim();
    if target_language.is_empty() {
        DEFAULT_TARGET_LANGUAGE.to_string()
    } else {
        target_language.to_string()
    }
}

pub fn settings_hash_from_config(config: &PersistentConfig) -> String {
    translation_settings_hash(
        &config.translation_provider,
        &provider_model_from_config(config),
        &target_language_from_config(config),
        export_mode_from_str(&config.translation_export_mode),
    )
}

pub async fn request_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    match config.translation_provider.as_str() {
        "gemini" => request_gemini_translation(config, request).await,
        "deepl" => request_deepl_translation(config, request).await,
        "google" => request_google_translation(config, request).await,
        "microsoft" => request_microsoft_translation(config, request).await,
        "openai_compatible" => request_openai_compatible_translation(config, request).await,
        provider => Err(anyhow!("unsupported translation provider '{}'", provider)),
    }
}

pub fn validate_translation_lines(
    source_lrc: &str,
    translations_json: &str,
) -> Result<Vec<TranslationLine>> {
    let source_lines = parse_source_lines(source_lrc);
    let response: TranslationLinesResponse = serde_json::from_str(translations_json)
        .map_err(|err| anyhow!("invalid translation response JSON: {}", err))?;

    if response.lines.len() != source_lines.len() {
        return Err(anyhow!(
            "translation line count mismatch: expected {}, got {}",
            source_lines.len(),
            response.lines.len()
        ));
    }

    for (expected_index, translated_line) in response.lines.iter().enumerate() {
        if translated_line.source_index != expected_index {
            return Err(anyhow!(
                "translation index mismatch at position {}: expected {}, got {}",
                expected_index,
                expected_index,
                translated_line.source_index
            ));
        }

        let source_text = source_lines
            .get(expected_index)
            .map(|line| line.text.trim())
            .unwrap_or_default();
        if !source_text.is_empty() && translated_line.translated_text.trim().is_empty() {
            return Err(anyhow!(
                "translation for non-empty source line {} is empty",
                expected_index
            ));
        }
    }

    Ok(response.lines)
}

pub fn build_translated_lrc(
    source_lrc: &str,
    translations_json: &str,
    mode: TranslationExportMode,
) -> Result<String> {
    let source_lines = parse_source_lines(source_lrc);
    let translated_lines = validate_translation_lines(source_lrc, translations_json)?;

    let mut output = Vec::new();

    match mode {
        TranslationExportMode::Original => {
            return Ok(source_lrc.replace("\r\n", "\n").trim_end().to_string());
        }
        TranslationExportMode::Translation => {
            for (source_line, translated_line) in source_lines.iter().zip(translated_lines.iter()) {
                output.push(format!(
                    "{}{}",
                    format_timestamp(source_line.timestamp_ms),
                    translated_line.translated_text.trim()
                ));
            }
        }
        TranslationExportMode::Dual => {
            for (source_line, translated_line) in source_lines.iter().zip(translated_lines.iter()) {
                output.push(format!(
                    "{}{}",
                    format_timestamp(source_line.timestamp_ms),
                    source_line.text.trim()
                ));
                output.push(format!(
                    "{}{}",
                    format_timestamp(source_line.timestamp_ms),
                    translated_line.translated_text.trim()
                ));
            }
        }
    }

    Ok(output.join("\n"))
}

pub fn build_export_lrc_for_translation_status(
    source_lrc: &str,
    translation_status: &str,
    translations_json: Option<&str>,
    mode: TranslationExportMode,
) -> Result<String> {
    if mode == TranslationExportMode::Original
        || translation_status == TRANSLATION_STATUS_SKIPPED_SAME_LANGUAGE
    {
        return Ok(source_lrc.replace("\r\n", "\n").trim_end().to_string());
    }

    let translations_json = translations_json
        .ok_or_else(|| anyhow!("current translation row has no translated line payload"))?;
    build_translated_lrc(source_lrc, translations_json, mode)
}

pub fn same_language_skip_decision(
    source_lrc: &str,
    target_language: &str,
) -> Result<Option<SameLanguageSkipDecision>> {
    let detection_text = source_texts_from_lrc(source_lrc)
        .into_iter()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let signal_chars = alphabetic_char_count(&detection_text);

    if signal_chars < MIN_LANGUAGE_DETECTION_CHARS {
        return Ok(None);
    }

    let Some(info) = whatlang::detect(&detection_text) else {
        return Ok(None);
    };

    let detected_language_code = normalize_detected_language_code(info.lang().code());
    let target_language_code = target_language_code(target_language, false);
    let confidence = info.confidence();
    let script_counts = script_counts(&detection_text);
    let has_language_conflict = has_script_conflict(&script_counts, &target_language_code)
        || has_reliable_non_target_chunk(&detection_text, &target_language_code);

    if detected_language_code == target_language_code
        && confidence >= SAME_LANGUAGE_SKIP_CONFIDENCE
        && info.is_reliable()
        && !has_language_conflict
    {
        return Ok(Some(SameLanguageSkipDecision {
            detected_language: info.lang().eng_name().to_string(),
            detected_language_code,
            target_language: target_language_label(target_language).to_string(),
            target_language_code,
            confidence,
            reason: "source_language_matches_target".to_string(),
        }));
    }

    Ok(None)
}

pub fn structured_json_from_gemini_response(raw_response: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw_response)
        .map_err(|err| anyhow!("invalid Gemini response JSON: {}", err))?;
    value
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Gemini response did not include text content"))
}

pub fn structured_json_from_openai_response(raw_response: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw_response)
        .map_err(|err| anyhow!("invalid OpenAI-compatible response JSON: {}", err))?;
    value
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("OpenAI-compatible response did not include message content"))
}

pub fn structured_json_from_deepl_response(raw_response: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw_response).map_err(|err| anyhow!("invalid DeepL JSON: {}", err))?;
    let translations = value
        .get("translations")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("DeepL response did not include translations"))?;
    let texts = translations
        .iter()
        .map(|item| {
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("DeepL translation entry did not include text"))
        })
        .collect::<Result<Vec<_>>>()?;
    structured_json_from_texts(texts)
}

pub fn structured_json_from_google_response(raw_response: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw_response)
        .map_err(|err| anyhow!("invalid Google Translate JSON: {}", err))?;
    let translations = value
        .pointer("/data/translations")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("Google Translate response did not include translations"))?;
    let texts = translations
        .iter()
        .map(|item| {
            item.get("translatedText")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("Google translation entry did not include translatedText"))
        })
        .collect::<Result<Vec<_>>>()?;
    structured_json_from_texts(texts)
}

pub fn structured_json_from_microsoft_response(raw_response: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw_response)
        .map_err(|err| anyhow!("invalid Microsoft Translator JSON: {}", err))?;
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("Microsoft Translator response was not an array"))?;
    let texts = items
        .iter()
        .map(|item| {
            item.pointer("/translations/0/text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("Microsoft translation entry did not include text"))
        })
        .collect::<Result<Vec<_>>>()?;
    structured_json_from_texts(texts)
}

fn parse_source_lines(source_lrc: &str) -> Vec<SourceLine> {
    parse_lrc(source_lrc)
        .timed_lines
        .into_iter()
        .map(|line| SourceLine {
            timestamp_ms: line.timestamp_ms,
            text: line.text,
        })
        .collect()
}

fn structured_json_from_texts(texts: Vec<String>) -> Result<String> {
    let lines = texts
        .into_iter()
        .enumerate()
        .map(|(source_index, translated_text)| TranslationLine {
            source_index,
            translated_text,
        })
        .collect();

    serde_json::to_string(&TranslationLinesResponse { lines }).map_err(Into::into)
}

async fn request_gemini_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    let api_key = require_config_value(&config.translation_gemini_api_key, "Gemini API key")?;
    let model = non_empty_or_default(&config.translation_gemini_model, DEFAULT_GEMINI_MODEL);
    let line_count = parse_source_lines(&request.source_lrc).len();
    let client = reqwest::Client::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
        model
    );
    let response = client
        .post(url)
        .header("x-goog-api-key", api_key)
        .json(&json!({
            "contents": [{
                "parts": [{ "text": build_context_prompt(request) }]
            }],
            "generationConfig": {
                "temperature": 0.2,
                "responseMimeType": "application/json",
                "responseJsonSchema": translation_response_schema(line_count)
            }
        }))
        .send()
        .await?;
    let raw = response_text_or_status_error(response, "Gemini").await?;
    structured_json_from_gemini_response(&raw)
}

async fn request_openai_compatible_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    let base_url = require_config_value(
        &config.translation_openai_base_url,
        "OpenAI-compatible base URL",
    )?;
    let model = require_config_value(&config.translation_openai_model, "OpenAI-compatible model")?;
    let client = reqwest::Client::new();
    let mut builder = client
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .json(&json!({
            "model": model,
            "temperature": 0.2,
            "response_format": { "type": "json_object" },
            "messages": [
                {
                    "role": "system",
                    "content": "You translate song lyrics. Return only JSON with a lines array. Preserve source indexes exactly."
                },
                {
                    "role": "user",
                    "content": build_context_prompt(request)
                }
            ]
        }));

    if !config.translation_openai_api_key.trim().is_empty() {
        builder = builder.bearer_auth(config.translation_openai_api_key.trim());
    }

    let response = builder.send().await?;
    let raw = response_text_or_status_error(response, "OpenAI-compatible").await?;
    structured_json_from_openai_response(&raw)
}

async fn request_deepl_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    let api_key = require_config_value(&config.translation_deepl_api_key, "DeepL API key")?;
    let target_lang = target_language_code(&request.target_language, true);
    let endpoint = if api_key.ends_with(":fx") {
        "https://api-free.deepl.com/v2/translate"
    } else {
        "https://api.deepl.com/v2/translate"
    };
    let mut form = vec![
        ("auth_key".to_string(), api_key),
        ("target_lang".to_string(), target_lang),
    ];
    for line in source_texts(request) {
        form.push(("text".to_string(), line));
    }
    let response = reqwest::Client::new()
        .post(endpoint)
        .form(&form)
        .send()
        .await?;
    let raw = response_text_or_status_error(response, "DeepL").await?;
    structured_json_from_deepl_response(&raw)
}

async fn request_google_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    let api_key = require_config_value(&config.translation_google_api_key, "Google API key")?;
    let response = reqwest::Client::new()
        .post(format!(
            "https://translation.googleapis.com/language/translate/v2?key={}",
            api_key
        ))
        .json(&json!({
            "q": source_texts(request),
            "target": target_language_code(&request.target_language, false),
            "format": "text"
        }))
        .send()
        .await?;
    let raw = response_text_or_status_error(response, "Google Translate").await?;
    structured_json_from_google_response(&raw)
}

async fn request_microsoft_translation(
    config: &PersistentConfig,
    request: &TranslationRequest,
) -> Result<String> {
    let api_key = require_config_value(
        &config.translation_microsoft_api_key,
        "Microsoft Translator API key",
    )?;
    let client = reqwest::Client::new();
    let mut builder = client
        .post(format!(
            "https://api.cognitive.microsofttranslator.com/translate?api-version=3.0&to={}",
            target_language_code(&request.target_language, false)
        ))
        .header("Ocp-Apim-Subscription-Key", api_key)
        .json(
            &source_texts(request)
                .into_iter()
                .map(|text| json!({ "text": text }))
                .collect::<Vec<_>>(),
        );

    if !config.translation_microsoft_region.trim().is_empty() {
        builder = builder.header(
            "Ocp-Apim-Subscription-Region",
            config.translation_microsoft_region.trim(),
        );
    }

    let response = builder.send().await?;
    let raw = response_text_or_status_error(response, "Microsoft Translator").await?;
    structured_json_from_microsoft_response(&raw)
}

async fn response_text_or_status_error(
    response: reqwest::Response,
    provider: &str,
) -> Result<String> {
    let status = response.status();
    let url = response.url().clone();
    let body = response.text().await?;

    if !status.is_success() {
        return Err(anyhow!(
            "{}",
            provider_status_error_message(provider, status, &url, &body)
        ));
    }

    Ok(body)
}

fn provider_status_error_message(
    provider: &str,
    status: reqwest::StatusCode,
    url: &reqwest::Url,
    body: &str,
) -> String {
    let body = response_body_preview(body);
    format!(
        "{} request failed with HTTP {} for {}: {}",
        provider,
        status.as_u16(),
        redacted_url_string(url),
        body
    )
}

fn response_body_preview(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty response body>".to_string();
    }

    const MAX_CHARS: usize = 1200;
    let mut preview = trimmed.chars().take(MAX_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_CHARS {
        preview.push_str("...");
    }
    preview
}

fn redacted_url_string(url: &reqwest::Url) -> String {
    const SENSITIVE_QUERY_KEYS: &[&str] = &["key", "api_key", "apikey", "access_token", "auth_key"];
    let query_pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let redacted = SENSITIVE_QUERY_KEYS
                .iter()
                .any(|sensitive_key| key.eq_ignore_ascii_case(sensitive_key));
            (
                key.to_string(),
                if redacted {
                    "REDACTED".to_string()
                } else {
                    value.to_string()
                },
            )
        })
        .collect::<Vec<_>>();

    if query_pairs.is_empty() {
        return url.to_string();
    }

    let mut redacted_url = url.clone();
    redacted_url.set_query(None);
    {
        let mut serializer = redacted_url.query_pairs_mut();
        for (key, value) in query_pairs {
            serializer.append_pair(&key, &value);
        }
    }
    redacted_url.to_string()
}

fn build_context_prompt(request: &TranslationRequest) -> String {
    let source_language = request
        .source_language
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("auto-detect");
    let lines = parse_source_lines(&request.source_lrc)
        .into_iter()
        .enumerate()
        .map(|(index, line)| format!("{}: {}", index, line.text))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Translate these song lyrics into {target_language}.\n\
         Preserve meaning, tone, imagery, repeated phrases, and singable lyric feel where possible.\n\
         Do not add explanations. Return exactly one translated_text for each source_index.\n\n\
         Title: {title}\nArtist: {artist}\nAlbum: {album}\nSource language: {source_language}\n\n\
         Lines:\n{lines}",
        target_language = request.target_language,
        title = request.title,
        artist = request.artist_name,
        album = request.album_name,
        source_language = source_language,
        lines = lines
    )
}

fn translation_response_schema(line_count: usize) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "lines": {
                "type": "array",
                "minItems": line_count,
                "maxItems": line_count,
                "items": {
                    "type": "object",
                    "properties": {
                        "source_index": { "type": "integer" },
                        "translated_text": { "type": "string" }
                    },
                    "required": ["source_index", "translated_text"]
                }
            }
        },
        "required": ["lines"]
    })
}

fn source_texts(request: &TranslationRequest) -> Vec<String> {
    source_texts_from_lrc(&request.source_lrc)
}

fn source_texts_from_lrc(source_lrc: &str) -> Vec<String> {
    parse_source_lines(source_lrc)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

fn has_reliable_non_target_chunk(text: &str, target_language_code: &str) -> bool {
    language_detection_chunks(text)
        .into_iter()
        .filter_map(|chunk| whatlang::detect(&chunk))
        .any(|info| {
            normalize_detected_language_code(info.lang().code()) != target_language_code
                && info.confidence() >= CHUNK_LANGUAGE_DETECTION_CONFIDENCE
                && info.is_reliable()
        })
}

fn language_detection_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);

        if alphabetic_char_count(&current) >= CHUNK_LANGUAGE_DETECTION_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
    }

    if alphabetic_char_count(&current) >= CHUNK_LANGUAGE_DETECTION_CHARS {
        chunks.push(current);
    }

    chunks
}

fn alphabetic_char_count(value: &str) -> usize {
    value.chars().filter(|ch| ch.is_alphabetic()).count()
}

fn script_counts(value: &str) -> ScriptCounts {
    let mut counts = ScriptCounts::default();

    for ch in value.chars().filter(|ch| ch.is_alphabetic()) {
        let codepoint = ch as u32;

        if ch.is_ascii_alphabetic() || (0x00C0..=0x024F).contains(&codepoint) {
            counts.latin += 1;
        } else if (0xAC00..=0xD7AF).contains(&codepoint)
            || (0x1100..=0x11FF).contains(&codepoint)
            || (0x3130..=0x318F).contains(&codepoint)
        {
            counts.hangul += 1;
        } else if (0x3040..=0x30FF).contains(&codepoint) {
            counts.kana += 1;
        } else if (0x4E00..=0x9FFF).contains(&codepoint) {
            counts.cjk += 1;
        } else {
            counts.other_alpha += 1;
        }
    }

    counts
}

fn has_script_conflict(counts: &ScriptCounts, target_language_code: &str) -> bool {
    let total = counts.total_alpha();
    if total == 0 {
        return false;
    }

    match target_language_code {
        "en" | "de" | "es" | "fr" | "pt" | "it" => {
            counts.non_latin() as f64 / total as f64 >= LATIN_TARGET_NON_LATIN_CONFLICT_RATIO
        }
        "ko" => counts.hangul == 0 && counts.latin > counts.non_latin(),
        "ja" => counts.kana + counts.cjk == 0 && counts.latin > counts.non_latin(),
        "zh" => counts.cjk == 0 && counts.latin > counts.non_latin(),
        _ => false,
    }
}

fn require_config_value(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(anyhow!("{} is required", label))
    } else {
        Ok(trimmed.to_string())
    }
}

fn non_empty_or_default(value: &str, default_value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default_value.to_string()
    } else {
        trimmed.to_string()
    }
}

fn target_language_code(target_language: &str, uppercase: bool) -> String {
    let normalized = target_language.trim().to_lowercase();
    let code = match normalized.as_str() {
        "english" => "en",
        "german" | "deutsch" => "de",
        "spanish" | "español" => "es",
        "french" | "français" => "fr",
        "portuguese" | "português" => "pt",
        "italian" | "italiano" => "it",
        "japanese" | "日本語" => "ja",
        "korean" | "한국어" => "ko",
        "chinese" | "中文" => "zh",
        "russian" | "русский" => "ru",
        value if value.len() == 2 || value.len() == 5 => value,
        _ => "en",
    };

    if uppercase {
        code.to_uppercase()
    } else {
        code.to_string()
    }
}

fn target_language_label(target_language: &str) -> &str {
    let trimmed = target_language.trim();
    if trimmed.is_empty() {
        DEFAULT_TARGET_LANGUAGE
    } else {
        trimmed
    }
}

fn normalize_detected_language_code(code: &str) -> String {
    match code {
        "eng" => "en",
        "deu" => "de",
        "spa" => "es",
        "fra" => "fr",
        "por" => "pt",
        "ita" => "it",
        "jpn" => "ja",
        "kor" => "ko",
        "cmn" => "zh",
        "rus" => "ru",
        value => value,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lrc_from_lines(lines: &[&str]) -> String {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| format!("[00:{:02}.00]{}", index + 1, line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn source_hash_changes_when_original_changes() {
        let first = lyrics_source_hash("[00:01.00]안녕");
        let second = lyrics_source_hash("[00:01.00]안녕하세요");

        assert_ne!(first, second);
        assert_eq!(first, lyrics_source_hash("[00:01.00]안녕"));
    }

    #[test]
    fn settings_hash_does_not_change_for_export_mode_only() {
        let translated = translation_settings_hash(
            "gemini",
            "gemini-flash-latest",
            "English",
            TranslationExportMode::Translation,
        );
        let dual = translation_settings_hash(
            "gemini",
            "gemini-flash-latest",
            "English",
            TranslationExportMode::Dual,
        );

        assert_eq!(translated, dual);
    }

    #[test]
    fn target_language_defaults_to_english_when_config_is_blank() {
        let config = PersistentConfig {
            skip_tracks_with_synced_lyrics: false,
            skip_tracks_with_plain_lyrics: false,
            show_line_count: true,
            try_embed_lyrics: false,
            theme_mode: "auto".to_string(),
            lrclib_instance: "https://lrclib.net".to_string(),
            volume: 1.0,
            translation_auto_enabled: false,
            translation_target_language: " ".to_string(),
            translation_provider: "gemini".to_string(),
            translation_export_mode: "original".to_string(),
            translation_gemini_api_key: String::new(),
            translation_gemini_model: DEFAULT_GEMINI_MODEL.to_string(),
            translation_deepl_api_key: String::new(),
            translation_google_api_key: String::new(),
            translation_microsoft_api_key: String::new(),
            translation_microsoft_region: String::new(),
            translation_openai_base_url: String::new(),
            translation_openai_api_key: String::new(),
            translation_openai_model: String::new(),
        };

        assert_eq!(target_language_from_config(&config), "English");
        assert_eq!(
            settings_hash_from_config(&config),
            translation_settings_hash(
                "gemini",
                DEFAULT_GEMINI_MODEL,
                "English",
                TranslationExportMode::Original
            )
        );
    }

    #[test]
    fn validates_one_translation_for_each_source_line() {
        let source = "[00:01.00]첫 줄\n[00:02.00]둘째 줄";
        let json = r#"{"lines":[{"source_index":0,"translated_text":"First line"},{"source_index":1,"translated_text":"Second line"}]}"#;

        let lines = validate_translation_lines(source, json).unwrap();

        assert_eq!(
            lines,
            vec![
                TranslationLine {
                    source_index: 0,
                    translated_text: "First line".to_string(),
                },
                TranslationLine {
                    source_index: 1,
                    translated_text: "Second line".to_string(),
                }
            ]
        );
    }

    #[test]
    fn rejects_missing_reordered_or_duplicate_translation_indexes() {
        let source = "[00:01.00]첫 줄\n[00:02.00]둘째 줄";
        let missing = r#"{"lines":[{"source_index":0,"translated_text":"First line"}]}"#;
        let reordered = r#"{"lines":[{"source_index":1,"translated_text":"Second line"},{"source_index":0,"translated_text":"First line"}]}"#;
        let duplicate = r#"{"lines":[{"source_index":0,"translated_text":"First line"},{"source_index":0,"translated_text":"Again"}]}"#;

        assert!(validate_translation_lines(source, missing).is_err());
        assert!(validate_translation_lines(source, reordered).is_err());
        assert!(validate_translation_lines(source, duplicate).is_err());
    }

    #[test]
    fn builds_translation_only_lrc_with_original_timestamps() {
        let source = "[00:01.00]첫 줄\n[00:02.50]둘째 줄";
        let json = r#"{"lines":[{"source_index":0,"translated_text":"First line"},{"source_index":1,"translated_text":"Second line"}]}"#;

        let lrc = build_translated_lrc(source, json, TranslationExportMode::Translation).unwrap();

        assert_eq!(lrc, "[00:01.00]First line\n[00:02.50]Second line");
    }

    #[test]
    fn builds_dual_lrc_by_repeating_each_timestamp() {
        let source = "[00:01.00]첫 줄\n[00:02.50]둘째 줄";
        let json = r#"{"lines":[{"source_index":0,"translated_text":"First line"},{"source_index":1,"translated_text":"Second line"}]}"#;

        let lrc = build_translated_lrc(source, json, TranslationExportMode::Dual).unwrap();

        assert_eq!(
            lrc,
            "[00:01.00]첫 줄\n[00:01.00]First line\n[00:02.50]둘째 줄\n[00:02.50]Second line"
        );
    }

    #[test]
    fn detects_same_language_english_source_as_skip() {
        let source = "[00:09.07]You know I'm gonna give you ten out of ten\n[00:11.01]Know I'm gonna love you, love you to death\n[00:12.96]Baby, all night, again and again, yeah\n[00:16.88]You know I'm gonna give you ten out of ten";

        let decision = same_language_skip_decision(source, "English")
            .unwrap()
            .expect("English lyrics targeting English should be skipped");

        assert_eq!(decision.detected_language_code, "en");
        assert_eq!(decision.target_language_code, "en");
        assert!(decision.confidence >= SAME_LANGUAGE_SKIP_CONFIDENCE);
    }

    #[test]
    fn detects_same_language_repetitive_english_source_as_skip() {
        let source = test_lrc_from_lines(&[
            "One by one, two by two",
            "My walls come falling down",
            "Was lost and now I'm found",
            "Wasn't looking, wasn't searching",
            "You came without a sound",
            "So let 'em fall",
            "Let 'em fall, let 'em fall",
        ]);

        assert!(same_language_skip_decision(&source, "English")
            .unwrap()
            .is_some());
    }

    #[test]
    fn does_not_skip_korean_source_targeting_english() {
        let source = "[00:01.00]안녕하세요 오늘 밤은 너무 아름다워요\n[00:02.00]그대와 함께 노래하고 싶어요\n[00:03.00]이 마음을 영어로 전해 주세요";

        assert!(same_language_skip_decision(source, "English")
            .unwrap()
            .is_none());
    }

    #[test]
    fn does_not_skip_mixed_korean_english_source_targeting_english() {
        let source = "[00:01.00]아침에 눈을 뜨면 다가오는 햇살\n[00:01.00]When I open my eyes in the morning, sunlight comes toward me\n[00:02.00]햇살에 눈 비비고 일어나고\n[00:02.00]I rub my eyes in the sunlight and get up\n[00:03.00]누군가 청소를 한 깨끗해진 거리\n[00:03.00]A clean street, as if someone has swept it";

        assert!(same_language_skip_decision(source, "English")
            .unwrap()
            .is_none());
    }

    #[test]
    fn does_not_skip_uncertain_short_source() {
        let source = "[00:01.00]Oh\n[00:02.00]Yeah";

        assert!(same_language_skip_decision(source, "English")
            .unwrap()
            .is_none());
    }

    #[test]
    fn skipped_same_language_export_uses_original_lrc_for_translated_modes() {
        let source = "[00:01.00]Hello world\n[00:02.00]Again";

        assert_eq!(
            build_export_lrc_for_translation_status(
                source,
                "skipped_same_language",
                None,
                TranslationExportMode::Translation,
            )
            .unwrap(),
            source
        );
        assert_eq!(
            build_export_lrc_for_translation_status(
                source,
                "skipped_same_language",
                None,
                TranslationExportMode::Dual,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn extracts_structured_json_from_gemini_response() {
        let raw = r#"{
          "candidates": [{
            "content": {
              "parts": [{
                "text": "{\"lines\":[{\"source_index\":0,\"translated_text\":\"Hello\"}]}"
              }]
            }
          }]
        }"#;

        assert_eq!(
            structured_json_from_gemini_response(raw).unwrap(),
            r#"{"lines":[{"source_index":0,"translated_text":"Hello"}]}"#
        );
    }

    #[test]
    fn provider_status_error_includes_body_without_url_secret() {
        let url = reqwest::Url::parse(
            "https://translation.googleapis.com/language/translate/v2?key=secret-token",
        )
        .unwrap();
        let message = provider_status_error_message(
            "Google Translate",
            reqwest::StatusCode::BAD_REQUEST,
            &url,
            r#"{"error":{"message":"Invalid request payload"}}"#,
        );

        assert!(message.contains("Google Translate request failed with HTTP 400"));
        assert!(message.contains("Invalid request payload"));
        assert!(!message.contains("secret-token"));
        assert!(message.contains("key=REDACTED"));
    }

    #[test]
    fn extracts_structured_json_from_openai_response() {
        let raw = r#"{
          "choices": [{
            "message": {
              "content": "{\"lines\":[{\"source_index\":0,\"translated_text\":\"Hello\"}]}"
            }
          }]
        }"#;

        assert_eq!(
            structured_json_from_openai_response(raw).unwrap(),
            r#"{"lines":[{"source_index":0,"translated_text":"Hello"}]}"#
        );
    }

    #[test]
    fn converts_traditional_provider_responses_to_structured_json() {
        assert_eq!(
            structured_json_from_deepl_response(
                r#"{"translations":[{"text":"Hello"},{"text":"World"}]}"#
            )
            .unwrap(),
            r#"{"lines":[{"source_index":0,"translated_text":"Hello"},{"source_index":1,"translated_text":"World"}]}"#
        );

        assert_eq!(
            structured_json_from_google_response(
                r#"{"data":{"translations":[{"translatedText":"Hello"},{"translatedText":"World"}]}}"#
            )
            .unwrap(),
            r#"{"lines":[{"source_index":0,"translated_text":"Hello"},{"source_index":1,"translated_text":"World"}]}"#
        );

        assert_eq!(
            structured_json_from_microsoft_response(
                r#"[{"translations":[{"text":"Hello"}]},{"translations":[{"text":"World"}]}]"#
            )
            .unwrap(),
            r#"{"lines":[{"source_index":0,"translated_text":"Hello"},{"source_index":1,"translated_text":"World"}]}"#
        );
    }
}
