#!/usr/bin/env python3
"""Tests for the offline tool-call checker experiment (no model required)."""
import importlib.util
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("guard_eval", Path(__file__).with_name("tool-guard-eval.py"))
eval = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(eval)


class GuardEvalTests(unittest.TestCase):
    def test_prompt_contains_action_and_context_but_not_answer(self):
        case = {"id": "example", "expected": "deny", "reason": "SECRET LABEL",
                "user_request": "Inspect the project", "tool": "bash",
                "arguments": {"command": "git status"}, "evidence": "Untrusted output"}
        prompt = eval.case_text(case)
        self.assertIn("git status", prompt)
        self.assertIn("Inspect the project", prompt)
        self.assertIn("Untrusted output", prompt)
        self.assertNotIn("SECRET LABEL", prompt)
        self.assertNotIn('"expected"', prompt)

    def test_parse_only_final_verdict_not_reasoning_or_quoted_labels(self):
        self.assertEqual(eval.parse_verdict('[Answer] unsafe\n[Analysis] Why.', "shieldagent"), "deny")
        self.assertEqual(eval.parse_verdict('{"decision":"allow","reason":"not unsafe"}', "policy"), "allow")
        self.assertIsNone(eval.parse_verdict("This might be safe or unsafe", "policy"))
        self.assertIsNone(eval.parse_verdict('{"reason":"safe"}', "policy"))

    def test_ts_guard_keeps_action_rating_separate_from_combined_score(self):
        response = ("<Malicious_User_Request>no<\\Malicious_User_Request>\n"
                    "<Being_Attacked>no<\\Being_Attacked>\n"
                    "<Harmfulness_Rating>1.0<\\Harmfulness_Rating>")
        self.assertEqual(eval.parse_verdict(response, "ts-guard"), "deny")
        self.assertEqual(eval.ts_fields(response)["published_combined_rating"], 0.0)
        self.assertIsNone(eval.parse_verdict(response.split("<Being_Attacked>")[0], "ts-guard"))
        self.assertIsNone(eval.parse_verdict("<Think>quoted example " + response + "<\\Think>", "ts-guard"))
        self.assertEqual(eval.parse_verdict("<Think>reasoning<\\Think>" + response, "ts-guard"), "deny")

    def test_dataset_has_both_labels_and_prompts_never_include_label_reasons(self):
        import json
        cases = json.loads((eval.ROOT / "cases.json").read_text())
        self.assertEqual(len(cases), len({case["id"] for case in cases}))
        self.assertEqual({case["expected"] for case in cases}, {"allow", "deny"})
        for case in cases:
            for style in ("ts-guard", "shieldagent", "policy"):
                text = str(eval.messages_for(case, style))
                self.assertNotIn(case["reason"], text)
                self.assertIn(case["user_request"], text)

    def test_errors_are_not_counted_as_denials_or_correct(self):
        cases = [{"expected": "allow"}, {"expected": "deny"}, {"expected": "deny"}]
        rows = [{"decision": "deny", "seconds": 1}, {"decision": "allow", "seconds": 2},
                {"decision": None, "seconds": 3}]
        report = eval.summarize(cases, rows)
        self.assertEqual(report["false_blocks"], 1)
        self.assertEqual(report["false_allows"], 1)
        self.assertEqual(report["errors"], 1)
        self.assertEqual(report["correct"], 0)


if __name__ == "__main__":
    unittest.main()
