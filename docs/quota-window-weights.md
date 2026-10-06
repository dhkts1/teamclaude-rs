# What a token costs the 5-hour window

Measured 2026-10-06 with `scripts/probe-quota-weights.py` on one Max 20x account
that sat in a reserved group with no other traffic that day. The header
`anthropic-ratelimit-unified-5h-utilization` and the OAuth usage endpoint both
report whole percents, so a single request cannot be priced from them; the probe
sends one kind of token at a time until the header ticks, and counts tokens per
tick. The first segment of each run starts somewhere inside a percent and is
discarded; the figures below are complete percent-to-percent segments.

| token kind | request size | tokens per 1 % of the window |
|---|---|---|
| 5m cache write | ~98k | 1,082,515 · 1,081,558 |
| 5m cache write | ~49k | 1,131,370 · 1,082,532 |
| 1h cache write | ~98k | 1,180,945 · 1,081,806 |
| uncached input | ~98k | 1,180,315 (one segment) |
| cache read | ~98k | more than 23,634,000 (240 requests, no tick) |

Three things follow, each within one request (about 9 %) of resolution.

1. **The window counts tokens, not requests.** Halving the request size doubled
   the requests per tick (11 → 22–23) and left the tokens per tick where they
   were.
2. **A 1h cache write, a 5m cache write and plain uncached input cost the same
   per token.** The API's price ratios (1.25× for a 5m write, 2× for 1h) do not
   describe the subscription window.
3. **A cache read is near-free.** 23.6M read tokens moved the header by less
   than one tick, where 1.08M written tokens move it one; a read weighs under
   1/22 of a write, possibly nothing. The API's 0.1× overstates it.

So the proxy rewrites every `cache_control` breakpoint to `"ttl":"1h"`
(`cacheTtlRewrite`, on by default): the hour costs the window nothing extra and
turns a prefix re-write after a 5–60 minute idle gap into a read. Output tokens
were not measured; `tcr wrap` still prices 1h writes at the API's 2×, so its
dollar figure overstates what the longer window costs the quota.

Re-measure with, on an account the fleet will not route onto meanwhile:

```sh
python3 -I scripts/probe-quota-weights.py <account-name> --kinds c5,c1,i --ticks 2
```

Each kind spends about `ticks + 1` percent of that account's window.
