import unittest

import asr_first_eval as asr_eval


class AsrFirstEvalTests(unittest.TestCase):
    def test_parse_transcript_handles_qwen_and_whisper_shapes(self):
        qwen = {
            "segments": [
                {
                    "text": "And you are all that I need",
                    "start": 24.0,
                    "end": 28.5,
                    "words": [
                        {"word": "And", "start": 24.0, "end": 24.2},
                        {"word": "you", "start": 24.2, "end": 24.4},
                    ],
                }
            ]
        }
        whisper = {
            "language": "en",
            "segments": [
                {
                    "text": " You're my gravity",
                    "start": 28.0,
                    "end": 31.0,
                    "words": [
                        {"word": "You're", "start": 28.0, "end": 28.3},
                        {"word": "my", "start": 28.3, "end": 28.5},
                        {"word": "gravity", "start": 28.5, "end": 31.0},
                    ],
                }
            ],
        }

        qwen_transcript = asr_eval.parse_asr_transcript(qwen, "qwen")
        whisper_transcript = asr_eval.parse_asr_transcript(whisper, "whisper")

        self.assertEqual(qwen_transcript.segments[0].start_ms, 24000)
        self.assertEqual([word.text for word in qwen_transcript.words], ["And", "you"])
        self.assertEqual(whisper_transcript.language, "en")
        self.assertEqual(whisper_transcript.words[-1].end_ms, 31000)

    def test_parse_qwen_srt_output(self):
        transcript = asr_eval.parse_srt_transcript(
            "1\n00:00:24,000 --> 00:00:28,500\nAnd you are all that I need\n\n"
            "2\n00:00:28,500 --> 00:00:31,000\nYou're my gravity\n",
            "qwen",
            "en",
        )

        self.assertEqual(transcript.segments[0].start_ms, 24000)
        self.assertEqual(transcript.segments[1].text, "You're my gravity")
        self.assertGreaterEqual(len(transcript.words), 10)

    def test_monotonic_alignment_resolves_repeated_lines_in_order(self):
        transcript = asr_eval.AsrTranscript(
            source="test",
            language="en",
            segments=[],
            words=[
                asr_eval.TimedWord("And", 24000, 24100),
                asr_eval.TimedWord("you", 24100, 24200),
                asr_eval.TimedWord("are", 24200, 24300),
                asr_eval.TimedWord("all", 24300, 24400),
                asr_eval.TimedWord("that", 24400, 24500),
                asr_eval.TimedWord("I", 24500, 24600),
                asr_eval.TimedWord("need", 24600, 25000),
                asr_eval.TimedWord("filler", 40000, 41000),
                asr_eval.TimedWord("And", 60000, 60100),
                asr_eval.TimedWord("you", 60100, 60200),
                asr_eval.TimedWord("are", 60200, 60300),
                asr_eval.TimedWord("all", 60300, 60400),
                asr_eval.TimedWord("that", 60400, 60500),
                asr_eval.TimedWord("I", 60500, 60600),
                asr_eval.TimedWord("need", 60600, 61000),
            ],
            raw={},
        )

        generated = asr_eval.align_lyrics_to_asr(
            ["And you are all that I need", "And you are all that I need"],
            transcript,
        )

        self.assertEqual([line.start_ms for line in generated.lines], [24000, 60000])
        self.assertEqual(generated.metrics["matchedLineCount"], 2)

    def test_segment_alignment_does_not_consume_next_line(self):
        transcript = asr_eval.AsrTranscript(
            source="whisper",
            language="en",
            segments=[
                asr_eval.AsrSegment("You are all that I need", 21740, 25180),
                asr_eval.AsrSegment("You're my gravity", 25180, 28080),
            ],
            words=[
                asr_eval.TimedWord("You", 21740, 22860),
                asr_eval.TimedWord("are", 22860, 23420),
                asr_eval.TimedWord("all", 23420, 23900),
                asr_eval.TimedWord("that", 23900, 24060),
                asr_eval.TimedWord("I", 24060, 24500),
                asr_eval.TimedWord("need", 24500, 25180),
                asr_eval.TimedWord("You're", 25180, 26980),
                asr_eval.TimedWord("my", 26980, 27340),
                asr_eval.TimedWord("gravity", 27340, 28080),
            ],
            raw={},
        )

        generated = asr_eval.align_lyrics_to_asr(
            ["And you are all that I need", "You're my gravity"],
            transcript,
        )

        self.assertEqual([line.start_ms for line in generated.lines], [21740, 25180])
        self.assertTrue(generated.metrics["firstTwoLineSanityPass"])

    def test_first_two_line_sanity_rejects_current_aeneas_shape(self):
        generated = asr_eval.GeneratedLrc(
            lrc="",
            lines=[
                asr_eval.LineAlignment(0, "And you are all that I need", 0, 35360, 7, 1.0, False),
                asr_eval.LineAlignment(1, "You're my gravity", 35360, 36160, 3, 1.0, False),
            ],
            metrics={},
        )

        metrics = asr_eval.score_asr_first_alignment(generated.lines, 2, 10.0, 10)

        self.assertFalse(metrics["firstTwoLineSanityPass"])
        self.assertEqual(metrics["firstLineDeltaMs"], 24000)
        self.assertEqual(metrics["secondLineDeltaMs"], 7360)

    def test_asr_rank_prefers_sanity_and_cluster_free_result(self):
        vocal = {
            "name": "demucs_vocals_whisper_turbo",
            "metrics": {
                "firstTwoLineSanityPass": True,
                "impossibleClusterCount": 0,
                "maxLineDurationMs": 3500,
                "matchedLineRatio": 0.8,
                "averageWordSimilarity": 0.8,
            },
        }
        mixed = {
            "name": "original_qwen",
            "metrics": {
                "firstTwoLineSanityPass": False,
                "impossibleClusterCount": 0,
                "maxLineDurationMs": 2000,
                "matchedLineRatio": 1.0,
                "averageWordSimilarity": 1.0,
            },
        }

        ranked = asr_eval.rank_asr_results([mixed, vocal])

        self.assertEqual(ranked[0]["name"], "demucs_vocals_whisper_turbo")


if __name__ == "__main__":
    unittest.main()
