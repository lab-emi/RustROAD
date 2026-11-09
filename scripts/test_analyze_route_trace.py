"""Sub-kilobyte ledger controls; no graph, PDK, or solver allocations."""
import json
from pathlib import Path
import tempfile
import unittest

from analyze_route_trace import analyze


class LedgerTests(unittest.TestCase):
    def run_trace(self, records):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "trace"
            path.write_text("".join(prefix + json.dumps(row) + "\n" for prefix, row in records))
            return analyze(path)

    def round(self, **changes):
        return dict(round=1, net=2, queue_before=3, queue_after=4, newly_queued=2,
                    victims=[4, 5], expansions=10, comparisons=20,
                    wire_length_dbu=100, vias=1) | changes

    def test_queue_conservation_and_repeated_pending_victim(self):
        result = self.run_trace([
            ("[troute-round] ", self.round()),
            ("[troute-round] ", self.round(round=2, queue_before=4, queue_after=3,
                                           newly_queued=0, victims=[5])),
        ])
        self.assertEqual(result["queue_delta"], 0)
        self.assertEqual(result["avg_victims"], 1.5)
        self.assertTrue(result["ledger_complete"])

    def test_broken_balance_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "queue conservation"):
            self.run_trace([("[troute-round] ", self.round(queue_after=8))])

    def test_unknown_wire_length_remains_unknown(self):
        result = self.run_trace([("[troute-round] ", self.round(wire_length_dbu=None))])
        self.assertIsNone(result["routed_wire_length_dbu"])

    def test_duplicate_round_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "duplicated"):
            self.run_trace([("[troute-round] ", self.round())] * 2)

    def test_search_overrun_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "search exceeded"):
            self.run_trace([("[troute-search] ", dict(
                round=1, owner=None, stage=0, status="limited", cap=10, expansions=11))])

    def test_legacy_missing_events_do_not_become_zeroes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "trace"
            path.write_text("[troute] round 2 net 7 soft victims [8, 9] work 5 exp 6\n")
            result = analyze(path)
        self.assertEqual(result["legacy_soft_records"], 1)
        self.assertFalse(result["ledger_complete"])
        self.assertNotIn("queue_delta", result)


if __name__ == "__main__":
    unittest.main()
