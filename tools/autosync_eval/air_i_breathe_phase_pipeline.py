#!/usr/bin/env python3
"""Phase-by-phase lyric alignment evaluator for Air I Breathe.

This is a scratch-only CLI pipeline. It reads the current LRCGET database for
the reference lyrics, but never writes to the database or calls app commands.
All artifacts are written below target/autosync-eval.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import time
import wave
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any

from air_i_breathe_eval import (
    DEFAULT_AUDIO_PATH,
    DEFAULT_TRACK_ID,
    clean_lyrics_lines,
    default_db_path,
    extract_plain_lyrics,
    format_lrc_timestamp,
    load_track_from_db,
    normalize_compact,
    normalize_line,
    tokenize,
    word_similarity,
)


DEFAULT_WORKDIR = Path("target") / "autosync-eval" / "air-i-breathe" / "phased"
DEFAULT_ALIGN_ENV = Path("target") / "autosync-eval" / "asr-align-venv"
DEFAULT_MODEL = "large-v3-turbo"
DEFAULT_LANGUAGE = "en"
DEFAULT_TIMEOUT_SECONDS = 60 * 60
FIRST_EXPECTED_MS = 24_000
SECOND_EXPECTED_MS = 28_000
SANITY_TOLERANCE_MS = 8_000
JUNK_COMPACT_LINES = {
    "lyricsvideoslisten",
    "lyricsvideo",
    "lyricsvideos",
    "listen",
}
FILLER_TOKENS = {
    "oh",
    "ooh",
    "oooh",
    "ah",
    "aah",
    "ahh",
    "uh",
    "huh",
    "hmm",
    "mmm",
    "whoa",
    "woah",
    "yeah",
    "yea",
    "hey",
}


@dataclass(frozen=True)
class ReferenceLine:
    id: str
    index: int
    text: str
    normalized: str


@dataclass(frozen=True)
class AsrSegment:
    text: str
    start_ms: int
    end_ms: int
    avg_logprob: float | None = None
    no_speech_prob: float | None = None
    compression_ratio: float | None = None
    source: str = "asr"


@dataclass(frozen=True)
class LineMatch:
    line: ReferenceLine
    start_ms: int
    end_ms: int
    asr_text: str
    confidence: float
    source: str
    status: str

    @property
    def line_id(self) -> str:
        return self.line.id

    @property
    def line_index(self) -> int:
        return self.line.index

    @property
    def text(self) -> str:
        return self.line.text


@dataclass(frozen=True)
class WindowSpec:
    id: str
    start_ms: int
    end_ms: int
    line_ids: list[str]


@dataclass(frozen=True)
class Candidate:
    name: str
    matches: list[LineMatch]
    lrc_path: Path
    metrics_path: Path


class PhaseWriter:
    def __init__(self, root: Path, name: str):
        self.root = root
        self.name = name
        self.dir = root / name
        self.dir.mkdir(parents=True, exist_ok=True)
        self.started_at = utc_now()
        self.status: dict[str, Any] = {
            "phase": name,
            "startedAt": self.started_at,
            "status": "running",
            "inputs": {},
            "outputs": {},
            "command": None,
            "error": None,
        }
        self.write_status()

    def set_inputs(self, inputs: dict[str, Any]) -> None:
        self.status["inputs"] = stringify_paths(inputs)
        self.write_status()

    def set_command(self, command: list[str] | None) -> None:
        self.status["command"] = command
        self.write_status()

    def succeed(self, outputs: dict[str, Any]) -> None:
        self.status["status"] = "succeeded"
        self.status["outputs"] = stringify_paths(outputs)
        self.status["finishedAt"] = utc_now()
        self.write_status()

    def partial(self, outputs: dict[str, Any], error: str) -> None:
        self.status["status"] = "partial"
        self.status["outputs"] = stringify_paths(outputs)
        self.status["error"] = error
        self.status["finishedAt"] = utc_now()
        self.write_status()

    def fail(self, error: BaseException | str) -> None:
        self.status["status"] = "failed"
        self.status["error"] = str(error)
        self.status["finishedAt"] = utc_now()
        self.write_status()

    def write_status(self) -> None:
        write_json(self.dir / "status.json", self.status)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def stringify_paths(value: Any) -> Any:
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, dict):
        return {key: stringify_paths(item) for key, item in value.items()}
    if isinstance(value, list):
        return [stringify_paths(item) for item in value]
    return value


def write_text(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")
    return path


def write_json(path: Path, value: Any) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(stringify_paths(value), ensure_ascii=False, indent=2), encoding="utf-8", newline="\n")
    return path


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def build_reference_lines(raw_lines: list[str] | str) -> tuple[list[ReferenceLine], list[dict[str, Any]]]:
    if isinstance(raw_lines, str):
        lines = raw_lines.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    else:
        lines = raw_lines

    reference: list[ReferenceLine] = []
    removals: list[dict[str, Any]] = []
    for source_index, raw_line in enumerate(lines, start=1):
        text = raw_line.strip()
        if not text:
            removals.append({"sourceLine": source_index, "text": raw_line, "reason": "blank"})
            continue
        compact = normalize_compact(text)
        if compact in JUNK_COMPACT_LINES:
            removals.append({"sourceLine": source_index, "text": raw_line, "reason": "junk"})
            continue
        reference.append(
            ReferenceLine(
                id=f"L{len(reference) + 1:03d}",
                index=len(reference),
                text=text,
                normalized=normalize_line(text),
            )
        )
    return reference, removals


def is_weak_echo_segment(segment: AsrSegment) -> bool:
    tokens = tokenize(segment.text)
    if not tokens:
        return True
    unique_tokens = set(tokens)
    if unique_tokens <= FILLER_TOKENS:
        return True
    if len(tokens) == 1 and tokens[0] in FILLER_TOKENS:
        return True
    if len(unique_tokens) == 1 and len(tokens) >= 3:
        return True
    if (segment.compression_ratio or 0.0) >= 2.4 and repeated_ngram_ratio(tokens) >= 0.55:
        return True
    return False


def repeated_ngram_ratio(tokens: list[str]) -> float:
    if len(tokens) < 4:
        return 0.0
    bigrams = list(zip(tokens, tokens[1:]))
    if not bigrams:
        return 0.0
    return 1.0 - (len(set(bigrams)) / len(bigrams))


def match_occurrences(
    reference: list[ReferenceLine],
    segments: list[AsrSegment],
    min_similarity: float = 0.58,
) -> list[LineMatch]:
    matched: list[LineMatch] = []
    segment_cursor = 0
    for line in reference:
        best: tuple[int, float] | None = None
        for segment_index in range(segment_cursor, len(segments)):
            segment = segments[segment_index]
            if is_weak_echo_segment(segment):
                continue
            similarity = line_similarity(line.normalized, segment.text)
            if similarity < min_similarity:
                continue
            distance_penalty = min(0.30, max(0, segment_index - segment_cursor) * 0.012)
            score = similarity - distance_penalty
            if best is None or score > best[1]:
                best = (segment_index, score)
            if best and segment_index - segment_cursor > 48:
                break
        if best is None:
            continue
        segment_index, score = best
        segment = segments[segment_index]
        segment_cursor = segment_index + 1
        matched.append(
            LineMatch(
                line=line,
                start_ms=segment.start_ms,
                end_ms=max(segment.end_ms, segment.start_ms + 200),
                asr_text=segment.text,
                confidence=max(0.0, min(1.0, score)),
                source=segment.source,
                status="matched",
            )
        )
    return matched


def collect_skipped_segments(segments: list[AsrSegment], matches: list[LineMatch]) -> list[AsrSegment]:
    matched_ranges = {(match.start_ms, match.end_ms, normalize_line(match.asr_text)) for match in matches}
    return [
        segment
        for segment in segments
        if is_weak_echo_segment(segment)
        and (segment.start_ms, segment.end_ms, normalize_line(segment.text)) not in matched_ranges
    ]


def line_similarity(normalized_target: str, candidate_text: str) -> float:
    candidate = normalize_line(candidate_text)
    if not normalized_target or not candidate:
        return 0.0
    if normalized_target == candidate:
        return 1.0
    target_tokens = normalized_target.split()
    candidate_tokens = candidate.split()
    token_scores = [
        max((word_similarity(token, candidate_token) for candidate_token in candidate_tokens), default=0.0)
        for token in target_tokens
    ]
    token_similarity = sum(token_scores) / len(token_scores)
    length_ratio = min(len(target_tokens), len(candidate_tokens)) / max(len(target_tokens), len(candidate_tokens))
    sequence_similarity = SequenceMatcher(None, normalized_target, candidate).ratio()
    return token_similarity * 0.62 + length_ratio * 0.18 + sequence_similarity * 0.20


def detect_drift(
    matches: list[LineMatch],
    max_gap_ms: int = 45_000,
    min_prior_matches: int = 3,
) -> dict[str, Any]:
    sorted_matches = sorted(matches, key=lambda match: match.line.index)
    for index in range(1, len(sorted_matches)):
        previous = sorted_matches[index - 1]
        current = sorted_matches[index]
        gap_ms = current.start_ms - previous.start_ms
        if index >= min_prior_matches and gap_ms > max_gap_ms:
            return {
                "firstDivergenceLineId": current.line.id,
                "previousLineId": previous.line.id,
                "gapMs": gap_ms,
                "atMs": current.start_ms,
                "previousMs": previous.start_ms,
                "reason": "large_monotonic_gap",
            }
    return {
        "firstDivergenceLineId": None,
        "previousLineId": None,
        "gapMs": 0,
        "atMs": None,
        "previousMs": None,
        "reason": "none",
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
            if first_cluster_ms is None:
                first_cluster_ms = current[0]
        current = [start]
    if len(current) >= min_size:
        clusters += 1
        if first_cluster_ms is None:
            first_cluster_ms = current[0]
    return clusters, first_cluster_ms


def write_lrc(reference: list[ReferenceLine], matches: list[LineMatch], path: Path, source: str) -> tuple[Path, list[LineMatch]]:
    complete = complete_matches(reference, matches, source)
    lines = [f"{format_lrc_timestamp(match.start_ms)}{match.text}" for match in complete]
    write_text(path, "\n".join(lines) + "\n")
    return path, complete


def complete_matches(reference: list[ReferenceLine], matches: list[LineMatch], source: str) -> list[LineMatch]:
    by_index = {match.line.index: match for match in matches}
    known = sorted(by_index)
    if not known:
        return [
            LineMatch(line=line, start_ms=line.index * 1500, end_ms=line.index * 1500 + 300, asr_text="", confidence=0.0, source=source, status="interpolated")
            for line in reference
        ]

    completed: list[LineMatch] = []
    for line in reference:
        existing = by_index.get(line.index)
        if existing is not None:
            completed.append(existing)
            continue
        previous_index = max((index for index in known if index < line.index), default=None)
        next_index = min((index for index in known if index > line.index), default=None)
        if previous_index is None and next_index is None:
            start_ms = line.index * 1500
        elif previous_index is None:
            next_match = by_index[next_index]
            start_ms = max(0, next_match.start_ms - (next_index - line.index) * 1500)
        elif next_index is None:
            previous_match = by_index[previous_index]
            start_ms = previous_match.start_ms + (line.index - previous_index) * 1500
        else:
            previous_match = by_index[previous_index]
            next_match = by_index[next_index]
            span = next_match.start_ms - previous_match.start_ms
            step = span / max(1, next_index - previous_index)
            start_ms = int(round(previous_match.start_ms + step * (line.index - previous_index)))
        completed.append(
            LineMatch(line=line, start_ms=start_ms, end_ms=start_ms + 300, asr_text="", confidence=0.0, source=source, status="interpolated")
        )

    completed = sorted(completed, key=lambda match: match.line.index)
    repaired: list[LineMatch] = []
    previous_start = -1
    for match in completed:
        start_ms = max(match.start_ms, previous_start + 20)
        previous_start = start_ms
        repaired.append(
            LineMatch(
                line=match.line,
                start_ms=start_ms,
                end_ms=max(match.end_ms, start_ms + 200),
                asr_text=match.asr_text,
                confidence=match.confidence,
                source=match.source,
                status=match.status,
            )
        )
    return repaired


def score_candidate(reference: list[ReferenceLine], matches: list[LineMatch], skipped_echo_count: int) -> dict[str, Any]:
    complete = complete_matches(reference, matches, "score")
    matched_count = sum(1 for match in complete if match.status == "matched")
    interpolated_count = len(complete) - matched_count
    starts = [match.start_ms for match in complete]
    clusters, first_cluster_ms = impossible_timestamp_clusters(starts)
    first_start = complete[0].start_ms if complete else None
    second_start = complete[1].start_ms if len(complete) > 1 else None
    first_delta = abs(first_start - FIRST_EXPECTED_MS) if first_start is not None else None
    second_delta = abs(second_start - SECOND_EXPECTED_MS) if second_start is not None else None
    drift = detect_drift([match for match in complete if match.status == "matched"])
    matched_ratio = matched_count / max(1, len(reference))
    interpolated_ratio = interpolated_count / max(1, len(reference))
    confidence = sum(match.confidence for match in complete if match.status == "matched") / max(1, matched_count)
    sanity = (
        first_delta is not None
        and second_delta is not None
        and first_delta <= SANITY_TOLERANCE_MS
        and second_delta <= SANITY_TOLERANCE_MS
    )
    grade = "good"
    if not sanity or clusters or drift["firstDivergenceLineId"] or matched_ratio < 0.65:
        grade = "bad"
    elif interpolated_ratio > 0.25 or confidence < 0.78:
        grade = "repairable"
    return {
        "lineCount": len(reference),
        "matchedLineCount": matched_count,
        "matchedCanonicalRatio": matched_ratio,
        "interpolatedLineCount": interpolated_count,
        "interpolatedLineRatio": interpolated_ratio,
        "averageConfidence": confidence,
        "skippedEchoAdlibCount": skipped_echo_count,
        "firstTimestampMs": first_start,
        "secondTimestampMs": second_start,
        "firstLineDeltaMs": first_delta,
        "secondLineDeltaMs": second_delta,
        "firstTwoLineSanityPass": sanity,
        "impossibleClusterCount": clusters,
        "firstBadClusterMs": first_cluster_ms,
        "drift": drift,
        "grade": grade,
    }


def create_candidate(
    name: str,
    reference: list[ReferenceLine],
    matches: list[LineMatch],
    skipped_echo_count: int,
    out_dir: Path,
) -> Candidate:
    candidate_dir = out_dir / name
    candidate_dir.mkdir(parents=True, exist_ok=True)
    lrc_path, complete = write_lrc(reference, matches, candidate_dir / f"{name}.lrc", name)
    metrics = score_candidate(reference, matches, skipped_echo_count)
    write_json(candidate_dir / "line-matches.json", [line_match_to_json(match) for match in complete])
    metrics_path = write_json(candidate_dir / "metrics.json", metrics)
    return Candidate(name=name, matches=complete, lrc_path=lrc_path, metrics_path=metrics_path)


def line_match_to_json(match: LineMatch) -> dict[str, Any]:
    return {
        "lineId": match.line.id,
        "lineIndex": match.line.index,
        "text": match.line.text,
        "startMs": match.start_ms,
        "endMs": match.end_ms,
        "asrText": match.asr_text,
        "confidence": match.confidence,
        "source": match.source,
        "status": match.status,
    }


def ensure_scratch_env(env_dir: Path, phase: PhaseWriter, timeout: int, require_cuda: bool) -> Path:
    python_path = env_dir / "Scripts" / "python.exe"
    required_modules = ["torch", "torchaudio", "demucs", "faster_whisper", "stable_whisper", "whisperx", "transformers", "nltk", "rapidfuzz", "librosa", "soundfile", "pandas"]
    phase.set_inputs({"envDir": env_dir, "requiredModules": required_modules})
    if not python_path.exists():
        command = ["uv", "venv", "--python", "3.11", str(env_dir)]
        phase.set_command(command)
        run_logged(command, phase.dir / "uv-venv.log", timeout)

    check = [str(python_path), "-c", module_check_script(required_modules, require_cuda)]
    if run_quiet(check):
        phase.succeed({"python": python_path, "installed": False})
        return python_path

    torch_command = [
        "uv",
        "pip",
        "install",
        "--python",
        str(python_path),
        "--index-url",
        "https://download.pytorch.org/whl/cu121",
        "torch==2.5.1+cu121",
        "torchaudio==2.5.1+cu121",
    ]
    phase.set_command(torch_command)
    run_logged(torch_command, phase.dir / "uv-pip-install-torch-cu121.log", timeout)

    # WhisperX currently pulls pyannote-audio -> lightning, and uv cannot resolve
    # a compatible lightning candidate on this Windows setup. Install the rest of
    # the stack normally, then add WhisperX/pyannote without their optional deps;
    # the CLI and alignment path load with the already-installed torch/faster-whisper stack.
    core_packages = ["demucs", "faster-whisper", "stable-ts", "transformers", "nltk", "rapidfuzz", "librosa", "soundfile", "pandas"]
    command = ["uv", "pip", "install", "--python", str(python_path), *core_packages]
    phase.set_command(command)
    run_logged(command, phase.dir / "uv-pip-install.log", timeout)
    whisperx_command = [
        "uv",
        "pip",
        "install",
        "--python",
        str(python_path),
        "whisperx==3.8.5",
        "pyannote-audio==4.0.4",
        "--no-deps",
    ]
    phase.set_command(whisperx_command)
    run_logged(whisperx_command, phase.dir / "uv-pip-install-whisperx.log", timeout)
    run_logged([str(python_path), "-c", module_check_script(required_modules, require_cuda)], phase.dir / "module-check.log", timeout)
    phase.succeed({"python": python_path, "installed": True, "packages": ["torch==2.5.1+cu121", "torchaudio==2.5.1+cu121", *core_packages, "whisperx==3.8.5", "pyannote-audio==4.0.4 --no-deps"]})
    return python_path


def module_check_script(required_modules: list[str], require_cuda: bool) -> str:
    return (
        "import importlib, json, torch\n"
        f"mods={required_modules!r}\n"
        f"require_cuda={require_cuda!r}\n"
        "missing=[]\n"
        "for mod in mods:\n"
        "    try: importlib.import_module(mod)\n"
        "    except Exception as exc: missing.append((mod, str(exc)))\n"
        "if require_cuda and not torch.cuda.is_available(): missing.append(('torch.cuda', 'CUDA is not available'))\n"
        "if missing: raise SystemExit(json.dumps({'missing': missing}))\n"
        "print(json.dumps({'cuda': torch.cuda.is_available(), 'torch': torch.__version__}))\n"
    )


def run_quiet(command: list[str]) -> bool:
    try:
        return subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True, timeout=120).returncode == 0
    except Exception:
        return False


def run_logged(command: list[str], log_path: Path, timeout: int, cwd: Path | None = None) -> float:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    started = time.perf_counter()
    with log_path.open("w", encoding="utf-8", newline="\n") as log:
        log.write("$ " + command_for_log(command) + "\n\n")
        process = subprocess.run(
            command,
            cwd=str(cwd) if cwd else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
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


def command_for_log(command: list[str]) -> str:
    return " ".join(f'"{part}"' if " " in part else part for part in command)


def phase_manifest(args: argparse.Namespace, run_dir: Path, align_python: Path | None) -> dict[str, Any]:
    phase = PhaseWriter(run_dir, "00_manifest")
    try:
        ffprobe_json = phase.dir / "ffprobe.json"
        ffprobe_command = ["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", str(args.audio)]
        phase.set_inputs({"audio": args.audio, "db": args.db, "trackId": args.track_id})
        phase.set_command(ffprobe_command)
        run_capture_json(ffprobe_command, ffprobe_json, phase.dir / "ffprobe.log", args.timeout)
        tools = collect_tool_versions(align_python)
        manifest = {
            "runId": args.run_id,
            "audio": str(args.audio),
            "audioSha256": sha256_file(args.audio),
            "db": str(args.db),
            "trackId": args.track_id,
            "device": args.device,
            "model": args.model,
            "language": args.language,
            "createdAt": utc_now(),
            "tools": tools,
        }
        write_json(phase.dir / "manifest.json", manifest)
        phase.succeed({"manifest": phase.dir / "manifest.json", "ffprobe": ffprobe_json})
        return manifest
    except Exception as error:
        phase.fail(error)
        raise


def run_capture_json(command: list[str], output_path: Path, log_path: Path, timeout: int) -> None:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    process = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout)
    log_path.write_text(process.stderr, encoding="utf-8", newline="\n")
    if process.returncode != 0:
        raise RuntimeError(f"Command failed ({process.returncode}); see {log_path}")
    output_path.write_text(process.stdout, encoding="utf-8", newline="\n")


def collect_tool_versions(align_python: Path | None) -> dict[str, Any]:
    versions: dict[str, Any] = {"python": sys.version}
    for command_name in ["ffmpeg", "ffprobe", "uv"]:
        try:
            output = subprocess.run([command_name, "-version"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=30)
            versions[command_name] = output.stdout.splitlines()[0] if output.stdout else None
        except Exception as error:
            versions[command_name] = str(error)
    if align_python and align_python.exists():
        script = (
            "import importlib.metadata as m, json\n"
            "pkgs=['demucs','faster-whisper','stable-ts','whisperx','rapidfuzz','librosa','soundfile','pandas','torch']\n"
            "print(json.dumps({p: (m.version(p) if p in {d.metadata['Name'].lower(): d for d in m.distributions()} else None) for p in pkgs}))\n"
        )
        try:
            output = subprocess.run([str(align_python), "-c", script], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=60)
            versions["scratchPythonPackages"] = json.loads(output.stdout) if output.returncode == 0 and output.stdout else output.stderr
        except Exception as error:
            versions["scratchPythonPackages"] = str(error)
    return versions


def phase_reference(args: argparse.Namespace, run_dir: Path) -> tuple[dict[str, Any], list[ReferenceLine]]:
    phase = PhaseWriter(run_dir, "01_reference")
    try:
        track = load_track_from_db(args.db, args.track_id)
        plain = extract_plain_lyrics(track.get("lyricsfile") or "")
        original_lines = plain.replace("\r\n", "\n").replace("\r", "\n").split("\n")
        reference, removals = build_reference_lines(original_lines)
        phase.set_inputs({"db": args.db, "trackId": args.track_id})
        write_text(phase.dir / "lyrics-original.txt", plain + "\n")
        write_text(phase.dir / "lyrics-clean.txt", "\n".join(line.text for line in reference) + "\n")
        write_json(phase.dir / "lyrics-indexed.json", [asdict(line) for line in reference])
        write_json(phase.dir / "cleanup-log.json", removals)
        track_json = {key: value for key, value in track.items() if key != "lyricsfile"}
        write_json(phase.dir / "track.json", track_json)
        phase.succeed(
            {
                "originalLyrics": phase.dir / "lyrics-original.txt",
                "cleanLyrics": phase.dir / "lyrics-clean.txt",
                "indexedLyrics": phase.dir / "lyrics-indexed.json",
                "cleanupLog": phase.dir / "cleanup-log.json",
                "track": phase.dir / "track.json",
            }
        )
        return track, reference
    except Exception as error:
        phase.fail(error)
        raise


def phase_audio(args: argparse.Namespace, run_dir: Path) -> dict[str, Path]:
    phase = PhaseWriter(run_dir, "02_audio")
    try:
        phase.set_inputs({"audio": args.audio})
        original_copy = phase.dir / "audio.original.flac"
        if not original_copy.exists() or sha256_file(original_copy) != sha256_file(args.audio):
            shutil.copyfile(args.audio, original_copy)
        raw_wav = phase.dir / "audio.original.16k.mono.wav"
        loudnorm = phase.dir / "audio.original.loudnorm.wav"
        loudnorm_16k = phase.dir / "audio.original.loudnorm.16k.mono.wav"
        commands = [
            ["ffmpeg", "-y", "-hide_banner", "-i", str(args.audio), "-vn", "-ac", "1", "-ar", "16000", str(raw_wav)],
            ["ffmpeg", "-y", "-hide_banner", "-i", str(args.audio), "-vn", "-af", "loudnorm=I=-16:TP=-1.5:LRA=11", str(loudnorm)],
            ["ffmpeg", "-y", "-hide_banner", "-i", str(loudnorm), "-vn", "-ac", "1", "-ar", "16000", str(loudnorm_16k)],
        ]
        for index, command in enumerate(commands, start=1):
            target = Path(command[-1])
            if target.exists():
                continue
            run_logged(command, phase.dir / f"ffmpeg-{index}.log", args.timeout)
        metadata: dict[str, Any] = {}
        for path in [original_copy, raw_wav, loudnorm, loudnorm_16k]:
            out = phase.dir / f"{path.stem}.ffprobe.json"
            run_capture_json(["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", str(path)], out, out.with_suffix(".log"), args.timeout)
            metadata[path.name] = read_json(out)
        write_json(phase.dir / "audio-metadata.json", metadata)
        outputs = {
            "original_flac": original_copy,
            "original_16k": raw_wav,
            "original_loudnorm": loudnorm,
            "original_loudnorm_16k": loudnorm_16k,
        }
        phase.succeed(outputs)
        return outputs
    except Exception as error:
        phase.fail(error)
        raise


def phase_vocals(args: argparse.Namespace, run_dir: Path, align_python: Path, audio_path: Path) -> dict[str, Path]:
    phase = PhaseWriter(run_dir, "03_vocals")
    if args.skip_demucs:
        phase.succeed({"skipped": True})
        return {}
    try:
        phase.set_inputs({"audio": audio_path, "models": ["htdemucs", "htdemucs_ft"]})
        stems: dict[str, Path] = {}
        errors: list[str] = []
        for model in ["htdemucs", "htdemucs_ft"]:
            command = [
                str(align_python),
                "-m",
                "demucs.separate",
                "--two-stems=vocals",
                "-n",
                model,
                "-o",
                str(phase.dir / "stems"),
                str(audio_path),
            ]
            try:
                run_logged(command, phase.dir / f"demucs-{model}.log", args.timeout)
                expected = phase.dir / "stems" / model / audio_path.stem / "vocals.wav"
                if not expected.exists():
                    raise RuntimeError(f"Missing expected stem {expected}")
                stems[f"demucs_{model}_vocals"] = expected
            except Exception as error:
                errors.append(f"{model}: {error}")
        if errors and not stems:
            raise RuntimeError("; ".join(errors))
        if errors:
            phase.partial(stems, "; ".join(errors))
        else:
            phase.succeed(stems)
        return stems
    except Exception as error:
        phase.fail(error)
        raise


def phase_rough_asr(
    args: argparse.Namespace,
    run_dir: Path,
    align_python: Path,
    audio_outputs: dict[str, Path],
    stems: dict[str, Path],
) -> dict[str, list[AsrSegment]]:
    phase = PhaseWriter(run_dir, "04_rough_asr")
    try:
        variants: dict[str, Path] = {
            "original_flac": audio_outputs["original_flac"],
            "original_flac_loudnorm": audio_outputs["original_loudnorm"],
        }
        variants.update(stems)
        for name, path in list(stems.items()):
            loudnorm = phase.dir / "audio_variants" / f"{name}.loudnorm.wav"
            if not loudnorm.exists():
                loudnorm.parent.mkdir(parents=True, exist_ok=True)
                run_logged(
                    ["ffmpeg", "-y", "-hide_banner", "-i", str(path), "-vn", "-af", "loudnorm=I=-16:TP=-1.5:LRA=11", str(loudnorm)],
                    phase.dir / f"{name}.loudnorm.ffmpeg.log",
                    args.timeout,
                )
            variants[f"{name}_loudnorm"] = loudnorm

        phase.set_inputs({"variants": variants, "model": args.model, "language": args.language, "device": args.device})
        results: dict[str, list[AsrSegment]] = {}
        errors: list[str] = []
        for name, path in variants.items():
            try:
                result_path = run_faster_whisper(align_python, args, phase.dir, name, path)
                raw = read_json(result_path)
                segments = parse_segments(raw, source=name)
                results[name] = segments
                write_json(phase.dir / name / "segments-normalized.json", [asdict(segment) for segment in segments])
            except Exception as error:
                errors.append(f"{name}: {error}")
        if errors and not results:
            raise RuntimeError("; ".join(errors))
        if errors:
            phase.partial({"variants": list(results), "errors": errors}, "; ".join(errors))
        else:
            phase.succeed({"variants": list(results)})
        return results
    except Exception as error:
        phase.fail(error)
        raise


def run_faster_whisper(align_python: Path, args: argparse.Namespace, phase_dir: Path, name: str, audio_path: Path) -> Path:
    variant_dir = phase_dir / name
    variant_dir.mkdir(parents=True, exist_ok=True)
    output_json = variant_dir / "raw-asr.json"
    if output_json.exists():
        return output_json
    script = (
        "import json, sys\n"
        "from faster_whisper import WhisperModel\n"
        "model_name,audio_path,language,device,compute_type,output_path,use_vad=sys.argv[1:8]\n"
        "model=WhisperModel(model_name, device=device, compute_type=compute_type)\n"
        "segments,info=model.transcribe(audio_path, language=language, beam_size=5, word_timestamps=True, "
        "vad_filter=(use_vad == 'true'), condition_on_previous_text=False, temperature=0.0)\n"
        "payload={'language': getattr(info,'language',None), 'duration': getattr(info,'duration',None), 'vadFilter': use_vad == 'true', 'segments': []}\n"
        "for s in segments:\n"
        "    payload['segments'].append({\n"
        "      'id': getattr(s,'id',None), 'seek': getattr(s,'seek',None), 'start': s.start, 'end': s.end,\n"
        "      'text': s.text, 'avg_logprob': getattr(s,'avg_logprob',None),\n"
        "      'no_speech_prob': getattr(s,'no_speech_prob',None), 'compression_ratio': getattr(s,'compression_ratio',None),\n"
        "      'words': [{'word': w.word, 'start': w.start, 'end': w.end, 'probability': getattr(w,'probability',None)} for w in (s.words or [])]\n"
        "    })\n"
        "json.dump(payload, open(output_path,'w',encoding='utf-8'), ensure_ascii=False, indent=2)\n"
    )
    compute_type = "float16" if args.device == "cuda" else "int8"
    vad_command = [str(align_python), "-c", script, args.model, str(audio_path), args.language, args.device, compute_type, str(output_json), "true"]
    run_logged(vad_command, variant_dir / "faster-whisper.vad.log", args.timeout)
    raw = read_json(output_json)
    if raw.get("segments"):
        return output_json

    shutil.copyfile(output_json, variant_dir / "raw-asr.vad-empty.json")
    no_vad_command = [str(align_python), "-c", script, args.model, str(audio_path), args.language, args.device, compute_type, str(output_json), "false"]
    run_logged(no_vad_command, variant_dir / "faster-whisper.no-vad.log", args.timeout)
    return output_json


def parse_segments(raw: dict[str, Any], source: str, offset_ms: int = 0) -> list[AsrSegment]:
    segments: list[AsrSegment] = []
    for raw_segment in raw.get("segments", []):
        text = str(raw_segment.get("text") or "").strip()
        if not text:
            continue
        start_ms = int(round(float(raw_segment.get("start", 0.0)) * 1000)) + offset_ms
        end_ms = int(round(float(raw_segment.get("end", 0.0)) * 1000)) + offset_ms
        segments.append(
            AsrSegment(
                text=text,
                start_ms=start_ms,
                end_ms=max(end_ms, start_ms + 100),
                avg_logprob=as_optional_float(raw_segment.get("avg_logprob")),
                no_speech_prob=as_optional_float(raw_segment.get("no_speech_prob")),
                compression_ratio=as_optional_float(raw_segment.get("compression_ratio")),
                source=source,
            )
        )
    return segments


def as_optional_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        return float(value)
    except Exception:
        return None


def choose_best_rough_variant(reference: list[ReferenceLine], variants: dict[str, list[AsrSegment]]) -> tuple[str, list[AsrSegment], list[LineMatch]]:
    best: tuple[str, list[AsrSegment], list[LineMatch], dict[str, Any]] | None = None
    for name, segments in variants.items():
        matches = match_occurrences(reference, segments)
        skipped = collect_skipped_segments(segments, matches)
        metrics = score_candidate(reference, matches, len(skipped))
        key = (
            metrics["firstTwoLineSanityPass"],
            -metrics["impossibleClusterCount"],
            metrics["matchedCanonicalRatio"],
            metrics["averageConfidence"],
        )
        if best is None:
            best = (name, segments, matches, metrics)
            continue
        best_metrics = best[3]
        best_key = (
            best_metrics["firstTwoLineSanityPass"],
            -best_metrics["impossibleClusterCount"],
            best_metrics["matchedCanonicalRatio"],
            best_metrics["averageConfidence"],
        )
        if key > best_key:
            best = (name, segments, matches, metrics)
    if best is None:
        raise RuntimeError("No ASR variants produced usable segments")
    return best[0], best[1], best[2]


def phase_occurrence_match(
    args: argparse.Namespace,
    run_dir: Path,
    reference: list[ReferenceLine],
    variants: dict[str, list[AsrSegment]],
) -> tuple[str, list[AsrSegment], list[LineMatch], Candidate]:
    phase = PhaseWriter(run_dir, "05_occurrence_match")
    try:
        phase.set_inputs({"variants": list(variants), "lineCount": len(reference)})
        all_metrics: dict[str, Any] = {}
        best_name, best_segments, best_matches = choose_best_rough_variant(reference, variants)
        for name, segments in variants.items():
            matches = match_occurrences(reference, segments)
            skipped = collect_skipped_segments(segments, matches)
            candidate = create_candidate(f"{name}_rough_hybrid", reference, matches, len(skipped), phase.dir / "candidates")
            all_metrics[name] = read_json(candidate.metrics_path)
            write_json(phase.dir / name / "skipped-echo-segments.json", [asdict(segment) for segment in skipped])
        best_skipped = collect_skipped_segments(best_segments, best_matches)
        best_candidate = create_candidate("rough_asr_hybrid", reference, best_matches, len(best_skipped), phase.dir / "best")
        write_json(phase.dir / "variant-metrics.json", all_metrics)
        write_json(phase.dir / "best-variant.json", {"name": best_name, "candidate": best_candidate.lrc_path})
        phase.succeed({"bestVariant": best_name, "roughHybridLrc": best_candidate.lrc_path})
        return best_name, best_segments, best_matches, best_candidate
    except Exception as error:
        phase.fail(error)
        raise


def phase_windows(
    args: argparse.Namespace,
    run_dir: Path,
    reference: list[ReferenceLine],
    rough_matches: list[LineMatch],
    duration_ms: int,
) -> list[WindowSpec]:
    phase = PhaseWriter(run_dir, "06_windows")
    try:
        phase.set_inputs({"matchCount": len(rough_matches), "durationMs": duration_ms})
        windows = build_windows(reference, rough_matches, duration_ms)
        write_json(phase.dir / "windows.json", [asdict(window) for window in windows])
        for window in windows:
            window_ref = [line.text for line in reference if line.id in window.line_ids]
            write_text(phase.dir / window.id / "lyrics.txt", "\n".join(window_ref) + "\n")
            write_json(phase.dir / window.id / "window.json", asdict(window))
        phase.succeed({"windows": phase.dir / "windows.json", "count": len(windows)})
        return windows
    except Exception as error:
        phase.fail(error)
        raise


def build_windows(reference: list[ReferenceLine], rough_matches: list[LineMatch], duration_ms: int) -> list[WindowSpec]:
    matched = sorted(rough_matches, key=lambda match: match.line.index)
    if not matched:
        return [WindowSpec("window-001", 0, duration_ms, [line.id for line in reference])]

    windows: list[WindowSpec] = []
    for index, match in enumerate(matched):
        previous_match = matched[index - 1] if index > 0 else None
        next_match = matched[index + 1] if index + 1 < len(matched) else None
        start_ms = max(0, match.start_ms - 2500)
        end_ms = min(duration_ms, match.end_ms + 2500)
        if previous_match and match.start_ms - previous_match.start_ms <= 45_000:
            start_ms = max(0, (previous_match.start_ms + match.start_ms) // 2 - 1500)
        if next_match and next_match.start_ms - match.start_ms <= 45_000:
            end_ms = min(duration_ms, (match.start_ms + next_match.start_ms) // 2 + 2500)
        line_start = max(0, match.line.index - 1)
        line_end = min(len(reference), match.line.index + 2)
        line_ids = [line.id for line in reference[line_start:line_end]]
        windows.append(WindowSpec(f"window-{index + 1:03d}", start_ms, max(end_ms, start_ms + 1000), line_ids))
    return merge_near_duplicate_windows(windows)


def merge_near_duplicate_windows(windows: list[WindowSpec]) -> list[WindowSpec]:
    merged: list[WindowSpec] = []
    for window in windows:
        if merged and window.start_ms <= merged[-1].end_ms and window.end_ms - merged[-1].start_ms <= 45_000:
            previous = merged[-1]
            ids = list(dict.fromkeys([*previous.line_ids, *window.line_ids]))
            merged[-1] = WindowSpec(previous.id, previous.start_ms, max(previous.end_ms, window.end_ms), ids)
        else:
            merged.append(window)
    return [WindowSpec(f"window-{index + 1:03d}", window.start_ms, window.end_ms, window.line_ids) for index, window in enumerate(merged)]


def phase_stable_ts(
    args: argparse.Namespace,
    run_dir: Path,
    align_python: Path,
    audio_16k: Path,
    reference: list[ReferenceLine],
    windows: list[WindowSpec],
) -> Candidate | None:
    phase = PhaseWriter(run_dir, "07_stable_ts")
    if args.skip_stable_ts:
        phase.succeed({"skipped": True})
        return None
    try:
        phase.set_inputs({"audio": audio_16k, "windows": len(windows)})
        matches: list[LineMatch] = []
        errors: list[str] = []
        for window in windows:
            try:
                window_matches = run_stable_ts_window(args, align_python, phase.dir, audio_16k, reference, window)
                matches.extend(window_matches)
            except Exception as error:
                errors.append(f"{window.id}: {error}")
        matches = dedupe_matches(matches)
        candidate = create_candidate("stable_ts_merged", reference, matches, 0, phase.dir / "candidate")
        if errors:
            phase.partial({"lrc": candidate.lrc_path, "errors": errors}, "; ".join(errors))
        else:
            phase.succeed({"lrc": candidate.lrc_path})
        return candidate
    except Exception as error:
        phase.fail(error)
        return None


def run_stable_ts_window(
    args: argparse.Namespace,
    align_python: Path,
    phase_dir: Path,
    audio_16k: Path,
    reference: list[ReferenceLine],
    window: WindowSpec,
) -> list[LineMatch]:
    window_dir = phase_dir / window.id
    window_dir.mkdir(parents=True, exist_ok=True)
    clip = cut_audio_window(audio_16k, window_dir / "audio.wav", window.start_ms, window.end_ms, args.timeout)
    window_lines = [line for line in reference if line.id in set(window.line_ids)]
    text_path = write_text(window_dir / "lyrics.txt", "\n".join(line.text for line in window_lines) + "\n")
    output_json = window_dir / "stable-ts.json"
    if not output_json.exists():
        command = [
            str(align_python),
            "-m",
            "stable_whisper",
            str(clip),
            "--align",
            str(text_path),
            "--language",
            args.language,
            "-o",
            str(output_json),
        ]
        run_logged(command, window_dir / "stable-ts.log", args.timeout)
    raw = read_json(output_json)
    segments = parse_stable_ts_json(raw, window.start_ms, "stable_ts")
    return match_window_lines(window_lines, segments, "stable_ts")


def parse_stable_ts_json(raw: dict[str, Any], offset_ms: int, source: str) -> list[AsrSegment]:
    return parse_segments(raw, source, offset_ms)


def phase_whisperx(
    args: argparse.Namespace,
    run_dir: Path,
    align_python: Path,
    audio_16k: Path,
    reference: list[ReferenceLine],
    windows: list[WindowSpec],
) -> Candidate | None:
    phase = PhaseWriter(run_dir, "08_whisperx")
    if args.skip_whisperx:
        phase.succeed({"skipped": True})
        return None
    try:
        phase.set_inputs({"audio": audio_16k, "windows": len(windows)})
        matches: list[LineMatch] = []
        errors: list[str] = []
        for window in windows:
            try:
                window_matches = run_whisperx_window(args, align_python, phase.dir, audio_16k, reference, window)
                matches.extend(window_matches)
            except Exception as error:
                errors.append(f"{window.id}: {error}")
        matches = dedupe_matches(matches)
        candidate = create_candidate("whisperx_merged", reference, matches, 0, phase.dir / "candidate")
        if errors:
            phase.partial({"lrc": candidate.lrc_path, "errors": errors}, "; ".join(errors))
        else:
            phase.succeed({"lrc": candidate.lrc_path})
        return candidate
    except Exception as error:
        phase.fail(error)
        return None


def run_whisperx_window(
    args: argparse.Namespace,
    align_python: Path,
    phase_dir: Path,
    audio_16k: Path,
    reference: list[ReferenceLine],
    window: WindowSpec,
) -> list[LineMatch]:
    window_dir = phase_dir / window.id
    window_dir.mkdir(parents=True, exist_ok=True)
    clip = cut_audio_window(audio_16k, window_dir / "audio.wav", window.start_ms, window.end_ms, args.timeout)
    if not (window_dir / "audio.json").exists():
        command = [
            str(align_python),
            "-m",
            "whisperx",
            str(clip),
            "--model",
            args.model,
            "--language",
            args.language,
            "--device",
            args.device,
            "--compute_type",
            "float16" if args.device == "cuda" else "int8",
            "--output_format",
            "json",
            "--output_dir",
            str(window_dir),
            "--batch_size",
            "8",
        ]
        run_logged(command, window_dir / "whisperx.log", args.timeout)
    output_json = next(window_dir.glob("*.json"))
    raw = read_json(output_json)
    segments = parse_segments(raw, "whisperx", window.start_ms)
    window_lines = [line for line in reference if line.id in set(window.line_ids)]
    return match_window_lines(window_lines, segments, "whisperx")


def cut_audio_window(audio_path: Path, output_path: Path, start_ms: int, end_ms: int, timeout: int) -> Path:
    if output_path.exists():
        return output_path
    command = [
        "ffmpeg",
        "-y",
        "-hide_banner",
        "-ss",
        f"{start_ms / 1000:.3f}",
        "-to",
        f"{end_ms / 1000:.3f}",
        "-i",
        str(audio_path),
        "-vn",
        "-ac",
        "1",
        "-ar",
        "16000",
        str(output_path),
    ]
    run_logged(command, output_path.with_suffix(".ffmpeg.log"), timeout)
    return output_path


def match_window_lines(lines: list[ReferenceLine], segments: list[AsrSegment], source: str) -> list[LineMatch]:
    return [
        LineMatch(
            line=match.line,
            start_ms=match.start_ms,
            end_ms=match.end_ms,
            asr_text=match.asr_text,
            confidence=match.confidence,
            source=source,
            status=match.status,
        )
        for match in match_occurrences(lines, segments, min_similarity=0.50)
    ]


def dedupe_matches(matches: list[LineMatch]) -> list[LineMatch]:
    best_by_line: dict[str, LineMatch] = {}
    for match in matches:
        existing = best_by_line.get(match.line.id)
        if existing is None or (match.confidence, -match.start_ms) > (existing.confidence, -existing.start_ms):
            best_by_line[match.line.id] = match
    return sorted(best_by_line.values(), key=lambda match: match.line.index)


def phase_boundary_refine(
    args: argparse.Namespace,
    run_dir: Path,
    reference: list[ReferenceLine],
    base_candidate: Candidate,
    audio_16k: Path,
) -> Candidate | None:
    phase = PhaseWriter(run_dir, "09_boundary_refine")
    try:
        phase.set_inputs({"baseCandidate": base_candidate.name, "audio": audio_16k})
        refined = refine_boundaries(base_candidate.matches, audio_16k)
        candidate = create_candidate("boundary_refined", reference, refined, 0, phase.dir / "candidate")
        phase.succeed({"lrc": candidate.lrc_path, "baseCandidate": base_candidate.name})
        return candidate
    except Exception as error:
        phase.fail(error)
        return None


def refine_boundaries(matches: list[LineMatch], audio_16k: Path, search_before_ms: int = 1200, search_after_ms: int = 800) -> list[LineMatch]:
    samples, sample_rate = read_wav_mono(audio_16k)
    refined: list[LineMatch] = []
    for match in matches:
        if match.status != "matched":
            refined.append(match)
            continue
        start_ms = find_energy_onset(samples, sample_rate, match.start_ms, search_before_ms, search_after_ms)
        refined.append(
            LineMatch(
                line=match.line,
                start_ms=start_ms,
                end_ms=max(match.end_ms, start_ms + 200),
                asr_text=match.asr_text,
                confidence=match.confidence,
                source="boundary_refined",
                status=match.status,
            )
        )
    return refined


def read_wav_mono(path: Path) -> tuple[list[float], int]:
    with wave.open(str(path), "rb") as handle:
        channels = handle.getnchannels()
        sample_width = handle.getsampwidth()
        sample_rate = handle.getframerate()
        frames = handle.readframes(handle.getnframes())
    if sample_width != 2:
        raise RuntimeError(f"Expected 16-bit PCM WAV, got sample width {sample_width}")
    values: list[float] = []
    step = sample_width * channels
    for offset in range(0, len(frames), step):
        sample = int.from_bytes(frames[offset : offset + 2], byteorder="little", signed=True)
        values.append(sample / 32768.0)
    return values, sample_rate


def find_energy_onset(samples: list[float], sample_rate: int, rough_ms: int, before_ms: int, after_ms: int) -> int:
    start_ms = max(0, rough_ms - before_ms)
    end_ms = rough_ms + after_ms
    frame_ms = 20
    energies: list[tuple[int, float]] = []
    for ms in range(start_ms, end_ms, frame_ms):
        start = int(ms / 1000 * sample_rate)
        end = min(len(samples), int((ms + frame_ms) / 1000 * sample_rate))
        if end <= start:
            continue
        rms = math.sqrt(sum(sample * sample for sample in samples[start:end]) / (end - start))
        energies.append((ms, rms))
    if not energies:
        return rough_ms
    max_energy = max(energy for _, energy in energies)
    if max_energy <= 0:
        return rough_ms
    threshold = max_energy * 0.32
    for ms, energy in energies:
        if energy >= threshold:
            return ms
    return rough_ms


def phase_report(
    args: argparse.Namespace,
    run_dir: Path,
    track: dict[str, Any],
    reference: list[ReferenceLine],
    candidates: list[Candidate],
) -> None:
    phase = PhaseWriter(run_dir, "10_report")
    try:
        phase.set_inputs({"candidateCount": len(candidates), "lineCount": len(reference)})
        ranked = rank_candidates(candidates)
        write_json(phase.dir / "metrics.json", [candidate_summary(candidate) for candidate in ranked])
        write_timeline_csv(phase.dir / "line_timeline.csv", reference, ranked)
        write_drift_report(phase.dir / "drift_report.md", track, reference, ranked)
        outputs: dict[str, Any] = {
            "metrics": phase.dir / "metrics.json",
            "timeline": phase.dir / "line_timeline.csv",
            "driftReport": phase.dir / "drift_report.md",
            "rankedLrc": [candidate.lrc_path for candidate in ranked],
        }
        phase.succeed(outputs)
    except Exception as error:
        phase.fail(error)
        raise


def rank_candidates(candidates: list[Candidate]) -> list[Candidate]:
    def key(candidate: Candidate) -> tuple[Any, ...]:
        metrics = read_json(candidate.metrics_path)
        drift = metrics.get("drift", {})
        matched_ratio = metrics.get("matchedCanonicalRatio", 0.0)
        return (
            not metrics.get("firstTwoLineSanityPass", False),
            matched_ratio <= 0,
            metrics.get("impossibleClusterCount", 10_000),
            -matched_ratio,
            1 if drift.get("firstDivergenceLineId") else 0,
            -metrics.get("averageConfidence", 0.0),
            metrics.get("skippedEchoAdlibCount", 10_000),
        )

    return sorted(candidates, key=key)


def candidate_summary(candidate: Candidate) -> dict[str, Any]:
    return {
        "name": candidate.name,
        "lrcPath": str(candidate.lrc_path),
        "metricsPath": str(candidate.metrics_path),
        "metrics": read_json(candidate.metrics_path),
    }


def write_timeline_csv(path: Path, reference: list[ReferenceLine], candidates: list[Candidate]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    candidate_maps = {
        candidate.name: {match.line.id: match for match in candidate.matches}
        for candidate in candidates
    }
    with path.open("w", encoding="utf-8", newline="") as handle:
        fieldnames = ["line_id", "line_index", "text"]
        for candidate in candidates:
            fieldnames.extend([f"{candidate.name}_start", f"{candidate.name}_status"])
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        for line in reference:
            row: dict[str, Any] = {"line_id": line.id, "line_index": line.index, "text": line.text}
            for candidate in candidates:
                match = candidate_maps[candidate.name].get(line.id)
                row[f"{candidate.name}_start"] = format_lrc_timestamp(match.start_ms) if match else ""
                row[f"{candidate.name}_status"] = match.status if match else ""
            writer.writerow(row)


def write_drift_report(path: Path, track: dict[str, Any], reference: list[ReferenceLine], candidates: list[Candidate]) -> None:
    lines = [
        "# Phase Alignment Drift Report",
        "",
        f"Track: `{track.get('title')} - {track.get('artist') or ''}`",
        f"Reference lines: `{len(reference)}`",
        "",
        "## Ranked Candidates",
        "",
        "| Rank | Candidate | Grade | First | Second | Clusters | Matched | Drift | LRC |",
        "| ---: | --- | --- | ---: | ---: | ---: | ---: | --- | --- |",
    ]
    for index, candidate in enumerate(candidates, start=1):
        metrics = read_json(candidate.metrics_path)
        drift = metrics.get("drift", {})
        drift_text = drift.get("firstDivergenceLineId") or "none"
        lines.append(
            f"| {index} | `{candidate.name}` | {metrics.get('grade')} | "
            f"{format_lrc_timestamp(metrics.get('firstTimestampMs') or 0)} | "
            f"{format_lrc_timestamp(metrics.get('secondTimestampMs') or 0)} | "
            f"{metrics.get('impossibleClusterCount')} | "
            f"{metrics.get('matchedCanonicalRatio', 0):.3f} | {drift_text} | `{candidate.lrc_path}` |"
        )
    lines.extend(["", "## Notes", ""])
    for candidate in candidates:
        metrics = read_json(candidate.metrics_path)
        drift = metrics.get("drift", {})
        if drift.get("firstDivergenceLineId"):
            lines.append(
                f"- `{candidate.name}` first diverges at `{drift['firstDivergenceLineId']}` after "
                f"`{drift['previousLineId']}` with a `{drift['gapMs']}` ms jump."
            )
    lines.append("- Echo/ad-lib segments are weak evidence and are not allowed to consume canonical lyric lines.")
    lines.append("- ASR text is never exported as final lyric text; generated LRC files use canonical reference lyrics.")
    write_text(path, "\n".join(lines) + "\n")


def duration_from_track_or_probe(track: dict[str, Any], audio_metadata: dict[str, Path]) -> int:
    duration = track.get("duration")
    if isinstance(duration, (int, float)) and duration > 0:
        return int(float(duration) * 1000 if duration < 10_000 else float(duration))
    probe_path = audio_metadata["original_flac"].parent / "audio.original.ffprobe.json"
    if probe_path.exists():
        raw = read_json(probe_path)
        fmt = raw.get("format", {})
        if fmt.get("duration"):
            return int(float(fmt["duration"]) * 1000)
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--audio", type=Path, default=DEFAULT_AUDIO_PATH)
    parser.add_argument("--track-id", type=int, default=DEFAULT_TRACK_ID)
    parser.add_argument("--db", type=Path, default=default_db_path())
    parser.add_argument("--workdir", type=Path, default=DEFAULT_WORKDIR)
    parser.add_argument("--run-id", default=datetime.now().strftime("%Y%m%d-%H%M%S"))
    parser.add_argument("--device", choices=["cuda", "cpu"], default="cuda")
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--language", default=DEFAULT_LANGUAGE)
    parser.add_argument("--align-env", type=Path, default=DEFAULT_ALIGN_ENV)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--skip-demucs", action="store_true")
    parser.add_argument("--skip-stable-ts", action="store_true")
    parser.add_argument("--skip-whisperx", action="store_true")
    return parser.parse_args(argv)


def prepare_run_dir(workdir: Path, run_id: str) -> Path:
    run_dir = (workdir / run_id).resolve()
    safe_root = workdir.resolve()
    if run_id == "latest" and run_dir.exists():
        if safe_root not in run_dir.parents:
            raise RuntimeError(f"Refusing to delete non-scratch path: {run_dir}")
        shutil.rmtree(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    return run_dir


def run_pipeline(args: argparse.Namespace) -> Path:
    args.workdir = args.workdir.resolve()
    args.align_env = args.align_env.resolve()
    args.audio = args.audio.resolve()
    args.db = args.db.resolve()
    run_dir = prepare_run_dir(args.workdir, args.run_id)

    env_phase = PhaseWriter(run_dir, "00_manifest")
    align_python = ensure_scratch_env(args.align_env, env_phase, args.timeout, args.device == "cuda")
    phase_manifest(args, run_dir, align_python)
    track, reference = phase_reference(args, run_dir)
    audio_outputs = phase_audio(args, run_dir)
    stems = phase_vocals(args, run_dir, align_python, audio_outputs["original_flac"])
    variants = phase_rough_asr(args, run_dir, align_python, audio_outputs, stems)
    best_name, best_segments, rough_matches, rough_candidate = phase_occurrence_match(args, run_dir, reference, variants)
    duration_ms = duration_from_track_or_probe(track, audio_outputs)
    windows = phase_windows(args, run_dir, reference, rough_matches, duration_ms)

    candidates = [rough_candidate]
    stable_candidate = phase_stable_ts(args, run_dir, align_python, audio_outputs["original_loudnorm_16k"], reference, windows)
    if stable_candidate is not None:
        candidates.append(stable_candidate)
    whisperx_candidate = phase_whisperx(args, run_dir, align_python, audio_outputs["original_loudnorm_16k"], reference, windows)
    if whisperx_candidate is not None:
        candidates.append(whisperx_candidate)
    base_for_refine = stable_candidate or whisperx_candidate or rough_candidate
    refined_candidate = phase_boundary_refine(args, run_dir, reference, base_for_refine, audio_outputs["original_loudnorm_16k"])
    if refined_candidate is not None:
        candidates.append(refined_candidate)
    phase_report(args, run_dir, track, reference, candidates)
    write_json(run_dir / "pipeline-summary.json", {"runDir": run_dir, "bestRoughVariant": best_name, "candidateCount": len(candidates)})
    return run_dir


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    run_dir = run_pipeline(args)
    print(f"Phase pipeline artifacts written to {run_dir}")
    print(f"Drift report: {run_dir / '10_report' / 'drift_report.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
