import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import air_i_breathe_eval as eval_script


class AutoSyncEvalTests(unittest.TestCase):
    def test_extracts_plain_block_from_lyricsfile(self):
        lyricsfile = """version: '1.0'
metadata:
  title: Air I Breathe
lines: []
plain: |-
  LyricsVideosListen
  And you are all that I need

  You're my gravity
"""

        self.assertEqual(
            eval_script.extract_plain_lyrics(lyricsfile),
            "LyricsVideosListen\nAnd you are all that I need\n\nYou're my gravity",
        )

    def test_cleans_blank_and_known_junk_lines(self):
        lines = eval_script.clean_lyrics_lines(
            "LyricsVideosListen\n\nAnd you are all that I need\n  \nYou're my gravity\n"
        )

        self.assertEqual(lines, ["And you are all that I need", "You're my gravity"])

    def test_parses_qwen_words_from_flat_and_segment_json(self):
        words = eval_script.parse_qwen_words(
            {
                "segments": [
                    {
                        "words": [
                            {"word": "And", "start": 40.96, "end": 41.1},
                            {"text": "you", "start_ms": 41100, "end_ms": 41200},
                        ]
                    }
                ]
            }
        )

        self.assertEqual([word.text for word in words], ["And", "you"])
        self.assertEqual(words[0].start_ms, 40960)
        self.assertEqual(words[1].end_ms, 41200)

    def test_generates_lrc_and_detects_impossible_clusters(self):
        lines = ["And you are all that I need", "You're my gravity", "My gravity", "Pulling you in so deep"]
        generated = eval_script.generate_lrc_from_words(
            lines,
            [
                eval_script.TimedWord("And", 40960, 41000),
                eval_script.TimedWord("you", 41010, 41040),
                eval_script.TimedWord("are", 41050, 41070),
                eval_script.TimedWord("all", 41080, 41100),
                eval_script.TimedWord("that", 41110, 41130),
                eval_script.TimedWord("I", 41140, 41150),
                eval_script.TimedWord("need", 41160, 41180),
                eval_script.TimedWord("You're", 42240, 42240),
                eval_script.TimedWord("my", 42240, 42240),
                eval_script.TimedWord("gravity", 42240, 42240),
                eval_script.TimedWord("My", 42260, 42260),
                eval_script.TimedWord("gravity", 42260, 42260),
                eval_script.TimedWord("Pulling", 42280, 42280),
                eval_script.TimedWord("you", 42280, 42280),
                eval_script.TimedWord("in", 42280, 42280),
                eval_script.TimedWord("so", 42280, 42280),
                eval_script.TimedWord("deep", 42280, 42280),
            ],
        )

        self.assertIn("[00:40.96]And you are all that I need", generated.lrc)
        self.assertGreaterEqual(generated.metrics["impossibleClusterCount"], 1)
        self.assertEqual(generated.metrics["grade"], "bad")

    def test_first_occurrence_search_reuses_first_match_for_repeated_lines(self):
        words = [
            eval_script.TimedWord("And", 1000, 1100),
            eval_script.TimedWord("you", 1110, 1200),
            eval_script.TimedWord("are", 1210, 1300),
            eval_script.TimedWord("all", 1310, 1400),
            eval_script.TimedWord("that", 1410, 1500),
            eval_script.TimedWord("I", 1510, 1550),
            eval_script.TimedWord("need", 1560, 1650),
            eval_script.TimedWord("My", 3000, 3100),
            eval_script.TimedWord("gravity", 3110, 3300),
            eval_script.TimedWord("And", 5000, 5100),
            eval_script.TimedWord("you", 5110, 5200),
            eval_script.TimedWord("are", 5210, 5300),
            eval_script.TimedWord("all", 5310, 5400),
            eval_script.TimedWord("that", 5410, 5500),
            eval_script.TimedWord("I", 5510, 5550),
            eval_script.TimedWord("need", 5560, 5650),
        ]

        generated = eval_script.generate_first_occurrence_lrc(
            [
                "And you are all that I need",
                "And you are all that I need",
                "And you are all that I need",
            ],
            words,
        )

        self.assertEqual([line.start_ms for line in generated.lines], [1000, 1000, 1000])
        self.assertEqual(generated.metrics["matchedLineCount"], 3)
        self.assertGreaterEqual(generated.metrics["impossibleClusterCount"], 1)

    def test_write_first_occurrence_search_result_creates_artifacts(self):
        with TemporaryDirectory() as temp_dir:
            workdir = Path(temp_dir)
            source_json = workdir / "runs" / "transcribe_align_4096" / "qwen.json"
            source_json.parent.mkdir(parents=True)
            source_json.write_text("{}", encoding="utf-8")

            result = eval_script.write_first_occurrence_search_result(
                "first_occurrence_search",
                ["And you are all that I need", "And you are all that I need"],
                [
                    eval_script.TimedWord("And", 1000, 1100),
                    eval_script.TimedWord("you", 1110, 1200),
                    eval_script.TimedWord("are", 1210, 1300),
                    eval_script.TimedWord("all", 1310, 1400),
                    eval_script.TimedWord("that", 1410, 1500),
                    eval_script.TimedWord("I", 1510, 1550),
                    eval_script.TimedWord("need", 1560, 1650),
                ],
                workdir,
                source_json,
            )

            run_dir = workdir / "runs" / "first_occurrence_search"
            self.assertEqual(result["name"], "first_occurrence_search")
            self.assertEqual(result["mode"], "first_occurrence_search")
            self.assertTrue((run_dir / "generated.lrc").exists())
            self.assertTrue((run_dir / "line-matches.json").exists())

    def test_rank_results_prefers_fewer_clusters_then_higher_match_ratio(self):
        slow_clean = {"name": "slow-clean", "metrics": {"impossibleClusterCount": 0, "matchedLineRatio": 0.6, "interpolatedLineRatio": 0.2, "averageWordSimilarity": 0.8}, "runtimeSeconds": 20}
        fast_clustered = {"name": "fast-clustered", "metrics": {"impossibleClusterCount": 2, "matchedLineRatio": 1.0, "interpolatedLineRatio": 0.0, "averageWordSimilarity": 1.0}, "runtimeSeconds": 1}
        better_match = {"name": "better-match", "metrics": {"impossibleClusterCount": 0, "matchedLineRatio": 0.9, "interpolatedLineRatio": 0.1, "averageWordSimilarity": 0.8}, "runtimeSeconds": 30}

        ranked = eval_script.rank_results([slow_clean, fast_clustered, better_match])

        self.assertEqual([result["name"] for result in ranked], ["better-match", "slow-clean", "fast-clustered"])

    def test_anchor_candidates_only_include_lines_unique_in_the_source_lyrics(self):
        lines = [
            "And you are all that I need",
            "Short unique",
            "A unique bridge phrase worth anchoring",
            "And you are all that I need",
            "Another unique phrase with enough words",
            "A unique bridge phrase worth anchoring",
        ]

        anchors = eval_script.select_anchor_lines(lines)

        self.assertEqual(anchors, ["Another unique phrase with enough words"])

    def test_anchor_chunks_do_not_fall_back_to_repeated_or_fixed_chunks(self):
        lines = [
            "And you are all that I need",
            "A unique bridge phrase worth anchoring",
            "And you are all that I need",
            "My gravity",
        ]
        approximate_lines = [
            eval_script.LineAlignment(0, lines[0], 10_000, 11_000, 6, 0.95, False),
            eval_script.LineAlignment(1, lines[1], 20_000, 21_000, 6, 0.95, False),
            eval_script.LineAlignment(2, lines[2], 30_000, 31_000, 6, 0.95, False),
            eval_script.LineAlignment(3, lines[3], 40_000, 41_000, 2, 0.95, False),
        ]

        chunks = eval_script.build_anchor_chunks(lines, approximate_lines)

        self.assertEqual(chunks, [])

    def test_collect_existing_results_reads_top_level_run_metrics(self):
        with TemporaryDirectory() as temp_dir:
            workdir = Path(temp_dir)
            run_dir = workdir / "runs" / "full_forced_clean"
            nested_dir = workdir / "runs" / "chunked_anchor_forced_align" / "chunk-00"
            run_dir.mkdir(parents=True)
            nested_dir.mkdir(parents=True)
            (run_dir / "generated.lrc").write_text("[00:01.00]Line\n", encoding="utf-8")
            (run_dir / "qwen.json").write_text("{}", encoding="utf-8")
            (run_dir / "metrics.json").write_text(
                '{"grade":"bad","runtimeSeconds":12.5,"mode":"forced_align"}',
                encoding="utf-8",
            )
            (nested_dir / "metrics.json").write_text(
                '{"grade":"bad","runtimeSeconds":1.0,"mode":"forced_align"}',
                encoding="utf-8",
            )

            results = eval_script.collect_existing_results(workdir)

        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["name"], "full_forced_clean")
        self.assertEqual(results[0]["runtimeSeconds"], 12.5)

    def test_stale_chunked_anchor_result_is_hidden_without_two_unique_anchors(self):
        results = [
            {"name": "full_forced_clean", "metrics": {}, "runtimeSeconds": 10},
            {"name": "chunked_anchor_forced_align", "metrics": {}, "runtimeSeconds": 20},
        ]

        filtered = eval_script.filter_results_for_current_anchor_rules(results, ["Only one unique anchor"])

        self.assertEqual([result["name"] for result in filtered], ["full_forced_clean"])

    def test_find_existing_transcription_json_prefers_largest_requested_token_run(self):
        with TemporaryDirectory() as temp_dir:
            workdir = Path(temp_dir)
            low = workdir / "runs" / "transcribe_align_1024" / "qwen.json"
            high = workdir / "runs" / "transcribe_align_4096" / "qwen.json"
            low.parent.mkdir(parents=True)
            high.parent.mkdir(parents=True)
            low.write_text("{}", encoding="utf-8")
            high.write_text("{}", encoding="utf-8")

            source = eval_script.find_existing_transcription_json(workdir, [1024, 2048, 4096])

        self.assertEqual(source, high)


if __name__ == "__main__":
    unittest.main()
