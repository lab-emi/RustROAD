#!/usr/bin/env python3
"""Bounded, read-only routing ledger; a partial trace never establishes signoff."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re

MAX_BYTES = 64 << 20
MAX_LINE = 1 << 20
MAX_ROUNDS = 200_000
PROGRESS = re.compile(r"repair progress ([\d.]+)s .* rounds (\d+) queued (\d+) work (\d+) exp (\d+)")
LEGACY = re.compile(r"round (\d+) net (\d+) soft victims \[([\d, ]*)\]")


def analyze(path, first=1, last=MAX_ROUNDS):
    if path.stat().st_size > MAX_BYTES or not 1 <= first <= last <= MAX_ROUNDS:
        raise ValueError("trace byte/round budget exceeded")
    digest = hashlib.sha256()
    progress = []
    rounds = []
    legacy = {}
    searches = Counter()
    observed_last = 0
    consumed = 0
    with path.open("rb") as stream:
        while raw := stream.readline(MAX_LINE + 1):
            consumed += len(raw)
            if len(raw) > MAX_LINE or consumed > MAX_BYTES:
                raise ValueError("trace line/byte budget exceeded")
            digest.update(raw)
            line = raw.decode("utf-8", errors="strict")
            match = PROGRESS.search(line)
            if match:
                elapsed, rnd, queue, work, expansions = match.groups()
                if len(progress) >= MAX_ROUNDS:
                    raise ValueError("progress record budget exceeded")
                progress.append(dict(seconds=float(elapsed), round=int(rnd),
                                     pending_after_pop=int(queue), comparisons=int(work),
                                     expansions=int(expansions)))
            if line.startswith("[troute-search] "):
                record = json.loads(line.split(" ", 1)[1])
                if first <= record["round"] <= last and record["owner"] is None:
                    key = f'stage{record["stage"]}_{record["status"]}'
                    searches[key] += 1
                    searches[f'stage{record["stage"]}_expansions'] += record["expansions"]
                    if record["expansions"] > record["cap"]:
                        raise ValueError("search exceeded its declared cap")
            if line.startswith("[troute-round] "):
                record = json.loads(line.split(" ", 1)[1])
                rnd = record["round"]
                if rnd <= observed_last or rnd > MAX_ROUNDS:
                    raise ValueError("rounds are duplicated, unordered, or beyond budget")
                observed_last = rnd
                if record["queue_after"] != record["queue_before"] - 1 + record["newly_queued"]:
                    raise ValueError("queue conservation failed")
                if not 0 <= record["newly_queued"] <= len(record["victims"]):
                    raise ValueError("enqueue/victim accounting failed")
                if first <= rnd <= last:
                    rounds.append(record)
            match = LEGACY.search(line)
            if match and first <= int(match[1]) <= last:
                rnd = int(match[1])
                if rnd in legacy or len(legacy) >= MAX_ROUNDS:
                    raise ValueError("duplicate legacy round or record budget exceeded")
                legacy[rnd] = len(match[3].split(",")) if match[3] else 0
    values = [len(r["victims"]) for r in rounds] if rounds else list(legacy.values())
    result = dict(schema="rustroad.route-ledger.v1", source=str(path.resolve()),
                  sha256=digest.hexdigest(), scope="routing diagnostic; signoff unchecked",
                  analysis_scope=dict(corner=None, wire_model="geometry_only_no_extraction",
                                      unconstrained_endpoints=None, timing_status="unchecked"),
                  requested_rounds=[first, last], progress=progress,
                  observed_round_records=len(rounds), legacy_soft_records=len(legacy),
                  search_stages=dict(searches), victim_histogram=dict(sorted(Counter(values).items())),
                  avg_victims=sum(values) / len(values) if values else None,
                  excess_victims=sum(max(0, v - 1) for v in values),
                  ledger_complete=bool(rounds) and all(
                      b["round"] == a["round"] + 1 for a, b in zip(rounds, rounds[1:])))
    if rounds:
        result.update(first_observed_round=rounds[0]["round"], last_observed_round=rounds[-1]["round"],
                      queue_before=rounds[0]["queue_before"], queue_after=rounds[-1]["queue_after"],
                      queue_delta=sum(r["queue_after"] - r["queue_before"] for r in rounds),
                      expansions=sum(r["expansions"] for r in rounds),
                      comparisons=sum(r["comparisons"] for r in rounds),
                      routed_wire_length_dbu=(sum(r["wire_length_dbu"] for r in rounds)
                                              if all(isinstance(r["wire_length_dbu"], int) for r in rounds)
                                              else None),
                      routed_vias=sum(r["vias"] for r in rounds))
    else:
        result["limitation"] = "legacy trace omits hard successes and actual enqueues; no exact queue balance or stage-cap rate"
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", type=Path)
    parser.add_argument("--first", type=int, default=1)
    parser.add_argument("--last", type=int, default=MAX_ROUNDS)
    args = parser.parse_args()
    print(json.dumps(analyze(args.trace, args.first, args.last), indent=2))
