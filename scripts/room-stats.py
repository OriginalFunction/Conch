#!/usr/bin/env python3
"""Score a Conch room for attention discipline.

Reads committed scene files straight from the data directory, so it measures
what the chain actually holds rather than what anyone remembers.

    python3 scripts/room-stats.py <room-id-or-prefix> [--data-dir ~/.conch]

The numbers that matter after 1.3.6:
  * agent takes per human take  - the stampede. Should be near 1.
  * distinct agents per question - should be 1 for most questions.
  * empty takes                  - an agent that took the floor and chose not
                                   to add anything. Zero of these means nobody
                                   is reconsidering.
  * endorsements / restatements  - takes that spend the floor on agreement.
  * handles per take             - the branching factor. Above 1 runs away.
"""

import argparse
import glob
import json
import os
import re
import statistics
import sys
from collections import Counter


def load(room_dir):
    scenes, hash_by_n = {}, {}
    for path in glob.glob(os.path.join(room_dir, "scenes", "*.json")):
        base = os.path.basename(path)[:-5]
        n, _, digest = base.partition("-")
        try:
            scenes[int(n)] = json.load(open(path))
        except (ValueError, json.JSONDecodeError):
            continue
        hash_by_n[int(n)] = digest
    return scenes, {h: n for n, h in hash_by_n.items()}


def takes_of(scenes, n_by_hash):
    grants = {
        n: s["scene"]["body"]["to"]["agent"]
        for n, s in scenes.items()
        if s["scene"]["body"]["type"] == "grant"
    }
    rows = []
    for n in sorted(scenes):
        body = scenes[n]["scene"]["body"]
        if body["type"] != "speech":
            continue
        grant_n = n_by_hash.get(body.get("closes_grant"))
        rows.append(
            {
                "n": n,
                "author": grants.get(grant_n),
                "ts": scenes[n]["scene"]["ts"],
                "text": body.get("text", ""),
                "grant_ts": scenes[grant_n]["scene"]["ts"] if grant_n in scenes else None,
            }
        )
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("room")
    parser.add_argument("--data-dir", default=os.path.expanduser("~/.conch"))
    args = parser.parse_args()

    matches = [
        d
        for d in glob.glob(os.path.join(os.path.expanduser(args.data_dir), "rooms", "*"))
        if os.path.basename(d).startswith(args.room)
    ]
    if len(matches) != 1:
        sys.exit(f"{len(matches)} rooms match {args.room!r}")
    scenes, n_by_hash = load(matches[0])
    if not scenes:
        sys.exit("no committed scenes")
    takes = takes_of(scenes, n_by_hash)
    agent_takes = [t for t in takes if t["author"] and t["author"].startswith("agent:")]
    human_takes = [t for t in takes if t["author"] and t["author"].startswith("human:")]

    print(f"room     {os.path.basename(matches[0])[:12]}  scenes {len(scenes)}  takes {len(takes)}")
    print(f"authors  {dict(Counter(t['author'] for t in takes if t['author']))}")

    bursts, answerers, current = [], [], None
    for take in takes:
        if take["author"] and take["author"].startswith("human:"):
            if current is not None:
                bursts.append(len(current))
                answerers.append(len(set(current)))
            current = []
        elif current is not None:
            current.append(take["author"])
    if current is not None:
        bursts.append(len(current))
        answerers.append(len(set(current)))

    print()
    print("STAMPEDE")
    if bursts:
        print(f"  agent takes per human take   mean {statistics.mean(bursts):.1f}  median {statistics.median(bursts)}  max {max(bursts)}")
        print(f"  distinct agents per question {dict(Counter(answerers))}   (1 is the goal)")
    else:
        print("  no human take opened a burst")

    empty = [t for t in takes if not t["text"].strip()]
    print()
    print("RECONSIDERING")
    print(f"  empty takes (took the floor, added nothing)  {len(empty)} of {len(takes)}")
    latencies = [t["ts"] - t["grant_ts"] for t in takes if t["grant_ts"] is not None]
    if latencies:
        quick = sum(1 for x in latencies if x <= 2)
        print(f"  spoken within 2 s of the grant              {quick} of {len(latencies)}")
        print(f"  grant to speech seconds                     median {statistics.median(latencies)}  max {max(latencies)}")

    patterns = {
        "endorsement (+1 / agree / right)": r"(\+1\b|\bI agree\b|\bconcur\b|\byou(?:'re| are) (?:100% )?right\b|\bgood catch\b)",
        "restates a plan": r"\b(PLAN|RECAST|REVISED PLAN|full state)\b",
        "status recap": r"\b(standing by|no change|still waiting|status update)\b",
    }
    print()
    print("WASTED TAKES")
    for label, pattern in patterns.items():
        hits = sum(1 for t in agent_takes if re.search(pattern, t["text"], re.I))
        share = 100 * hits / len(agent_takes) if agent_takes else 0
        print(f"  {label:34s} {hits:4d}  ({share:.0f}%)")

    handles = [len(set(re.findall(r"@([a-z][\w.:-]*)", t["text"].lower()))) for t in takes]
    agent_handles = [len(set(re.findall(r"@([a-z][\w.:-]*)", t["text"].lower()))) for t in agent_takes]
    print()
    print("BRANCHING")
    if handles:
        print(f"  handles per take        mean {statistics.mean(handles):.2f}  (above 1 runs away)")
    if agent_handles:
        print(f"  handles per agent take  mean {statistics.mean(agent_handles):.2f}")
    multi = sum(1 for h in handles if h >= 3)
    print(f"  takes naming 3+ agents  {multi} of {len(takes)}")

    chars_agent = sum(len(t["text"]) for t in agent_takes)
    chars_human = sum(len(t["text"]) for t in human_takes)
    print()
    print("VOLUME")
    print(f"  characters written by agents {chars_agent:,}")
    print(f"  characters written by humans {chars_human:,}")
    if chars_human:
        print(f"  ratio                        {chars_agent / chars_human:.0f}x")


if __name__ == "__main__":
    main()
