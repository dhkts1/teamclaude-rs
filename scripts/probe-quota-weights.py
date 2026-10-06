#!/usr/bin/env python3 -I
"""How many tokens of each kind move the 5h utilization header by one percent.

The header (and the OAuth usage endpoint) report whole percents, so a single
request cannot be priced from them. This sends ONE kind of token at a time on
ONE account until the header has ticked up `--ticks` times past its first
change, and reports tokens per tick for that kind. The first segment (from the
starting reading to the first tick) is discarded: its starting point is
somewhere inside a percent.

Kinds, each a request shape whose usage block is almost entirely that kind:
  i   uncached input: a fresh random prefix, no cache_control
  c5  5m cache write: a fresh random prefix with cache_control (default ttl)
  c1  1h cache write: a fresh random prefix with cache_control ttl=1h
  r   cache read: one prefix written once, then re-sent unchanged
  o   output: a short prompt asking for a long answer, max_tokens high

Run it on an account tcr will not route onto meanwhile (a reserved group, or
one parked), or other sessions' tokens land in the count. Prints no token.
Stops on any non-200, so a 429 ends the run rather than hammering upstream.

usage: python3 -I scripts/probe-quota-weights.py <account-name>
         [--kinds c5,c1,r,i,o] [--prefix-words 40000] [--ticks 2] [--max-requests 80]
"""

import argparse
import json
import os
import random
import sys
import time
import urllib.request

# What Claude Code 2.1.291 sends (strings of its binary): an OAuth token on
# /v1/messages is refused with a header-less 429 without this identity.
BETA = "claude-code-20250219,oauth-2025-04-20"
VERSION = "2023-06-01"
USER_AGENT = "claude-cli/2.1.291 (external, cli)"
CC_SYSTEM = "You are Claude Code, Anthropic's official CLI for Claude."
MODEL = "claude-opus-5-5"
UTIL = "anthropic-ratelimit-unified-5h-utilization"
VOCAB = ["alpha", "bravo", "cedar", "delta", "ember", "fjord", "gamma", "haze",
         "iris", "jade", "kelp", "lumen", "moss", "nadir", "ochre", "pike",
         "quill", "rune", "slate", "tarn", "umber", "vale", "wren", "yarn"]


def token_for(name):
    with open(os.path.expanduser("~/.config/teamclaude.json")) as f:
        cfg = json.load(f)
    for a in cfg["accounts"]:
        if a["name"] == name:
            return a["accessToken"]
    sys.exit(f"no account named {name}")


def post(tok, body):
    req = urllib.request.Request("https://api.anthropic.com/v1/messages", method="POST")
    req.add_header("Authorization", f"Bearer {tok}")
    req.add_header("anthropic-beta", BETA)
    req.add_header("anthropic-version", VERSION)
    req.add_header("user-agent", USER_AGENT)
    req.add_header("content-type", "application/json")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    data = json.dumps(body).encode()
    try:
        with opener.open(req, data=data, timeout=300) as r:
            return r.status, dict(r.headers), json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), json.loads(e.read() or b"{}")


def prefix(words, seed):
    rnd = random.Random(seed)
    return " ".join(rnd.choice(VOCAB) for _ in range(words))


def body_for(kind, words, seed, fixed_prefix):
    system = [{"type": "text", "text": CC_SYSTEM}]
    user = "reply with one word"
    max_tokens = 1
    if kind == "i":
        system.append({"type": "text", "text": prefix(words, seed)})
    elif kind == "c5":
        system.append({"type": "text", "text": prefix(words, seed),
                       "cache_control": {"type": "ephemeral"}})
    elif kind == "c1":
        system.append({"type": "text", "text": prefix(words, seed),
                       "cache_control": {"type": "ephemeral", "ttl": "1h"}})
    elif kind == "r":
        system.append({"type": "text", "text": fixed_prefix,
                       "cache_control": {"type": "ephemeral"}})
    elif kind == "o":
        user = ("Write a long, plain, factual essay about the history of the Roman"
                " aqueducts. Keep going until you are cut off.")
        max_tokens = 4000
    return {"model": MODEL, "max_tokens": max_tokens, "system": system,
            "messages": [{"role": "user", "content": user}]}


def tokens_of(kind, usage):
    cc = usage.get("cache_creation") or {}
    return {
        "i": usage.get("input_tokens", 0),
        "c5": cc.get("ephemeral_5m_input_tokens", 0),
        "c1": cc.get("ephemeral_1h_input_tokens", 0),
        "r": usage.get("cache_read_input_tokens", 0),
        "o": usage.get("output_tokens", 0),
    }[kind]


def run_kind(tok, kind, words, ticks_wanted, max_requests):
    fixed_prefix = prefix(words, seed=424242) if kind == "r" else None
    last_util = None
    segment = {"kind": 0, "all": 0, "requests": 0}
    segments = []
    for n in range(max_requests):
        seed = int(time.time() * 1000) + n
        st, h, j = post(tok, body_for(kind, words, seed, fixed_prefix))
        if st != 200:
            print(f"  {kind}: stop, status={st} body={json.dumps(j)[:200]}")
            break
        util = h.get(UTIL)
        util = float(util) if util is not None else None
        usage = j.get("usage", {})
        this_kind = tokens_of(kind, usage)
        every = (usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0)
                 + usage.get("cache_read_input_tokens", 0) + usage.get("output_tokens", 0))
        if kind == "r" and n == 0:
            # The first read request is the write that seeds the cache.
            print(f"  {kind}: seeded cache, usage={json.dumps(usage)} util={util}")
            last_util = util
            continue
        if last_util is not None and util is not None and util > last_util:
            segments.append(dict(segment, from_util=last_util, to_util=util))
            print(f"  {kind}: tick {last_util} -> {util} after {segment['requests']} requests, "
                  f"{segment['kind']:,} {kind}-tokens ({segment['all']:,} tokens of every kind)")
            segment = {"kind": 0, "all": 0, "requests": 0}
            if len(segments) > ticks_wanted:
                break
        segment["kind"] += this_kind
        segment["all"] += every
        segment["requests"] += 1
        if util is not None:
            last_util = util
        if n % 5 == 0:
            print(f"  {kind}: request {n} util={util} this={this_kind:,} "
                  f"segment={segment['kind']:,}", flush=True)
        time.sleep(0.5)
    usable = segments[1:]  # the first segment started mid-percent
    if usable:
        per_tick = sum(s["kind"] for s in usable) / len(usable)
        print(f"RESULT kind={kind} tokens_per_percent={per_tick:,.0f} "
              f"segments={len(usable)} values={[s['kind'] for s in usable]}")
    else:
        print(f"RESULT kind={kind} tokens_per_percent=unknown segments=0 "
              f"(partial first segment: {segment['kind']:,} {kind}-tokens, no complete tick)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("account")
    ap.add_argument("--kinds", default="c5,c1,r,i,o")
    ap.add_argument("--prefix-words", type=int, default=40000)
    ap.add_argument("--ticks", type=int, default=2)
    ap.add_argument("--max-requests", type=int, default=80)
    args = ap.parse_args()
    tok = token_for(args.account)
    print(f"account={args.account} model={MODEL} prefix_words={args.prefix_words} "
          f"ticks={args.ticks} max_requests={args.max_requests}")
    for kind in args.kinds.split(","):
        print(f"== kind {kind}", flush=True)
        run_kind(tok, kind.strip(), args.prefix_words, args.ticks, args.max_requests)


if __name__ == "__main__":
    main()
