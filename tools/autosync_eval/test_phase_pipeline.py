import unittest
import tempfile
from pathlib import Path

import air_i_breathe_phase_pipeline as phase


class PhasePipelineTests(unittest.TestCase):
    def test_reference_cleanup_indexes_occurrences_without_collapsing_repeats(self):
        reference, removals = phase.build_reference_lines(
            [
                "LyricsVideosListen",
                "",
                "And you are all that I need",
                "My gravity",
                "My gravity",
                "Pulling you in so deep",
            ]
        )

        self.assertEqual([line.id for line in reference], ["L001", "L002", "L003", "L004"])
        self.assertEqual(
            [line.text for line in reference],
            [
                "And you are all that I need",
                "My gravity",
                "My gravity",
                "Pulling you in so deep",
            ],
        )
        self.assertEqual(reference[1].normalized, reference[2].normalized)
        self.assertEqual(removals[0]["reason"], "junk")

    def test_weak_echo_segments_are_skipped_before_matching_canonical_lines(self):
        reference, _ = phase.build_reference_lines(
            [
                "And you are all that I need",
                "You're my gravity",
            ]
        )
        segments = [
            phase.AsrSegment("Oh", 10_000, 10_400, compression_ratio=1.1),
            phase.AsrSegment("And you are all that I need", 21_660, 25_180),
            phase.AsrSegment("Oh", 25_200, 25_400),
            phase.AsrSegment("You're my gravity", 25_180, 28_080),
        ]

        matches = phase.match_occurrences(reference, segments)

        self.assertEqual([match.start_ms for match in matches], [21_660, 25_180])
        self.assertTrue(all(match.status == "matched" for match in matches))
        skipped = phase.collect_skipped_segments(segments, matches)
        self.assertEqual([segment.text for segment in skipped], ["Oh", "Oh"])

    def test_monotonic_matching_keeps_repeated_lines_in_order(self):
        reference, _ = phase.build_reference_lines(
            [
                "My gravity",
                "Pulling you in so deep",
                "My gravity",
            ]
        )
        segments = [
            phase.AsrSegment("My gravity", 50_240, 52_900),
            phase.AsrSegment("Pulling you in so deep", 54_860, 58_160),
            phase.AsrSegment("My gravity", 204_720, 207_460),
        ]

        matches = phase.match_occurrences(reference, segments)

        self.assertEqual([match.line_id for match in matches], ["L001", "L002", "L003"])
        self.assertEqual([match.start_ms for match in matches], [50_240, 54_860, 204_720])

    def test_drift_report_flags_large_jump_after_early_correct_section(self):
        reference, _ = phase.build_reference_lines(
            [
                "And you are all that I need",
                "You're my gravity",
                "My gravity",
                "Pulling you in so deep",
                "You're my gravity",
                "I know you're holding me now",
            ]
        )
        matches = [
            phase.LineMatch(reference[0], 21_660, 25_180, "And you are all that I need", 0.98, "rough_asr", "matched"),
            phase.LineMatch(reference[1], 25_180, 28_080, "You're my gravity", 1.0, "rough_asr", "matched"),
            phase.LineMatch(reference[2], 50_240, 52_900, "My gravity", 1.0, "rough_asr", "matched"),
            phase.LineMatch(reference[3], 54_860, 58_160, "Pulling you in so deep", 1.0, "rough_asr", "matched"),
            phase.LineMatch(reference[4], 203_080, 204_720, "You're my gravity", 1.0, "rough_asr", "matched"),
            phase.LineMatch(reference[5], 217_080, 219_560, "I know you're holding me now", 1.0, "rough_asr", "matched"),
        ]

        report = phase.detect_drift(matches, max_gap_ms=45_000, min_prior_matches=3)

        self.assertEqual(report["firstDivergenceLineId"], "L005")
        self.assertEqual(report["previousLineId"], "L004")
        self.assertGreaterEqual(report["gapMs"], 140_000)

    def test_rank_prefers_partial_real_alignment_over_zero_match_interpolation(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            zero_metrics = root / "zero.json"
            partial_metrics = root / "partial.json"
            phase.write_json(
                zero_metrics,
                {
                    "firstTwoLineSanityPass": False,
                    "impossibleClusterCount": 0,
                    "matchedCanonicalRatio": 0.0,
                    "averageConfidence": 0.0,
                    "skippedEchoAdlibCount": 0,
                    "drift": {"firstDivergenceLineId": None},
                },
            )
            phase.write_json(
                partial_metrics,
                {
                    "firstTwoLineSanityPass": False,
                    "impossibleClusterCount": 1,
                    "matchedCanonicalRatio": 0.2,
                    "averageConfidence": 0.8,
                    "skippedEchoAdlibCount": 0,
                    "drift": {"firstDivergenceLineId": "L004"},
                },
            )
            zero = phase.Candidate("zero", [], root / "zero.lrc", zero_metrics)
            partial = phase.Candidate("partial", [], root / "partial.lrc", partial_metrics)

            ranked = phase.rank_candidates([zero, partial])

            self.assertEqual(ranked[0].name, "partial")


if __name__ == "__main__":
    unittest.main()
