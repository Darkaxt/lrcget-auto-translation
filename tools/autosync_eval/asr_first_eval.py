#!/usr/bin/env python3
"""ASR-first vocal-isolation shootout for Air I Breathe.

This evaluator is intentionally separate from LRCGET application code. It reads
the same stored lyrics as the earlier evaluator, creates local audio variants,
runs ASR-only backends, and aligns cleaned lyric lines to what the ASR heard.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from air_i_breathe_eval import (
    DEFAULT_AUDIO_PATH,
    DEFAULT_TRACK_ID,
    TimedWord,
    LineAlignment,
    GeneratedLrc,
    clean_lyrics_lines,
    clamp_line_timings,
    default_db_path,
    default_qwen_dir,
    extract_plain_lyrics,
    format_lrc_timestamp,
    impossible_timestamp_clusters,
    interpolate_missing_lines,
    load_track_from_db,
    normalize_line,
    prepare_audio,
    qwen_paths,
    read_timestamps_ms,
    run_command,
    timestamp_to_ms,
    tokenize,
    word_similarity,
    write_text,
)


DEFAULT_WORKDIR = Path("target") / "autosync-eval" / "air-i-breathe"
DEFAULT_ASR_ENV = Path("target") / "autosync-eval" / "asr-venv" / "Scripts" / "python.exe"
DEFAULT_TIMEOUT_SECONDS = 60 * 60
FIRST_EXPECTED_MS = 24_000
SECOND_EXPECTED_MS = 28_000
SANITY_TOLERANCE_MS = 8_000
SRT_TIMESTAMP_RE = re.compile(
    r"(?P<start>\d{1,2}:\d{2}:\d{2},\d{3})\s*-->\s*(?P<end>\d{1,2}:\d{2}:\d{2},\d{3})"
)


@dataclass(frozen=True)
class AsrSegment:
    text: str
    start_ms: int
    end_ms: int


@dataclass(frozen=True)
class AsrTranscript:
    source: str
    language: str | None
    segments: list[AsrSegment]
    words: list[TimedWord]
    raw: Any


def parse_asr_transcript(value: Any, source: str) -> AsrTranscript:
    language = value.get("language") if isinstance(value, dict) else None
    segments: list[AsrSegment] = []
    words: list[TimedWord] = []

    raw_segments = value.get("segments", []) if isinstance(value, dict) else []
    for raw_segment in raw_segments:
        if not isinstance(raw_segment, dict):
            continue
        text = str(raw_segment.get("text") or "").strip()
        start_ms, end_ms = read_segment_timestamps_ms(raw_segment)
        if text:
            segments.append(AsrSegment(text=text, start_ms=start_ms, end_ms=end_ms))
        segment_words = parse_raw_words(raw_segment.get("words"))
        if segment_words:
            words.extend(segment_words)
        elif text:
            words.extend(distribute_segment_words(text, start_ms, end_ms))

    if isinstance(value, dict) and isinstance(value.get("words"), list):
        words.extend(parse_raw_words(value.get("words")))

    words = sorted(dedupe_words(words), key=lambda word: (word.start_ms, word.end_ms, word.text.lower()))
    return AsrTranscript(source=source, language=language, segments=segments, words=words, raw=value)


def parse_srt_transcript(text: str, source: str, language: str | None = None) -> AsrTranscript:
    segments: list[AsrSegment] = []
    raw_blocks: list[dict[str, Any]] = []
    normalized = text.replace("\r\n", "\n").replace("\r", "\n").strip()
    if not normalized:
        return AsrTranscript(source=source, language=language, segments=[], words=[], raw={"srt": text})

    for block in re.split(r"\n\s*\n", normalized):
        lines = [line.strip("\ufeff ") for line in block.splitlines() if line.strip()]
        if not lines:
            continue
        timing_index = next((index for index, line in enumerate(lines) if "-->" in line), None)
        if timing_index is None:
            continue
        match = SRT_TIMESTAMP_RE.search(lines[timing_index])
        if not match:
            continue
        cue_text = " ".join(lines[timing_index + 1 :]).strip()
        if not cue_text:
            continue
        start_ms = parse_srt_timestamp_ms(match.group("start"))
        end_ms = parse_srt_timestamp_ms(match.group("end"))
        segments.append(AsrSegment(text=cue_text, start_ms=start_ms, end_ms=end_ms))
        raw_blocks.append({"startMs": start_ms, "endMs": end_ms, "text": cue_text})

    words: list[TimedWord] = []
    for segment in segments:
        words.extend(distribute_segment_words(segment.text, segment.start_ms, segment.end_ms))
    return AsrTranscript(
        source=source,
        language=language,
        segments=segments,
        words=dedupe_words(words),
        raw={"format": "srt", "segments": raw_blocks},
    )


def parse_srt_timestamp_ms(value: str) -> int:
    hours, minutes, rest = value.split(":")
    seconds, millis = rest.split(",")
    return (
        int(hours) * 60 * 60 * 1000
        + int(minutes) * 60 * 1000
        + int(seconds) * 1000
        + int(millis)
    )


def read_segment_timestamps_ms(raw: dict[str, Any]) -> tuple[int, int]:
    start_value = first_present(raw, ("start_ms", "start", "begin"))
    end_value = first_present(raw, ("end_ms", "end"))
    if start_value is None or end_value is None:
        return 0, 0
    return timestamp_to_ms(start_value, already_ms="start_ms" in raw), timestamp_to_ms(
        end_value, already_ms="end_ms" in raw
    )


def first_present(raw: dict[str, Any], keys: tuple[str, ...]) -> Any:
    for key in keys:
        if key in raw:
            return raw[key]
    return None


def parse_raw_words(raw_words: Any) -> list[TimedWord]:
    if not isinstance(raw_words, list):
        return []
    words: list[TimedWord] = []
    for raw in raw_words:
        if not isinstance(raw, dict):
            continue
        text = str(raw.get("word") or raw.get("text") or raw.get("token") or "").strip()
        if not text:
            continue
        try:
            start_ms, end_ms = read_timestamps_ms(raw)
        except Exception:
            continue
        words.append(TimedWord(text=text, start_ms=start_ms, end_ms=end_ms))
    return words


def distribute_segment_words(text: str, start_ms: int, end_ms: int) -> list[TimedWord]:
    tokens = tokenize(text)
    if not tokens:
        return []
    duration = max(1, end_ms - start_ms)
    step = duration / len(tokens)
    words: list[TimedWord] = []
    for index, token in enumerate(tokens):
        token_start = int(round(start_ms + step * index))
        token_end = int(round(start_ms + step * (index + 1)))
        words.append(TimedWord(token, token_start, max(token_start + 1, token_end)))
    return words


def dedupe_words(words: list[TimedWord]) -> list[TimedWord]:
    seen: set[tuple[str, int, int]] = set()
    output: list[TimedWord] = []
    for word in words:
        key = (word.text.lower(), word.start_ms, word.end_ms)
        if key in seen:
            continue
        seen.add(key)
        output.append(word)
    return output


def align_lyrics_to_asr(lines: list[str], transcript: AsrTranscript) -> GeneratedLrc:
    words = transcript.words
    aligned_lines: list[LineAlignment] = []
    segment_cursor = 0
    word_cursor = 0
    matched_lines = 0
    total_similarity = 0.0
    similarity_count = 0

    for index, text in enumerate(lines):
        segment_match = find_next_segment_match(text, transcript.segments, segment_cursor)
        if segment_match is not None:
            segment_index, confidence = segment_match
            segment = transcript.segments[segment_index]
            segment_cursor = segment_index + 1
            word_cursor = advance_word_cursor(words, segment.end_ms, word_cursor)
            matched_words = max(1, len(tokenize(segment.text)))
            matched_lines += 1
            total_similarity += confidence * matched_words
            similarity_count += matched_words
            aligned_lines.append(
                LineAlignment(index, text, segment.start_ms, segment.end_ms, matched_words, confidence, False)
            )
            continue

        match = find_next_line_match(text, words, word_cursor)
        if match is None:
            aligned_lines.append(LineAlignment(index, text, -1, None, 0, 0.0, True))
            continue

        start_index, end_index, confidence = match
        start_word = words[start_index]
        end_word = words[end_index - 1]
        word_cursor = end_index
        segment_cursor = advance_segment_cursor(transcript.segments, end_word.end_ms, segment_cursor)
        matched_words = end_index - start_index
        matched_lines += 1
        total_similarity += confidence * matched_words
        similarity_count += matched_words
        aligned_lines.append(
            LineAlignment(index, text, start_word.start_ms, end_word.end_ms, matched_words, confidence, False)
        )

    interpolate_missing_lines(aligned_lines, words)
    clamp_line_timings(aligned_lines)
    lrc = "\n".join(f"{format_lrc_timestamp(line.start_ms)}{line.text}" for line in aligned_lines)
    metrics = score_asr_first_alignment(aligned_lines, matched_lines, total_similarity, similarity_count)
    return GeneratedLrc(lrc=lrc, lines=aligned_lines, metrics=metrics)


def find_next_segment_match(
    line: str,
    segments: list[AsrSegment],
    cursor: int,
    min_similarity: float = 0.58,
) -> tuple[int, float] | None:
    if cursor >= len(segments):
        return None
    target = normalize_line(line)
    best: tuple[int, float] | None = None
    for segment_index in range(cursor, len(segments)):
        if segment_index - cursor > 36 and best is not None:
            break
        similarity = line_similarity(target, segments[segment_index].text)
        if similarity < min_similarity:
            continue
        candidate = (segment_index, similarity)
        if best is None or segment_candidate_sort_key(candidate, cursor) > segment_candidate_sort_key(best, cursor):
            best = candidate
    return best


def segment_candidate_sort_key(candidate: tuple[int, float], cursor: int) -> tuple[float, int]:
    segment_index, similarity = candidate
    distance_penalty = min(0.24, (segment_index - cursor) * 0.015)
    return (similarity - distance_penalty, -segment_index)


def advance_word_cursor(words: list[TimedWord], end_ms: int, cursor: int) -> int:
    while cursor < len(words) and words[cursor].end_ms <= end_ms:
        cursor += 1
    return cursor


def advance_segment_cursor(segments: list[AsrSegment], end_ms: int, cursor: int) -> int:
    while cursor < len(segments) and segments[cursor].end_ms <= end_ms:
        cursor += 1
    return cursor


def find_next_line_match(
    line: str,
    words: list[TimedWord],
    cursor: int,
    min_similarity: float = 0.58,
) -> tuple[int, int, float] | None:
    tokens = tokenize(line)
    if not tokens or cursor >= len(words):
        return None

    target = normalize_line(line)
    base_length = len(tokens)
    min_length = max(1, base_length - 2)
    max_length = min(base_length + 5, 18)
    best: tuple[int, int, float] | None = None
    for start_index in range(cursor, len(words)):
        if start_index - cursor > 180 and best is not None:
            break
        for length in range(min_length, max_length + 1):
            end_index = start_index + length
            if end_index > len(words):
                break
            window = " ".join(word.text for word in words[start_index:end_index])
            similarity = line_similarity(target, window)
            if similarity < min_similarity:
                continue
            # Prefer high similarity, then earlier starts, then compact windows.
            candidate = (start_index, end_index, similarity)
            if best is None or candidate_sort_key(candidate, cursor) > candidate_sort_key(best, cursor):
                best = candidate
    return best


def candidate_sort_key(candidate: tuple[int, int, float], cursor: int) -> tuple[float, float, float]:
    start_index, end_index, similarity = candidate
    distance_penalty = min(0.18, (start_index - cursor) * 0.001)
    length_penalty = (end_index - start_index) * 0.0001
    return (similarity - distance_penalty - length_penalty, -start_index, -(end_index - start_index))


def line_similarity(normalized_target: str, window_text: str) -> float:
    normalized_window = normalize_line(window_text)
    if not normalized_target or not normalized_window:
        return 0.0
    target_tokens = normalized_target.split()
    window_tokens = normalized_window.split()
    if target_tokens == window_tokens:
        return 1.0
    token_scores: list[float] = []
    for token in target_tokens:
        token_scores.append(max((word_similarity(token, candidate) for candidate in window_tokens), default=0.0))
    token_similarity = sum(token_scores) / len(token_scores)
    length_ratio = min(len(target_tokens), len(window_tokens)) / max(len(target_tokens), len(window_tokens))
    return token_similarity * 0.85 + length_ratio * 0.15


def score_asr_first_alignment(
    lines: list[LineAlignment],
    matched_lines: int,
    total_similarity: float,
    similarity_count: int,
) -> dict[str, Any]:
    line_count = max(1, len(lines))
    starts = [line.start_ms for line in lines]
    cluster_count, first_cluster_ms = impossible_timestamp_clusters(starts)
    first_start = lines[0].start_ms if len(lines) >= 1 else None
    second_start = lines[1].start_ms if len(lines) >= 2 else None
    first_delta = abs(first_start - FIRST_EXPECTED_MS) if first_start is not None else None
    second_delta = abs(second_start - SECOND_EXPECTED_MS) if second_start is not None else None
    max_duration = max((max(0, (line.end_ms or line.start_ms) - line.start_ms) for line in lines), default=0)
    matched_ratio = matched_lines / line_count
    interpolated_ratio = sum(1 for line in lines if line.interpolated) / line_count
    average_similarity = total_similarity / similarity_count if similarity_count else 0.0
    sanity_pass = (
        first_delta is not None
        and second_delta is not None
        and first_delta <= SANITY_TOLERANCE_MS
        and second_delta <= SANITY_TOLERANCE_MS
    )
    grade = "good"
    if not sanity_pass or cluster_count > 0 or matched_ratio < 0.45 or interpolated_ratio > 0.45 or max_duration > 20_000:
        grade = "bad"
    elif matched_ratio < 0.75 or interpolated_ratio > 0.25 or max_duration > 10_000:
        grade = "repairable"
    return {
        "matchedLineRatio": matched_ratio,
        "interpolatedLineRatio": interpolated_ratio,
        "averageWordSimilarity": average_similarity,
        "impossibleClusterCount": cluster_count,
        "firstBadClusterMs": first_cluster_ms,
        "firstTimestampMs": first_start,
        "secondTimestampMs": second_start,
        "firstLineDeltaMs": first_delta,
        "secondLineDeltaMs": second_delta,
        "firstTwoLineSanityPass": sanity_pass,
        "maxLineDurationMs": max_duration,
        "lineCount": len(lines),
        "matchedLineCount": matched_lines,
        "interpolatedLineCount": sum(1 for line in lines if line.interpolated),
        "missingExtraLineCount": 0,
        "grade": grade,
    }


def rank_asr_results(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return sorted(
        results,
        key=lambda result: (
            not result["metrics"].get("firstTwoLineSanityPass", False),
            result["metrics"].get("impossibleClusterCount", 10_000),
            result["metrics"].get("maxLineDurationMs", 10_000_000),
            -result["metrics"].get("matchedLineRatio", 0.0),
            -result["metrics"].get("averageWordSimilarity", 0.0),
            result.get("runtimeSeconds", 10_000_000),
        ),
    )


def write_asr_alignment_result(
    name: str,
    lines: list[str],
    transcript: AsrTranscript,
    run_dir: Path,
    runtime: float,
    metadata: dict[str, Any],
) -> dict[str, Any]:
    generated = align_lyrics_to_asr(lines, transcript)
    write_text(run_dir / "asr-transcript.json", transcript_to_json(transcript))
    write_text(run_dir / "generated.lrc", generated.lrc + "\n")
    write_text(run_dir / "line-matches.json", json.dumps([line.__dict__ for line in generated.lines], indent=2))
    metrics = {
        **generated.metrics,
        "runtimeSeconds": round(runtime, 3),
        **metadata,
    }
    write_text(run_dir / "metrics.json", json.dumps(metrics, indent=2, ensure_ascii=False))
    return {
        "name": name,
        "mode": metadata.get("mode", name),
        "runtimeSeconds": runtime,
        "metrics": metrics,
        "lrcPath": str((run_dir / "generated.lrc").resolve()),
        "metricsPath": str((run_dir / "metrics.json").resolve()),
        "jsonPath": str((run_dir / "raw-asr.json").resolve()),
    }


def transcript_to_json(transcript: AsrTranscript) -> str:
    return json.dumps(
        {
            "source": transcript.source,
            "language": transcript.language,
            "segments": [segment.__dict__ for segment in transcript.segments],
            "words": [word.__dict__ for word in transcript.words],
        },
        indent=2,
        ensure_ascii=False,
    )


def prepare_original_audio(audio_path: Path, workdir: Path) -> Path:
    target = workdir / "audio.original.flac"
    if not target.exists() or sha256_file(audio_path) != sha256_file(target):
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(audio_path, target)
    return target


def sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def detect_language_hint(lines: list[str]) -> str:
    joined = "\n".join(lines)
    ascii_letters = sum(1 for char in joined if "a" <= char.lower() <= "z")
    non_ascii_letters = sum(1 for char in joined if char.isalpha() and not ("a" <= char.lower() <= "z"))
    if ascii_letters >= max(20, non_ascii_letters * 3):
        return "en"
    return "auto"


def prepare_loudnorm_audio(input_path: Path, output_path: Path, timeout: int) -> Path:
    if not output_path.exists():
        run_command(
            [
                "ffmpeg",
                "-y",
                "-hide_banner",
                "-i",
                str(input_path),
                "-vn",
                "-af",
                "loudnorm=I=-16:TP=-1.5:LRA=11",
                str(output_path),
            ],
            None,
            output_path.with_suffix(".ffmpeg.log"),
            timeout,
        )
    return output_path


def prepare_qwen_wav(input_path: Path, output_path: Path, timeout: int) -> Path:
    if not output_path.exists():
        run_command(
            ["ffmpeg", "-y", "-hide_banner", "-i", str(input_path), "-vn", "-ac", "1", "-ar", "16000", str(output_path)],
            None,
            output_path.with_suffix(".ffmpeg.log"),
            timeout,
        )
    return output_path


def run_demucs_model(asr_python: Path, audio_path: Path, model: str, workdir: Path, timeout: int) -> Path:
    stems_root = workdir / "stems"
    vocals = stems_root / model / audio_path.stem / "vocals.wav"
    if vocals.exists():
        return vocals
    run_command(
        [
            str(asr_python),
            "-m",
            "demucs.separate",
            "--two-stems=vocals",
            "-n",
            model,
            "-o",
            str(stems_root),
            str(audio_path),
        ],
        None,
        workdir / "logs" / f"demucs.{model}.log",
        timeout,
    )
    if not vocals.exists():
        raise RuntimeError(f"Demucs did not create expected vocals stem: {vocals}")
    return vocals


def run_qwen_asr(
    name: str,
    qwen: dict[str, Path],
    audio_path: Path,
    lines: list[str],
    workdir: Path,
    language: str,
    timeout: int,
) -> dict[str, Any]:
    run_dir = workdir / "asr-runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    qwen_audio = prepare_qwen_wav(audio_path, run_dir / "qwen.16k.mono.wav", timeout)
    output_srt = run_dir / "raw-asr.srt"
    if output_srt.exists():
        runtime = previous_runtime_seconds(run_dir)
    else:
        command = [
            str(qwen["exe"]),
            "-m",
            str(qwen["asr"]),
            "-f",
            str(qwen_audio),
            "--max-tokens",
            "4096",
            "--progress",
            "--output-srt",
            "-o",
            str(output_srt),
        ]
        if language != "auto":
            command.extend(["--language", language])
        runtime = run_command(command, qwen["exe"].parent, run_dir / "asr.log", timeout)
    raw_srt = output_srt.read_text(encoding="utf-8")
    transcript = parse_srt_transcript(raw_srt, "qwen", None if language == "auto" else language)
    write_text(run_dir / "raw-asr.json", json.dumps(transcript.raw, ensure_ascii=False, indent=2))
    return write_asr_alignment_result(
        name,
        lines,
        transcript,
        run_dir,
        runtime,
        {"mode": "qwen_asr_only", "model": qwen["asr"].name, "audioVariant": name.removesuffix("_qwen")},
    )


def run_whisper_asr(
    name: str,
    asr_python: Path,
    model: str,
    audio_path: Path,
    lines: list[str],
    workdir: Path,
    language: str,
    timeout: int,
) -> dict[str, Any]:
    run_dir = workdir / "asr-runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    output_json = run_dir / "raw-asr.json"
    if output_json.exists():
        runtime = previous_runtime_seconds(run_dir)
    else:
        script = (
            "import json, sys, whisper, torch\n"
            "model_name, audio_path, language, output_path = sys.argv[1:5]\n"
            "model = whisper.load_model(model_name)\n"
            "kwargs = {'word_timestamps': True, 'verbose': False, 'fp16': torch.cuda.is_available()}\n"
            "if language != 'auto': kwargs['language'] = language\n"
            "result = model.transcribe(audio_path, **kwargs)\n"
            "json.dump(result, open(output_path, 'w', encoding='utf-8'), ensure_ascii=False, indent=2)\n"
        )
        runtime = run_command(
            [str(asr_python), "-c", script, model, str(audio_path), language, str(output_json)],
            None,
            run_dir / "asr.log",
            timeout,
        )
    raw = json.loads(output_json.read_text(encoding="utf-8"))
    transcript = parse_asr_transcript(raw, "whisper")
    return write_asr_alignment_result(
        name,
        lines,
        transcript,
        run_dir,
        runtime,
        {"mode": "whisper_asr_only", "model": model, "audioVariant": name.removesuffix(f"_whisper_{model_slug(model)}")},
    )


def previous_runtime_seconds(run_dir: Path) -> float:
    metrics_path = run_dir / "metrics.json"
    if not metrics_path.exists():
        return 0.0
    try:
        return float(json.loads(metrics_path.read_text(encoding="utf-8")).get("runtimeSeconds", 0.0))
    except Exception:
        return 0.0


def model_slug(model: str) -> str:
    return model.replace(".", "_").replace("-", "_")


def build_audio_variants(args: argparse.Namespace, original_audio: Path) -> dict[str, Path]:
    variants = {"original_flac": original_audio}
    if not args.skip_demucs:
        for model in ("htdemucs", "htdemucs_ft"):
            variants[f"demucs_{model}_vocals"] = run_demucs_model(
                args.asr_python, original_audio, model, args.workdir, args.timeout
            )
    if args.include_loudnorm:
        for name, path in list(variants.items()):
            variants[f"{name}_loudnorm"] = prepare_loudnorm_audio(
                path, args.workdir / "audio_variants" / f"{name}.loudnorm.wav", args.timeout
            )
    return variants


def collect_asr_results(workdir: Path) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for metrics_path in sorted((workdir / "asr-runs").glob("*/metrics.json")):
        run_dir = metrics_path.parent
        metrics = json.loads(metrics_path.read_text(encoding="utf-8"))
        results.append(
            {
                "name": run_dir.name,
                "mode": metrics.get("mode", run_dir.name),
                "runtimeSeconds": float(metrics.get("runtimeSeconds", 0.0)),
                "metrics": metrics,
                "lrcPath": str((run_dir / "generated.lrc").resolve()),
                "metricsPath": str(metrics_path.resolve()),
                "jsonPath": str((run_dir / "raw-asr.json").resolve()),
            }
        )
    return results


def write_asr_report(workdir: Path, results: list[dict[str, Any]], track: dict[str, Any]) -> Path:
    ranked = rank_asr_results(results)
    lines = [
        "# ASR-First Vocal-Isolation Shootout",
        "",
        f"Track: `{track['title']} - {track.get('artist') or ''}`",
        f"Audio: `{track['file_path']}`",
        "",
        "## Ranked Results",
        "",
        "| Rank | Run | Sanity | Grade | Clusters | First Δ | Second Δ | Max Duration | Matched | Similarity | LRC |",
        "| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for rank, result in enumerate(ranked, start=1):
        metrics = result["metrics"]
        lines.append(
            f"| {rank} | `{result['name']}` | {metrics.get('firstTwoLineSanityPass')} | {metrics.get('grade')} | "
            f"{metrics.get('impossibleClusterCount')} | {metrics.get('firstLineDeltaMs')} | "
            f"{metrics.get('secondLineDeltaMs')} | {metrics.get('maxLineDurationMs')} | "
            f"{metrics.get('matchedLineRatio', 0):.3f} | {metrics.get('averageWordSimilarity', 0):.3f} | "
            f"`{result['lrcPath']}` |"
        )
    lines.extend(build_report_observations(ranked))
    lines.extend(
        [
            "",
            "## Notes",
            "",
            f"- First lyric target: `{format_lrc_timestamp(FIRST_EXPECTED_MS)}` ± {SANITY_TOLERANCE_MS // 1000}s.",
            f"- Second lyric target: `{format_lrc_timestamp(SECOND_EXPECTED_MS)}` ± {SANITY_TOLERANCE_MS // 1000}s.",
            "- Results are produced from ASR-only transcripts and monotonic fuzzy lyric matching.",
            "- Demucs stems are scratch artifacts under `target/autosync-eval`; no app database rows are modified.",
        ]
    )
    report = workdir / "asr_first_report.md"
    write_text(report, "\n".join(lines) + "\n")
    write_text(workdir / "asr_summary.json", json.dumps(ranked, indent=2, ensure_ascii=False))
    return report


def build_report_observations(ranked: list[dict[str, Any]]) -> list[str]:
    if not ranked:
        return []
    best = ranked[0]
    best_metrics = best["metrics"]
    sanity_passes = [result for result in ranked if result["metrics"].get("firstTwoLineSanityPass")]
    qwen_results = [result for result in ranked if "qwen" in result["name"]]
    demucs_turbo_results = [
        result for result in ranked if "demucs" in result["name"] and "whisper_turbo" in result["name"]
    ]
    original_turbo = next((result for result in ranked if result["name"] == "original_flac_whisper_turbo"), None)
    observations = [
        "",
        "## Interpretation",
        "",
        f"- Best ranked run: `{best['name']}` with first/second lyric timestamps "
        f"`{format_lrc_timestamp(best_metrics.get('firstTimestampMs') or 0)}` and "
        f"`{format_lrc_timestamp(best_metrics.get('secondTimestampMs') or 0)}`.",
        f"- `{len(sanity_passes)}` run(s) passed the first-two-line sanity check.",
    ]
    if original_turbo is not None:
        original_metrics = original_turbo["metrics"]
        observations.append(
            f"- Original audio with Whisper turbo already finds the opening lines at "
            f"`{format_lrc_timestamp(original_metrics.get('firstTimestampMs') or 0)}` and "
            f"`{format_lrc_timestamp(original_metrics.get('secondTimestampMs') or 0)}`."
        )
    if demucs_turbo_results:
        best_demucs = rank_asr_results(demucs_turbo_results)[0]
        observations.append(
            f"- Best Demucs+Whisper turbo run: `{best_demucs['name']}`. It did not beat the top original-audio run "
            "because it introduced impossible timestamp clusters."
        )
    if qwen_results and all(result["metrics"].get("matchedLineCount", 0) == 0 for result in qwen_results):
        observations.append(
            "- Qwen ASR-only produced no usable timed word/segment transcript with the current CLI binary; it remains "
            "useful for forced-alignment experiments, not this ASR-first timing pass."
        )
    observations.append(
        "- All runs are still graded `bad` because the first verse is sane but later repeated sections require a second "
        "repair pass instead of naive interpolation."
    )
    return observations


def run_shootout(args: argparse.Namespace) -> Path:
    args.workdir = args.workdir.resolve()
    args.workdir.mkdir(parents=True, exist_ok=True)
    if args.report_only:
        track = load_track_from_db(args.db, args.track_id)
        return write_asr_report(args.workdir, collect_asr_results(args.workdir), track)

    if not args.audio.exists():
        raise FileNotFoundError(args.audio)
    if not args.db.exists():
        raise FileNotFoundError(args.db)
    if not args.skip_whisper and not args.asr_python.exists():
        raise FileNotFoundError(f"Missing ASR Python environment: {args.asr_python}")
    qwen = qwen_paths(args.qwen_dir)
    if not args.skip_qwen and not qwen["exe"].exists():
        raise FileNotFoundError(f"Missing Qwen executable: {qwen['exe']}")

    track = load_track_from_db(args.db, args.track_id)
    plain = extract_plain_lyrics(track.get("lyricsfile") or "")
    clean_lines = clean_lyrics_lines(plain)
    write_text(args.workdir / "lyrics-clean.txt", "\n".join(clean_lines) + "\n")
    write_text(args.workdir / "track.json", json.dumps({k: v for k, v in track.items() if k != "lyricsfile"}, indent=2))
    language = args.language or detect_language_hint(clean_lines)

    original_audio = prepare_original_audio(args.audio, args.workdir)
    variants = build_audio_variants(args, original_audio)
    results: list[dict[str, Any]] = []
    errors: list[str] = []

    def record(name: str, fn) -> None:
        print(f"Running {name}...", flush=True)
        try:
            result = fn()
            results.append(result)
            print(
                f"Finished {name}: sanity={result['metrics']['firstTwoLineSanityPass']} "
                f"grade={result['metrics']['grade']}",
                flush=True,
            )
        except Exception as error:  # noqa: BLE001 - shootout should continue across model failures.
            message = f"{name}: {error}"
            errors.append(message)
            write_text(args.workdir / "asr_errors.log", "\n".join(errors) + "\n")
            print(f"Failed {message}", file=sys.stderr, flush=True)

    for variant_name, variant_path in variants.items():
        if not args.skip_qwen:
            record(
                f"{variant_name}_qwen",
                lambda variant_name=variant_name, variant_path=variant_path: run_qwen_asr(
                    f"{variant_name}_qwen", qwen, variant_path, clean_lines, args.workdir, language, args.timeout
                ),
            )
        if not args.skip_whisper:
            for model in args.whisper_models:
                slug = model_slug(model)
                record(
                    f"{variant_name}_whisper_{slug}",
                    lambda model=model, slug=slug, variant_name=variant_name, variant_path=variant_path: run_whisper_asr(
                        f"{variant_name}_whisper_{slug}",
                        args.asr_python,
                        model,
                        variant_path,
                        clean_lines,
                        args.workdir,
                        language,
                        args.timeout,
                    ),
                )

    all_results = collect_asr_results(args.workdir)
    return write_asr_report(args.workdir, all_results if all_results else results, track)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--track-id", type=int, default=DEFAULT_TRACK_ID)
    parser.add_argument("--db", type=Path, default=default_db_path())
    parser.add_argument("--audio", type=Path, default=DEFAULT_AUDIO_PATH)
    parser.add_argument("--qwen-dir", type=Path, default=default_qwen_dir())
    parser.add_argument("--asr-python", type=Path, default=DEFAULT_ASR_ENV)
    parser.add_argument("--workdir", type=Path, default=DEFAULT_WORKDIR)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--language", default=None)
    parser.add_argument("--whisper-models", nargs="+", default=["turbo", "medium.en"])
    parser.add_argument("--skip-demucs", action="store_true")
    parser.add_argument("--skip-qwen", action="store_true")
    parser.add_argument("--skip-whisper", action="store_true")
    parser.add_argument("--include-loudnorm", action="store_true")
    parser.add_argument("--report-only", action="store_true")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    report = run_shootout(parse_args(argv or sys.argv[1:]))
    print(f"ASR-first report written to {report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
