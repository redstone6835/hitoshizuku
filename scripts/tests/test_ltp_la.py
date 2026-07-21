"""LoongArch64 LTP 编排器单元测试。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.ltp_la import (
    append_unique_results,
    classify_case,
    first_missing_index,
    load_jsonl,
    parse_marker_line,
    parse_runtest,
    parse_serial,
)


class RuntestParserTests(unittest.TestCase):
    """验证官方 runtest 行不会被错误拆分或重新编号。"""

    def test_comments_and_blank_lines_do_not_consume_indexes(self) -> None:
        cases = parse_runtest(
            """
            # comment
              getpid01 getpid01

            open01   open01 -T 1
            """
        )
        self.assertEqual([(item.index, item.tag) for item in cases], [(0, "getpid01"), (1, "open01")])
        self.assertEqual(cases[1].command, "open01   open01 -T 1")


class MarkerParserTests(unittest.TestCase):
    """验证串口 marker 和用例边界解析。"""

    def test_marker_can_follow_console_prefix(self) -> None:
        marker = parse_marker_line("qemu:@@LTP\tcase_end\tindex=3\ttag=a=b\tresult=run\n")
        self.assertEqual(
            marker,
            ("case_end", {"index": "3", "tag": "a=b", "result": "run"}),
        )

    def test_serial_parser_preserves_skip_reason(self) -> None:
        parsed = parse_serial(
            "@@LTP\tcase_start\tgroup=default\tscenario=syscalls\tindex=2\ttag=delete_module01\n"
            "@@LTP\tcase_skip\tgroup=default\tscenario=syscalls\tindex=2\t"
            "tag=delete_module01\tcategory=kernel-bound\treason=Linux LKM\n"
            "@@LTP\tcase_end\tgroup=default\tscenario=syscalls\tindex=2\t"
            "tag=delete_module01\tresult=skip\texit=0\n"
            "@@LTP\tshard_end\tnext=3\n"
        )
        self.assertEqual(len(parsed.cases), 1)
        self.assertEqual(parsed.cases[0]["classification"], "static-skip")
        self.assertEqual(parsed.cases[0]["reason"], "Linux LKM")
        self.assertEqual(parsed.shard_end, {"next": "3"})

    def test_unclosed_case_is_reported(self) -> None:
        parsed = parse_serial(
            "@@LTP\tcase_start\tgroup=default\tscenario=fs\tindex=7\ttag=fsx01\n"
            "kernel stopped here\n"
        )
        self.assertEqual(parsed.cases, [])
        self.assertEqual(parsed.starts_without_end[0]["tag"], "fsx01")


class ClassificationTests(unittest.TestCase):
    """验证 LTP 状态和运行器退出状态的组合语义。"""

    def test_summary_pass(self) -> None:
        classification, counts = classify_case(
            {"result": "run", "exit": "0"},
            "Summary:\npassed   3\nfailed   0\nbroken   0\nskipped  0\nwarnings 0\n",
        )
        self.assertEqual(classification, "pass")
        self.assertEqual(counts["passed"], 3)

    def test_failure_wins_over_pass(self) -> None:
        classification, _counts = classify_case(
            {"result": "run", "exit": "0"},
            "case 1 TPASS: ok\ncase 2 TFAIL: mismatch\n",
        )
        self.assertEqual(classification, "fail")

    def test_tconf_is_not_counted_as_pass(self) -> None:
        classification, counts = classify_case(
            {"result": "run", "exit": "0"},
            "feature TCONF: not supported\n",
        )
        self.assertEqual(classification, "tconf")
        self.assertEqual(counts["skipped"], 1)

    def test_nonzero_exit_without_ltp_failure_is_harness_error(self) -> None:
        classification, _counts = classify_case(
            {"result": "run", "exit": "2"},
            "no status emitted\n",
        )
        self.assertEqual(classification, "harness-error")

    def test_ltp_pan_status_is_authoritative(self) -> None:
        classification, counts = classify_case(
            {"result": "run", "exit": "32", "ltp_stat": "32", "termination": "exited"},
            "no textual status was emitted\n",
        )
        self.assertEqual(classification, "tconf")
        self.assertEqual(counts["skipped"], 1)

    def test_signaled_ltp_pan_child_is_broken(self) -> None:
        classification, counts = classify_case(
            {"result": "run", "exit": "11", "ltp_stat": "11", "termination": "signaled"},
            "",
        )
        self.assertEqual(classification, "broken")
        self.assertEqual(counts["broken"], 1)


class ResumeTests(unittest.TestCase):
    """验证断点查找和结果去重。"""

    def test_first_gap_is_resumed(self) -> None:
        self.assertEqual(first_missing_index(5, [0, 1, 3, 4]), 2)
        self.assertEqual(first_missing_index(2, [0, 1]), 2)

    def test_retry_does_not_duplicate_result(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.jsonl"
            record = {
                "group": "default",
                "scenario": "syscalls",
                "index": 0,
                "classification": "pass",
            }
            self.assertEqual(len(append_unique_results(path, [record])), 1)
            self.assertEqual(append_unique_results(path, [record]), [])
            self.assertEqual(len(load_jsonl(path)), 1)


if __name__ == "__main__":
    unittest.main()
