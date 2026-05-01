#!/usr/bin/env python3
"""CLI-only auto-sync evaluation pipeline for Air I Breathe.

This script deliberately does not call LRCGET commands or mutate its database.
It reads the current lyrics from SQLite, runs ffmpeg/Qwen directly, and writes
all experiment artifacts under a scratch directory.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shlex
import sqlite3
import subprocess
import sys
import time
from dataclasses import dataclass
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any, Iterable


DEFAULT_TRACK_ID = 595
DEFAULT_AUDIO_PATH = Path(r"H:\My Drive\Music\Albums\Sub Focus\Portals\01 - Air I Breathe.flac")
DEFAULT_WORKDIR = Path("target") / "autosync-eval" / "air-i-breathe"
DEFAULT_TIMEOUT_SECONDS = 60 * 60
JUNK_LINES = {
    "lyricsvideoslisten",
    "lyrics video listen",
    "lyricsvideos",
    "lyrics video",
    "listen",
}


@dataclass(frozen=True)
class TimedWord:
    text: str
    start_ms: int
    end_ms: int


@dataclass
class LineAlignment:
    index: int
    text: str
    start_ms: int
    end_ms: int | None
    matched_words: int
    confidence: float
    interpolated: bool


@dataclass
class GeneratedLrc:
    lrc: str
    lines: list[LineAlignment]
    metrics: dict[str, Any]


def default_db_path() -> Path:
    return Path(os.environ["APPDATA"]) / "net.lrclib.lrcget" / "db.sqlite3"


def default_qwen_dir() -> Path:
    return Path(os.environ["APPDATA"]) / "net.lrclib.lrcget" / "autosync" / "qwen3_asr_cpp"


def extract_plain_lyrics(lyricsfile: str) -> str:
    lines = lyricsfile.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    plain_start = None
    for index, line in enumerate(lines):
        if line.strip() in {"plain: |-", "plain: |", "plain:"}:
            plain_start = index + 1
            break
    if plain_start is None:
        return ""

    plain_lines: list[str] = []
    for line in lines[plain_start:]:
        if line and not line.startswith((" ", "\t")):
            break
        plain_lines.append(line[2:] if line.startswith("  ") else line.lstrip("\t"))
    return "\n".join(plain_lines).rstrip("\n")


def clean_lyrics_lines(plain_lyrics: str) -> list[str]:
    cleaned: list[str] = []
    for raw_line in plain_lyrics.replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        line = raw_line.strip()
        if not line:
            continue
        if normalize_compact(line) in JUNK_LINES:
            continue
        cleaned.append(line)
    return cleaned


def non_empty_lines(plain_lyrics: str) -> list[str]:
    return [line.strip() for line in plain_lyrics.splitlines() if line.strip()]


def select_anchor_lines(lines: list[str], limit: int = 10) -> list[str]:
    counts = normalized_line_counts(lines)

    candidates: list[tuple[float, int, str]] = []
    for index, line in enumerate(lines):
        tokens = tokenize(line)
        normalized = normalize_line(line)
        if len(tokens) < 4 or counts.get(normalized, 0) != 1:
            continue
        score = len(tokens) + len(set(tokens)) * 0.75
        candidates.append((score, index, line))

    chosen = sorted(candidates, key=lambda item: (-item[0], item[1]))[:limit]
    return [line for _, _, line in sorted(chosen, key=lambda item: item[1])]


def normalized_line_counts(lines: list[str]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for line in lines:
        normalized = normalize_line(line)
        if not normalized:
            continue
        counts[normalized] = counts.get(normalized, 0) + 1
    return counts


def load_track_from_db(db_path: Path, track_id: int) -> dict[str, Any]:
    connection = sqlite3.connect(db_path)
    connection.row_factory = sqlite3.Row
    row = connection.execute(
        """
        select t.id, t.title, t.file_path, t.duration, ar.name as artist, al.name as album, lf.lyricsfile
        from tracks t
        left join artists ar on ar.id=t.artist_id
        left join albums al on al.id=t.album_id
        left join lyricsfiles lf on lf.track_id=t.id
        where t.id=?
        """,
        (track_id,),
    ).fetchone()
    if row is None:
        raise RuntimeError(f"Track {track_id} not found in {db_path}")
    return dict(row)


def parse_qwen_words(value: Any) -> list[TimedWord]:
    raw_words: list[dict[str, Any]] = []
    if isinstance(value, list):
        raw_words.extend(item for item in value if isinstance(item, dict))
    elif isinstance(value, dict):
        if isinstance(value.get("words"), list):
            raw_words.extend(item for item in value["words"] if isinstance(item, dict))
        if isinstance(value.get("segments"), list):
            for segment in value["segments"]:
                if isinstance(segment, dict) and isinstance(segment.get("words"), list):
                    raw_words.extend(item for item in segment["words"] if isinstance(item, dict))

    words: list[TimedWord] = []
    for raw in raw_words:
        text = str(raw.get("word") or raw.get("text") or raw.get("token") or "").strip()
        if not text:
            continue
        start_ms, end_ms = read_timestamps_ms(raw)
        words.append(TimedWord(text=text, start_ms=start_ms, end_ms=end_ms))
    return sorted(words, key=lambda word: (word.start_ms, word.end_ms))


def read_timestamps_ms(raw: dict[str, Any]) -> tuple[int, int]:
    timestamp = raw.get("timestamp")
    if isinstance(timestamp, list) and len(timestamp) >= 2:
        return timestamp_to_ms(timestamp[0]), timestamp_to_ms(timestamp[1])

    start_value = first_present(raw, ("start_ms", "start", "begin", "start_time"))
    end_value = first_present(raw, ("end_ms", "end", "end_time"))
    if start_value is None or end_value is None:
        raise ValueError(f"Missing word timestamps: {raw}")
    return timestamp_to_ms(start_value, already_ms="start_ms" in raw), timestamp_to_ms(
        end_value, already_ms="end_ms" in raw
    )


def first_present(raw: dict[str, Any], keys: tuple[str, ...]) -> Any:
    for key in keys:
        if key in raw:
            return raw[key]
    return None


def timestamp_to_ms(value: Any, already_ms: bool = False) -> int:
    numeric = float(value)
    if already_ms or numeric > 10_000:
        return int(round(numeric))
    return int(round(numeric * 1000))


def generate_lrc_from_words(lines: list[str], words: list[TimedWord]) -> GeneratedLrc:
    usable_words = trim_unusable_leading_words(words)
    script_tokens: list[tuple[int, str]] = []
    for index, line in enumerate(lines):
        for token in tokenize(line):
            script_tokens.append((index, token))

    matches_by_line: dict[int, list[tuple[TimedWord, float]]] = {}
    word_cursor = 0
    lookahead = 120
    for line_index, token in script_tokens:
        best: tuple[int, float] | None = None
        for word_index in range(word_cursor, min(len(usable_words), word_cursor + lookahead)):
            similarity = word_similarity(token, usable_words[word_index].text)
            if similarity < 0.45:
                continue
            if best is None or similarity > best[1]:
                best = (word_index, similarity)
        if best is None:
            continue
        word_index, similarity = best
        word_cursor = word_index + 1
        matches_by_line.setdefault(line_index, []).append((usable_words[word_index], similarity))

    aligned_lines: list[LineAlignment] = []
    matched_lines = 0
    total_similarity = 0.0
    similarity_count = 0
    for index, text in enumerate(lines):
        matches = matches_by_line.get(index, [])
        if matches:
            matched_lines += 1
            start_ms = min(word.start_ms for word, _ in matches)
            end_ms = max(word.end_ms for word, _ in matches)
            confidence = sum(similarity for _, similarity in matches) / len(matches)
            total_similarity += sum(similarity for _, similarity in matches)
            similarity_count += len(matches)
            aligned_lines.append(
                LineAlignment(index, text, start_ms, end_ms, len(matches), confidence, False)
            )
        else:
            aligned_lines.append(LineAlignment(index, text, -1, None, 0, 0.0, True))

    interpolate_missing_lines(aligned_lines, usable_words)
    clamp_line_timings(aligned_lines)
    lrc = "\n".join(f"{format_lrc_timestamp(line.start_ms)}{line.text}" for line in aligned_lines)
    metrics = score_alignment(aligned_lines, matched_lines, total_similarity, similarity_count)
    return GeneratedLrc(lrc=lrc, lines=aligned_lines, metrics=metrics)


def generate_first_occurrence_lrc(
    lines: list[str],
    words: list[TimedWord],
    min_similarity: float = 0.72,
) -> GeneratedLrc:
    usable_words = trim_unusable_leading_words(words)
    aligned_lines: list[LineAlignment] = []
    matched_lines = 0
    total_similarity = 0.0
    similarity_count = 0

    for index, text in enumerate(lines):
        match = find_first_line_occurrence(text, usable_words, min_similarity)
        if match is None:
            aligned_lines.append(LineAlignment(index, text, -1, None, 0, 0.0, True))
            continue

        start_ms, end_ms, confidence, matched_words = match
        matched_lines += 1
        total_similarity += confidence * matched_words
        similarity_count += matched_words
        aligned_lines.append(LineAlignment(index, text, start_ms, end_ms, matched_words, confidence, False))

    interpolate_missing_lines(aligned_lines, usable_words)
    lrc = "\n".join(f"{format_lrc_timestamp(line.start_ms)}{line.text}" for line in aligned_lines)
    metrics = score_alignment(aligned_lines, matched_lines, total_similarity, similarity_count)
    return GeneratedLrc(lrc=lrc, lines=aligned_lines, metrics=metrics)


def find_first_line_occurrence(
    line: str,
    words: list[TimedWord],
    min_similarity: float,
) -> tuple[int, int, float, int] | None:
    tokens = tokenize(line)
    if not tokens or len(words) < len(tokens):
        return None

    min_token_similarity = 0.58
    min_token_coverage = 0.72
    for start_index in range(0, len(words) - len(tokens) + 1):
        similarities = [
            word_similarity(token, words[start_index + token_index].text)
            for token_index, token in enumerate(tokens)
        ]
        average_similarity = sum(similarities) / len(similarities)
        token_coverage = sum(1 for similarity in similarities if similarity >= min_token_similarity) / len(similarities)
        if average_similarity >= min_similarity and token_coverage >= min_token_coverage:
            start_word = words[start_index]
            end_word = words[start_index + len(tokens) - 1]
            return start_word.start_ms, end_word.end_ms, average_similarity, len(tokens)
    return None


def trim_unusable_leading_words(words: list[TimedWord]) -> list[TimedWord]:
    for index, word in enumerate(words):
        if word.start_ms != 0 or word.end_ms != 0:
            return words[index:]
    return words


def interpolate_missing_lines(lines: list[LineAlignment], words: list[TimedWord]) -> None:
    fallback_end = max((word.end_ms for word in words), default=0)
    known = [index for index, line in enumerate(lines) if line.start_ms >= 0]
    if not known:
        return

    for index, line in enumerate(lines):
        if line.start_ms >= 0:
            continue
        previous_known = max((known_index for known_index in known if known_index < index), default=None)
        next_known = min((known_index for known_index in known if known_index > index), default=None)
        if previous_known is None and next_known is None:
            line.start_ms = 0
        elif previous_known is None:
            line.start_ms = max(0, lines[next_known].start_ms - (next_known - index) * 1500)
        elif next_known is None:
            line.start_ms = min(fallback_end, lines[previous_known].start_ms + (index - previous_known) * 1500)
        else:
            span = lines[next_known].start_ms - lines[previous_known].start_ms
            step = span / (next_known - previous_known)
            line.start_ms = int(round(lines[previous_known].start_ms + step * (index - previous_known)))
        line.end_ms = line.start_ms + 300


def clamp_line_timings(lines: list[LineAlignment]) -> None:
    for index, line in enumerate(lines):
        if line.start_ms < 0:
            line.start_ms = 0
        if index + 1 < len(lines) and lines[index + 1].start_ms <= line.start_ms:
            lines[index + 1].start_ms = line.start_ms + 20
        if line.end_ms is None or line.end_ms <= line.start_ms:
            line.end_ms = line.start_ms + 300


def score_alignment(
    lines: list[LineAlignment],
    matched_lines: int,
    total_similarity: float,
    similarity_count: int,
) -> dict[str, Any]:
    line_count = max(1, len(lines))
    matched_line_ratio = matched_lines / line_count
    interpolated_line_ratio = sum(1 for line in lines if line.interpolated) / line_count
    average_word_similarity = total_similarity / similarity_count if similarity_count else 0.0
    cluster_count, first_cluster_ms = impossible_timestamp_clusters([line.start_ms for line in lines])
    first_timestamp_ms = lines[0].start_ms if lines else None
    grade = "good"
    if cluster_count > 0 or matched_line_ratio < 0.35 or interpolated_line_ratio > 0.50:
        grade = "bad"
    elif matched_line_ratio < 0.75 or interpolated_line_ratio > 0.25:
        grade = "repairable"
    return {
        "matchedLineRatio": matched_line_ratio,
        "interpolatedLineRatio": interpolated_line_ratio,
        "averageWordSimilarity": average_word_similarity,
        "impossibleClusterCount": cluster_count,
        "firstBadClusterMs": first_cluster_ms,
        "firstTimestampMs": first_timestamp_ms,
        "lineCount": len(lines),
        "matchedLineCount": matched_lines,
        "interpolatedLineCount": sum(1 for line in lines if line.interpolated),
        "missingExtraLineCount": 0,
        "grade": grade,
    }


def impossible_timestamp_clusters(starts_ms: list[int], threshold_ms: int = 100, min_size: int = 3) -> tuple[int, int | None]:
    clusters = 0
    first_cluster_ms: int | None = None
    current: list[int] = []
    for start in starts_ms:
        if not current or start - current[-1] < threshold_ms:
            current.append(start)
            continue
        if len(current) >= min_size:
            clusters += 1
            first_cluster_ms = current[0] if first_cluster_ms is None else first_cluster_ms
        current = [start]
    if len(current) >= min_size:
        clusters += 1
        first_cluster_ms = current[0] if first_cluster_ms is None else first_cluster_ms
    return clusters, first_cluster_ms


def normalize_compact(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "", value.lower())


def normalize_line(value: str) -> str:
    return " ".join(tokenize(value))


def tokenize(value: str) -> list[str]:
    return re.findall(r"[a-z0-9]+", value.lower())


def word_similarity(left: str, right: str) -> float:
    left_norm = "".join(tokenize(left))
    right_norm = "".join(tokenize(right))
    if not left_norm or not right_norm:
        return 0.0
    if left_norm == right_norm:
        return 1.0
    if left_norm in right_norm or right_norm in left_norm:
        return max(0.72, min(len(left_norm), len(right_norm)) / max(len(left_norm), len(right_norm)))
    return SequenceMatcher(None, left_norm, right_norm).ratio()


def format_lrc_timestamp(ms: int) -> str:
    ms = max(0, int(ms))
    minutes = ms // 60_000
    seconds = (ms % 60_000) // 1000
    centiseconds = (ms % 1000) // 10
    return f"[{minutes:02d}:{seconds:02d}.{centiseconds:02d}]"


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def run_command(command: list[str], cwd: Path | None, log_path: Path, timeout: int) -> float:
    started = time.perf_counter()
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("w", encoding="utf-8", newline="\n") as log:
        log.write("$ " + " ".join(shlex.quote(part) for part in command) + "\n\n")
        process = subprocess.run(
            command,
            cwd=str(cwd) if cwd else None,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
        if process.stdout:
            log.write("STDOUT\n")
            log.write(process.stdout)
            log.write("\n")
        if process.stderr:
            log.write("STDERR\n")
            log.write(process.stderr)
            log.write("\n")
        log.write(f"\nEXIT {process.returncode}\n")
    elapsed = time.perf_counter() - started
    if process.returncode != 0:
        raise RuntimeError(f"Command failed ({process.returncode}); see {log_path}")
    return elapsed


def prepare_audio(audio_path: Path, workdir: Path, timeout: int) -> dict[str, Path]:
    raw = workdir / "audio.raw.16k.mono.wav"
    loudnorm = workdir / "audio.loudnorm.16k.mono.wav"
    if not raw.exists():
        run_command(
            ["ffmpeg", "-y", "-hide_banner", "-i", str(audio_path), "-vn", "-ac", "1", "-ar", "16000", str(raw)],
            None,
            workdir / "logs" / "ffmpeg.raw.log",
            timeout,
        )
    if not loudnorm.exists():
        run_command(
            [
                "ffmpeg",
                "-y",
                "-hide_banner",
                "-i",
                str(audio_path),
                "-vn",
                "-af",
                "loudnorm=I=-16:TP=-1.5:LRA=11",
                "-ac",
                "1",
                "-ar",
                "16000",
                str(loudnorm),
            ],
            None,
            workdir / "logs" / "ffmpeg.loudnorm.log",
            timeout,
        )
    return {"raw": raw, "loudnorm": loudnorm}


def qwen_paths(qwen_dir: Path) -> dict[str, Path]:
    return {
        "exe": qwen_dir / "qwen3-asr-cli.exe",
        "asr": qwen_dir / "models" / "qwen3-asr-0.6b-q8_0.gguf",
        "aligner": qwen_dir / "models" / "qwen3-forced-aligner-0.6b-q4_k_m.gguf",
    }


def run_qwen_forced_align(
    name: str,
    qwen: dict[str, Path],
    wav_path: Path,
    lines: list[str],
    workdir: Path,
    timeout: int,
    language: str = "en",
    offset_ms: int = 0,
) -> dict[str, Any]:
    run_dir = workdir / "runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    text = "\n".join(lines)
    output_json = run_dir / "qwen.json"
    runtime = run_command(
        [
            str(qwen["exe"]),
            "-m",
            str(qwen["aligner"]),
            "-f",
            str(wav_path),
            "--align",
            "--text",
            text,
            "--language",
            language,
            "-o",
            str(output_json),
        ],
        qwen["exe"].parent,
        run_dir / "qwen.log",
        timeout,
    )
    words = load_words_from_json(output_json, offset_ms=offset_ms)
    return write_run_result(name, lines, words, run_dir, runtime, mode="forced_align")


def run_qwen_transcribe_align(
    name: str,
    qwen: dict[str, Path],
    wav_path: Path,
    lines: list[str],
    workdir: Path,
    timeout: int,
    max_tokens: int,
) -> dict[str, Any]:
    run_dir = workdir / "runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    output_json = run_dir / "qwen.json"
    runtime = run_command(
        [
            str(qwen["exe"]),
            "-m",
            str(qwen["asr"]),
            "--aligner-model",
            str(qwen["aligner"]),
            "-f",
            str(wav_path),
            "--transcribe-align",
            "--max-tokens",
            str(max_tokens),
            "--progress",
            "-o",
            str(output_json),
        ],
        qwen["exe"].parent,
        run_dir / "qwen.log",
        timeout,
    )
    words = load_words_from_json(output_json)
    return write_run_result(name, lines, words, run_dir, runtime, mode=f"transcribe_align_{max_tokens}")


def load_words_from_json(path: Path, offset_ms: int = 0) -> list[TimedWord]:
    value = json.loads(path.read_text(encoding="utf-8"))
    words = parse_qwen_words(value)
    if offset_ms:
        words = [TimedWord(word.text, word.start_ms + offset_ms, word.end_ms + offset_ms) for word in words]
    return words


def write_run_result(
    name: str,
    lines: list[str],
    words: list[TimedWord],
    run_dir: Path,
    runtime: float,
    mode: str,
) -> dict[str, Any]:
    generated = generate_lrc_from_words(lines, words)
    word_json = [word.__dict__ for word in words]
    write_text(run_dir / "generated.lrc", generated.lrc)
    write_text(run_dir / "words.normalized.json", json.dumps(word_json, indent=2, ensure_ascii=False))
    metrics = {
        **generated.metrics,
        "runtimeSeconds": round(runtime, 3),
        "mode": mode,
    }
    write_text(run_dir / "metrics.json", json.dumps(metrics, indent=2, ensure_ascii=False))
    return {
        "name": name,
        "mode": mode,
        "runtimeSeconds": runtime,
        "metrics": generated.metrics,
        "lrcPath": str((run_dir / "generated.lrc").resolve()),
        "metricsPath": str((run_dir / "metrics.json").resolve()),
        "jsonPath": str((run_dir / "qwen.json").resolve()),
    }


def write_first_occurrence_search_result(
    name: str,
    lines: list[str],
    words: list[TimedWord],
    workdir: Path,
    source_json: Path,
) -> dict[str, Any]:
    run_dir = workdir / "runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    generated = generate_first_occurrence_lrc(lines, words)
    write_text(run_dir / "generated.lrc", generated.lrc)
    if source_json.exists():
        write_text(run_dir / "qwen.json", source_json.read_text(encoding="utf-8"))
    write_text(run_dir / "words.normalized.json", json.dumps([word.__dict__ for word in words], indent=2, ensure_ascii=False))
    write_text(run_dir / "line-matches.json", json.dumps([line.__dict__ for line in generated.lines], indent=2, ensure_ascii=False))
    metrics = {
        **generated.metrics,
        "runtimeSeconds": 0.0,
        "mode": "first_occurrence_search",
        "sourceJson": str(source_json.resolve()),
    }
    write_text(run_dir / "metrics.json", json.dumps(metrics, indent=2, ensure_ascii=False))
    return {
        "name": name,
        "mode": "first_occurrence_search",
        "runtimeSeconds": 0.0,
        "metrics": generated.metrics,
        "lrcPath": str((run_dir / "generated.lrc").resolve()),
        "metricsPath": str((run_dir / "metrics.json").resolve()),
        "jsonPath": str((run_dir / "qwen.json").resolve()),
    }


def run_chunked_alignment(
    qwen: dict[str, Path],
    audio_raw: Path,
    lines: list[str],
    transcription_result: dict[str, Any],
    workdir: Path,
    timeout: int,
) -> dict[str, Any] | None:
    transcription_words = load_words_from_json(Path(transcription_result["jsonPath"]))
    approximate = generate_lrc_from_words(lines, transcription_words)
    chunks = build_anchor_chunks(lines, approximate.lines)
    if not chunks:
        return None

    run_dir = workdir / "runs" / "chunked_anchor_forced_align"
    chunks_dir = run_dir / "chunks"
    chunks_dir.mkdir(parents=True, exist_ok=True)
    merged_lines: list[LineAlignment] = []
    total_runtime = 0.0
    logs: list[str] = []
    for chunk_index, chunk in enumerate(chunks):
        start_ms, end_ms, start_line, end_line = chunk
        if end_ms <= start_ms or end_line <= start_line:
            continue
        clip_path = chunks_dir / f"chunk-{chunk_index:02d}.wav"
        duration_seconds = max(0.5, (end_ms - start_ms) / 1000)
        total_runtime += run_command(
            [
                "ffmpeg",
                "-y",
                "-hide_banner",
                "-ss",
                f"{start_ms / 1000:.3f}",
                "-t",
                f"{duration_seconds:.3f}",
                "-i",
                str(audio_raw),
                str(clip_path),
            ],
            None,
            chunks_dir / f"chunk-{chunk_index:02d}.ffmpeg.log",
            timeout,
        )
        chunk_lines = lines[start_line:end_line]
        if not chunk_lines:
            continue
        chunk_name = f"chunked_anchor_forced_align/chunk-{chunk_index:02d}"
        result = run_qwen_forced_align(
            chunk_name,
            qwen,
            clip_path,
            chunk_lines,
            workdir,
            timeout,
            offset_ms=start_ms,
        )
        total_runtime += result["runtimeSeconds"]
        chunk_result_dir = workdir / "runs" / chunk_name
        generated_data = json.loads((chunk_result_dir / "metrics.json").read_text(encoding="utf-8"))
        logs.append(f"chunk {chunk_index}: lines {start_line}-{end_line - 1}, {start_ms}-{end_ms} ms, {generated_data['grade']}")
        generated_lrc_words = load_words_from_json(chunk_result_dir / "qwen.json", offset_ms=start_ms)
        generated = generate_lrc_from_words(chunk_lines, generated_lrc_words)
        for line in generated.lines:
            merged_lines.append(
                LineAlignment(
                    index=start_line + line.index,
                    text=line.text,
                    start_ms=line.start_ms,
                    end_ms=line.end_ms,
                    matched_words=line.matched_words,
                    confidence=line.confidence,
                    interpolated=line.interpolated,
                )
            )

    if not merged_lines:
        return None

    merged_lines = sorted(merged_lines, key=lambda line: line.index)
    lrc = "\n".join(f"{format_lrc_timestamp(line.start_ms)}{line.text}" for line in merged_lines)
    matched_lines = sum(1 for line in merged_lines if not line.interpolated)
    similarity_count = sum(line.matched_words for line in merged_lines)
    total_similarity = sum(line.confidence * line.matched_words for line in merged_lines)
    metrics = score_alignment(merged_lines, matched_lines, total_similarity, similarity_count)
    metrics["runtimeSeconds"] = round(total_runtime, 3)
    metrics["mode"] = "chunked_anchor_forced_align"
    write_text(run_dir / "generated.lrc", lrc)
    write_text(run_dir / "metrics.json", json.dumps(metrics, indent=2, ensure_ascii=False))
    write_text(run_dir / "chunk-plan.txt", "\n".join(logs))
    return {
        "name": "chunked_anchor_forced_align",
        "mode": "chunked_anchor_forced_align",
        "runtimeSeconds": total_runtime,
        "metrics": metrics,
        "lrcPath": str((run_dir / "generated.lrc").resolve()),
        "metricsPath": str((run_dir / "metrics.json").resolve()),
        "jsonPath": "",
    }


def build_anchor_chunks(
    lines: list[str],
    approximate_lines: list[LineAlignment],
    max_lines: int = 8,
    pad_ms: int = 2_500,
) -> list[tuple[int, int, int, int]]:
    counts = normalized_line_counts(lines)
    anchors = [
        line
        for line in approximate_lines
        if not line.interpolated
        and line.matched_words >= 3
        and line.confidence >= 0.72
        and line.start_ms > 0
        and counts.get(normalize_line(line.text), 0) == 1
    ]
    if len(anchors) < 2:
        return []

    chunks: list[tuple[int, int, int, int]] = []
    boundaries = [0] + [anchor.index for anchor in anchors] + [len(lines)]
    times = [0] + [anchor.start_ms for anchor in anchors] + [max(line.start_ms for line in approximate_lines)]
    for index in range(len(boundaries) - 1):
        start_line = boundaries[index]
        end_line = max(start_line + 1, boundaries[index + 1])
        while end_line - start_line > max_lines:
            split_line = start_line + max_lines
            chunks.append((max(0, times[index] - pad_ms), times[index + 1] + pad_ms, start_line, split_line))
            start_line = split_line
        chunks.append((max(0, times[index] - pad_ms), times[index + 1] + pad_ms, start_line, end_line))
    return dedupe_chunks(chunks)


def build_fixed_chunks_from_approximation(
    lines: list[str],
    approximate_lines: list[LineAlignment],
    max_lines: int,
    pad_ms: int,
) -> list[tuple[int, int, int, int]]:
    chunks: list[tuple[int, int, int, int]] = []
    for start_line in range(0, len(lines), max_lines):
        end_line = min(len(lines), start_line + max_lines)
        timed = [line for line in approximate_lines[start_line:end_line] if line.start_ms >= 0]
        if not timed:
            continue
        start_ms = max(0, min(line.start_ms for line in timed) - pad_ms)
        end_ms = max(line.end_ms or line.start_ms for line in timed) + 12_000
        chunks.append((start_ms, end_ms, start_line, end_line))
    return dedupe_chunks(chunks)


def dedupe_chunks(chunks: list[tuple[int, int, int, int]]) -> list[tuple[int, int, int, int]]:
    seen: set[tuple[int, int]] = set()
    output: list[tuple[int, int, int, int]] = []
    for chunk in chunks:
        key = (chunk[2], chunk[3])
        if key in seen:
            continue
        seen.add(key)
        output.append(chunk)
    return output


def rank_results(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return sorted(
        results,
        key=lambda result: (
            result["metrics"].get("impossibleClusterCount", math.inf),
            -result["metrics"].get("matchedLineRatio", 0.0),
            result["metrics"].get("interpolatedLineRatio", 1.0),
            -result["metrics"].get("averageWordSimilarity", 0.0),
            result.get("runtimeSeconds", math.inf),
        ),
    )


def collect_existing_results(workdir: Path) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for metrics_path in sorted((workdir / "runs").glob("*/metrics.json")):
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
                "jsonPath": str((run_dir / "qwen.json").resolve()) if (run_dir / "qwen.json").exists() else "",
            }
        )
    return results


def filter_results_for_current_anchor_rules(results: list[dict[str, Any]], anchors: list[str]) -> list[dict[str, Any]]:
    if len(anchors) >= 2:
        return results
    return [result for result in results if result["name"] != "chunked_anchor_forced_align"]


def find_existing_transcription_json(workdir: Path, max_tokens: list[int]) -> Path | None:
    for token_count in sorted(max_tokens, reverse=True):
        candidate = workdir / "runs" / f"transcribe_align_{token_count}" / "qwen.json"
        if candidate.exists():
            return candidate
    return None


def write_report(workdir: Path, results: list[dict[str, Any]], track: dict[str, Any], anchors: list[str]) -> None:
    ranked = rank_results(results)
    lines = [
        "# Auto-Sync Evaluation Report",
        "",
        f"Track: `{track['title']} - {track.get('artist') or ''}`",
        f"Audio: `{track['file_path']}`",
        "",
        "## Ranked Results",
        "",
        "| Rank | Run | Grade | Clusters | Matched | Interpolated | Similarity | Runtime | LRC |",
        "| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for rank, result in enumerate(ranked, start=1):
        metrics = result["metrics"]
        lines.append(
            f"| {rank} | `{result['name']}` | {metrics.get('grade')} | "
            f"{metrics.get('impossibleClusterCount')} | "
            f"{metrics.get('matchedLineRatio', 0):.3f} | "
            f"{metrics.get('interpolatedLineRatio', 0):.3f} | "
            f"{metrics.get('averageWordSimilarity', 0):.3f} | "
            f"{result.get('runtimeSeconds', 0):.1f}s | `{result['lrcPath']}` |"
        )
    lines.extend(["", "## Anchor Candidates", ""])
    lines.extend(f"- {anchor}" for anchor in anchors)
    lines.extend(["", "## Interpretation Notes", ""])
    lines.append("- Runs with impossible timestamp clusters are ranked below cluster-free runs even when word similarity is high.")
    lines.append("- A high interpolated ratio means the model did not directly place most source lines.")
    lines.append("- Anchor candidates are cleaned lyric lines whose normalized text appears exactly once in the source lyrics.")
    if len(anchors) < 2:
        lines.append("- This track has fewer than two unique anchor candidates, so anchor-based chunking is not a valid segmentation strategy for it.")
        lines.append("- Any older `chunked_anchor_forced_align` artifacts are excluded from this report because they were produced without valid anchors.")
    lines.append("- `chunked_anchor_forced_align` is slower and experimental; it is useful only when the source has enough unique anchor lines to split sections.")
    lines.append("- `first_occurrence_search` scans the ASR word stream from the beginning for every lyric line independently; repeated lines can intentionally collapse to the same first timestamp.")
    write_text(workdir / "report.md", "\n".join(lines) + "\n")
    write_text(workdir / "summary.json", json.dumps(ranked, indent=2, ensure_ascii=False))


def validate_paths(audio: Path, db: Path, qwen: dict[str, Path]) -> None:
    missing = [path for path in [audio, db, qwen["exe"], qwen["asr"], qwen["aligner"]] if not path.exists()]
    if missing:
        raise FileNotFoundError("Missing required paths:\n" + "\n".join(str(path) for path in missing))


def run_pipeline(args: argparse.Namespace) -> Path:
    workdir = args.workdir.resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    qwen = qwen_paths(args.qwen_dir)
    if args.report_only:
        if not args.db.exists():
            raise FileNotFoundError(f"Missing database: {args.db}")
    else:
        validate_paths(args.audio, args.db, qwen)

    track = load_track_from_db(args.db, args.track_id)
    plain = extract_plain_lyrics(track.get("lyricsfile") or "")
    original_lines = non_empty_lines(plain)
    clean_lines = clean_lyrics_lines(plain)
    anchors = select_anchor_lines(clean_lines)

    write_text(workdir / "lyrics-original.txt", "\n".join(original_lines) + "\n")
    write_text(workdir / "lyrics-clean.txt", "\n".join(clean_lines) + "\n")
    write_text(workdir / "lyrics-anchors.txt", "\n".join(anchors) + "\n")
    write_text(workdir / "track.json", json.dumps({k: v for k, v in track.items() if k != "lyricsfile"}, indent=2))

    results: list[dict[str, Any]] = []
    errors: list[str] = []

    if args.report_only:
        if not args.skip_first_occurrence:
            transcription_json = find_existing_transcription_json(workdir, args.max_tokens)
            if transcription_json is not None:
                write_first_occurrence_search_result(
                    "first_occurrence_search",
                    clean_lines,
                    load_words_from_json(transcription_json),
                    workdir,
                    transcription_json,
                )
        results = filter_results_for_current_anchor_rules(collect_existing_results(workdir), anchors)
        write_report(workdir, results, track, anchors)
        return workdir / "report.md"

    audio = prepare_audio(args.audio, workdir, args.timeout)

    def record(name: str, fn) -> dict[str, Any] | None:
        print(f"Running {name}...", flush=True)
        try:
            result = fn()
            results.append(result)
            print(f"Finished {name}: {result['metrics']['grade']}", flush=True)
            return result
        except Exception as error:  # noqa: BLE001 - CLI experiment should continue across failed runs.
            message = f"{name}: {error}"
            errors.append(message)
            write_text(workdir / "errors.log", "\n".join(errors) + "\n")
            print(f"Failed {message}", file=sys.stderr, flush=True)
            return None

    if not args.skip_qwen:
        record(
            "full_forced_original",
            lambda: run_qwen_forced_align("full_forced_original", qwen, audio["raw"], original_lines, workdir, args.timeout),
        )
        record(
            "full_forced_clean",
            lambda: run_qwen_forced_align("full_forced_clean", qwen, audio["raw"], clean_lines, workdir, args.timeout),
        )
        transcription_results: dict[int, dict[str, Any]] = {}
        for max_tokens in args.max_tokens:
            result = record(
                f"transcribe_align_{max_tokens}",
                lambda max_tokens=max_tokens: run_qwen_transcribe_align(
                    f"transcribe_align_{max_tokens}",
                    qwen,
                    audio["raw"],
                    clean_lines,
                    workdir,
                    args.timeout,
                    max_tokens,
                ),
            )
            if result is not None:
                transcription_results[max_tokens] = result
        if not args.skip_first_occurrence and transcription_results:
            source_result = transcription_results.get(max(args.max_tokens)) or next(iter(transcription_results.values()))
            source_json = Path(source_result["jsonPath"])
            record(
                "first_occurrence_search",
                lambda source_json=source_json: write_first_occurrence_search_result(
                    "first_occurrence_search",
                    clean_lines,
                    load_words_from_json(source_json),
                    workdir,
                    source_json,
                ),
            )
        record(
            "full_forced_clean_loudnorm",
            lambda: run_qwen_forced_align(
                "full_forced_clean_loudnorm", qwen, audio["loudnorm"], clean_lines, workdir, args.timeout
            ),
        )
        if not args.skip_chunks and transcription_results:
            chunk_source = transcription_results.get(max(args.max_tokens)) or next(iter(transcription_results.values()))
            record(
                "chunked_anchor_forced_align",
                lambda: run_chunked_alignment(qwen, audio["raw"], clean_lines, chunk_source, workdir, args.timeout)
                or (_raise("Chunked alignment did not produce any chunks")),
            )

    write_report(workdir, filter_results_for_current_anchor_rules(results, anchors), track, anchors)
    return workdir / "report.md"


def _raise(message: str) -> None:
    raise RuntimeError(message)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--track-id", type=int, default=DEFAULT_TRACK_ID)
    parser.add_argument("--db", type=Path, default=default_db_path())
    parser.add_argument("--audio", type=Path, default=DEFAULT_AUDIO_PATH)
    parser.add_argument("--qwen-dir", type=Path, default=default_qwen_dir())
    parser.add_argument("--workdir", type=Path, default=DEFAULT_WORKDIR)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--max-tokens", type=int, nargs="+", default=[1024, 2048, 4096])
    parser.add_argument("--skip-qwen", action="store_true", help="Prepare inputs/report skeleton without running Qwen.")
    parser.add_argument("--skip-chunks", action="store_true", help="Skip experimental anchor/chunk alignment.")
    parser.add_argument("--skip-first-occurrence", action="store_true", help="Skip the first-occurrence ASR word search run.")
    parser.add_argument("--report-only", action="store_true", help="Regenerate report from existing run artifacts.")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    report = run_pipeline(args)
    print(f"Report written to {report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
