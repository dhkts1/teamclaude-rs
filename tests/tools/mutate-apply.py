"""Apply one mutation spec to one file, for `tests/tools/mutate-identity-gates.sh`.

The spec is the OLD text, a line reading `@@@`, then the NEW text. Reading both
from a file rather than from the command line is deliberate: multi-line Rust
carrying quotes, apostrophes and braces does not survive being quoted into a
shell argument, and a mutation that silently failed to apply is how a mutation
test reports success while testing nothing.

Exits non-zero when the anchor is absent, or when it appears more than once :
an ambiguous anchor would mutate a line nobody chose.
"""

import os
import sys

path = os.environ["MUT_PATH"]
spec = os.environ["MUT_SPEC"]

with open(spec, encoding="utf-8") as handle:
    text = handle.read()

parts = text.split("\n@@@\n")
if len(parts) != 2:
    sys.exit(f"spec {spec} does not hold exactly one `@@@` separator")
old, new = parts[0], parts[1]
# The heredoc leaves a trailing newline on the NEW half only.
if new.endswith("\n"):
    new = new[:-1]

with open(path, encoding="utf-8") as handle:
    source = handle.read()

count = source.count(old)
if count != 1:
    sys.exit(f"anchor appears {count} times in {path}: {old[:70]!r}")

with open(path, "w", encoding="utf-8") as handle:
    handle.write(source.replace(old, new, 1))
