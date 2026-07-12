#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 DESTINATION" >&2
  exit 2
fi

destination=$1
if [[ -e "$destination" ]]; then
  echo "destination already exists: $destination" >&2
  exit 1
fi

mkdir -p "$destination/tests"
: > "$destination/tests/__init__.py"
cat > "$destination/session_report.py" <<'PY'
def summarize(executions):
    """Return stable outcome counts for serialized Loomterm executions."""
    summary = {"passed": 0, "failed": 0, "cancelled": 0, "interrupted": 0}
    for execution in executions:
        state = execution["state"]
        if state == "finished":
            summary["passed"] += 1
        elif state == "cancelled":
            summary["cancelled"] += 1
        elif state == "interrupted":
            summary["interrupted"] += 1
    return summary
PY

cat > "$destination/tests/test_session_report.py" <<'PY'
import unittest

from session_report import summarize


class SessionReportTests(unittest.TestCase):
    def test_classifies_terminal_outcomes(self):
        executions = [
            {"state": "finished", "outcome": {"kind": "exited", "code": 0}},
            {"state": "finished", "outcome": {"kind": "exited", "code": 7}},
            {"state": "finished", "outcome": {"kind": "signaled", "signal": 9}},
            {"state": "cancelled", "outcome": {"kind": "cancelled", "signal": 15}},
            {"state": "interrupted", "outcome": {"kind": "interrupted"}},
        ]

        self.assertEqual(
            summarize(executions),
            {"passed": 1, "failed": 2, "cancelled": 1, "interrupted": 1},
        )


if __name__ == "__main__":
    unittest.main()
PY

cat > "$destination/handoff_worker.py" <<'PY'
import signal
import sys
import time


def stop(signum, _frame):
    print(f"handoff-worker: cancellation signal {signum}", flush=True)
    raise SystemExit(128 + signum)


signal.signal(signal.SIGTERM, stop)
print("handoff-worker: started by Codex", flush=True)
for checkpoint in range(1, 151):
    print(f"handoff-worker: checkpoint {checkpoint:03d}", flush=True)
    time.sleep(2)

print("handoff-worker: completed", flush=True)
PY

cat > "$destination/README.md" <<'EOF'
# Session report fixture

`session_report.summarize` classifies serialized terminal executions. Fix the
implementation so the existing test passes without changing the public return
shape or the test.

`handoff_worker.py` is a deterministic long-running process used to demonstrate
execution handoff between coding-agent sessions.
EOF

git -C "$destination" init -q
git -C "$destination" add .
git -C "$destination" -c user.name="Loomterm Demo" \
  -c user.email="demo@localhost" commit -qm "test: add failing outcome classifier"
echo "$destination"
