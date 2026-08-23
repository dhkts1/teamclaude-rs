#!/usr/bin/env python3
"""End-to-end probe: does `x-tcr-group: <g>` actually route to that group?

Sends real (1-token) /v1/messages requests through the RUNNING proxy, each with a
fresh metadata.user_id so it gets a fresh affinity key (no pre-existing pin), then
attributes each one from the proxy log by finding the session key whose FIRST-EVER
usage line lands inside that probe's window.

Read-only with respect to config: it never writes ~/.config/teamclaude.json and
never signals the proxy.
"""
import json, os, re, subprocess, sys, time, uuid

HAIKU = "claude-haiku-4-5-20251001"

LOG = os.path.expanduser("~/.cache/teamclaude/logs/teamclaude-rs.log.%s"
                         % time.strftime("%Y-%m-%d"))
CA = os.path.expanduser("~/.config/tcr-ca.pem")
PROXY = os.environ.get("HTTPS_PROXY") or "http://127.0.0.1:3456"
USAGE = re.compile(r'^(\S+)Z\s+INFO teamclaude_rs::proxy: request usage '
                   r'account="([^"]+)" session=Some\((\d+)\)')

def known_keys():
    keys = set()
    with open(LOG, errors="ignore") as fh:
        for line in fh:
            m = USAGE.match(line)
            if m:
                keys.add(m.group(3))
    return keys

def attribute(since_iso, before_keys):
    """Accounts that served a session key never seen before `since_iso`."""
    found = []
    with open(LOG, errors="ignore") as fh:
        for line in fh:
            m = USAGE.match(line)
            if not m:
                continue
            ts, acct, key = m.groups()
            if ts >= since_iso and key not in before_keys:
                found.append((ts, acct, key))
    return found

def probe(group, model, label):
    sess = str(uuid.uuid4())
    body = {
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": "user_e2e_account__session_%s" % sess},
    }
    headers = ["-H", "content-type: application/json",
               "-H", "anthropic-version: 2023-06-01"]
    if group:
        headers += ["-H", "x-tcr-group: %s" % group]
    before = known_keys()
    t0 = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
    cmd = (["curl", "-sS", "--max-time", "60", "--proxy", PROXY, "--cacert", CA,
            "https://api.anthropic.com/v1/messages", "-X", "POST"]
           + headers + ["-d", json.dumps(body)])
    out = subprocess.run(cmd, capture_output=True, text=True)
    status = "ok"
    try:
        resp = json.loads(out.stdout)
        if resp.get("type") == "error":
            status = "upstream-error: %s" % resp["error"].get("message", "")[:80]
    except Exception:
        status = "unparsed: %s" % (out.stdout[:80] or out.stderr[:80])
    time.sleep(1.5)          # let the usage line land
    hits = attribute(t0, before)
    served = hits[-1][1] if hits else "UNATTRIBUTED"
    return {"label": label, "group": group, "model": model,
            "served_by": served, "status": status, "new_keys": len(hits)}

def main():
    groups = json.loads(subprocess.run(
        ["tcr", "status", "--json"], capture_output=True, text=True).stdout)
    member = {}
    for a in groups:
        for g in (a.get("groups") or []):
            member.setdefault(g, []).append(a["name"])
    print("group membership per the RUNNING proxy: %s\n" % member)

    plan = [(None, HAIKU, "control: no group header")]
    for g in sorted(member):
        for i in range(3):
            plan.append((g, HAIKU, "group=%s #%d" % (g, i + 1)))

    rows = []
    for group, model, label in plan:
        r = probe(group, model, label)
        expect = member.get(group, []) if group else None
        r["expected"] = expect
        r["verdict"] = ("NO DATA" if r["served_by"] == "UNATTRIBUTED"
                        else "n/a" if expect is None
                        else "IN GROUP" if r["served_by"] in expect else "OFF GROUP")
        rows.append(r)
        print("%-22s served_by=%-26s %-9s %s"
              % (label, r["served_by"], r["verdict"], r["status"]))

    print()
    for g in sorted(member):
        rs = [r for r in rows if r["group"] == g]
        good = sum(1 for r in rs if r["verdict"] == "IN GROUP")
        nodata = sum(1 for r in rs if r["verdict"] == "NO DATA")
        scored = len(rs) - nodata
        print("group %-12s %d/%d attributed probes landed in the group%s (members: %s)"
              % (g, good, scored, " [%d unattributed]" % nodata if nodata else "",
                 ", ".join(member[g])))
    json.dump(rows, open("/tmp/group-routing-e2e.json", "w"), indent=2)

if __name__ == "__main__":
    main()
