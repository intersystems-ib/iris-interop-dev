"""Write benchmark results incrementally to JSON and generate HTML report."""
import json
import os
import datetime
from pathlib import Path
from typing import NamedTuple, Optional


class ResultWriter:
    def __init__(self):
        ts = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H-%M-%SZ")
        base = Path(__file__).parent.parent / "results" / ts
        base.mkdir(parents=True, exist_ok=True)
        self.run_dir = str(base)
        self.scores_path = str(base / "scores.json")
        self.report_path = str(base / "report.html")
        version = _get_version()
        self._run = {
            "run_id": ts,
            # null, never a placeholder. A version that could not be read is not a version, and
            # writing "unknown" here put a failure into the report shaped like a fact.
            "iris_dev_version": version.value,
            "iris_dev_version_error": version.detail,
            "tasks": [],
            "summary": {},
        }
        self._flush()

    def record(self, task_id: str, category: str, path: str, harness: str,
               scored: dict, result: dict, condition: str = "baseline"):
        entry = {
            "task_id": task_id,
            "category": category,
            "path": path,
            "harness": harness,
            "condition": condition,
            "score": scored["score"],
            "reasoning": scored.get("reasoning", ""),
            "tool_call_count": result.get("tool_call_count", 0),
            "stub_error_count": result.get("stub_error_count", 0),
            "wrong_tool_count": result.get("wrong_tool_count", 0),
            "scm_elicitation_triggered": _scm_triggered(result.get("transcript", [])),
        }
        self._run["tasks"].append(entry)
        self._flush()

    def set_condition_metadata(self, condition: str, wall_clock_seconds: float):
        self._run["condition"] = condition
        self._run["wall_clock_seconds"] = wall_clock_seconds

    def finalize(self):
        self._run["summary"] = _compute_summary(self._run["tasks"])
        self._flush()
        self._write_html()
        print(f"scores.json  → {self.scores_path}")
        print(f"report.html  → {self.report_path}")

    def _flush(self):
        with open(self.scores_path, "w") as f:
            json.dump(self._run, f, indent=2)

    def _write_html(self):
        from .report import generate_report
        generate_report(self.scores_path, self.report_path)


class Version(NamedTuple):
    """Either a known version, or the reason it could not be read -- never a placeholder.

    `value` is None ONLY when the version is genuinely unknown, and `detail` then says why. The
    previous code returned the string "unknown" for four unrelated causes (binary absent, killed by
    a signal, non-zero exit, empty stdout), which reads in the report as a statement about the
    server rather than a failure to ask it.
    """

    value: Optional[str]
    detail: Optional[str]


def _get_version(timeout: float = 10.0) -> Version:
    """Read `<binary> --version`, or report why it could not be read.

    `timeout` is the point of this signature. The previous call passed none, so a binary that blocks
    blocks the whole run -- and the blanket `except Exception` could not catch that, because a hang
    raises nothing. A stale pre-rename `iris-dev` took minutes under Gatekeeper before being killed.
    """
    import subprocess

    from .binary import BinaryUnavailable, exit_reason, resolve_binary

    try:
        path = resolve_binary()
    except BinaryUnavailable as e:
        return Version(None, str(e))
    try:
        r = subprocess.run(
            [path, "--version"], capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        return Version(None, f"{path} --version did not return within {timeout:g}s")
    except OSError as e:
        return Version(None, f"{path} could not be executed: {e}")
    stderr_tail = ""
    if r.stderr and r.stderr.strip():
        stderr_tail = r.stderr.strip().splitlines()[-1]
    if r.returncode != 0:
        return Version(None, exit_reason(path, r.returncode, stderr_tail))
    fields = r.stdout.split()
    if not fields:
        # Exit 0 with nothing on stdout. The old code indexed [-1] into the empty list, raised
        # IndexError, and the blanket `except Exception` turned that into "unknown" too.
        return Version(None, f"{path} --version exited 0 but printed nothing on stdout")
    return Version(fields[-1], None)


def _scm_triggered(transcript: list) -> bool:
    return any(
        t.get("tool_name") == "iris_source_control" or
        "elicitation" in str(t.get("tool_result", "")).lower()
        for t in transcript
    )


def _compute_summary(tasks: list) -> dict:
    if not tasks:
        return {}

    scores_a = [t["score"] for t in tasks if t["path"] == "A"]
    scores_b = [t["score"] for t in tasks if t["path"] == "B"]

    by_category = {}
    for t in tasks:
        cat = t["category"]
        if cat not in by_category:
            by_category[cat] = {"A": [], "B": []}
        by_category[cat][t["path"]].append(t["score"])

    return {
        "mean_score_path_a": _mean(scores_a),
        "mean_score_path_b": _mean(scores_b),
        "task_count": len(tasks),
        "by_category": {
            cat: {
                "path_a": _mean(v["A"]),
                "path_b": _mean(v["B"]),
            }
            for cat, v in by_category.items()
        },
    }


def _mean(vals: list) -> float:
    return round(sum(vals) / len(vals), 2) if vals else 0.0
