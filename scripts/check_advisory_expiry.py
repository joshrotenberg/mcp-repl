"""Enforce the fixed, maintainer-approved exception deadline from issue #309."""

from datetime import datetime, timezone
from pathlib import Path
import sys
import tomllib


ISSUE = "https://github.com/joshrotenberg/mcp-repl/issues/309"
APPROVED = {"RUSTSEC-2026-0370", "RUSTSEC-2026-0371", "RUSTSEC-2026-0372"}
# November 16, 2026 at 00:00 America/Los_Angeles (PST). Never roll forward.
EXPIRES = datetime(2026, 11, 16, 8, tzinfo=timezone.utc)


def check_exceptions(ignored, now):
    """Validate scope and cutoff; tests supply time, production uses the clock."""
    if now.utcoffset() is None:
        raise ValueError("exception checks require a timezone-aware timestamp")
    if not isinstance(ignored, list):
        raise ValueError("advisory ignores must be a list")
    seen = set()
    for entry in ignored:
        advisory = entry.get("id") if isinstance(entry, dict) else entry
        if not isinstance(advisory, str) or advisory not in APPROVED:
            raise ValueError(f"unapproved advisory exception: {advisory!r}; see {ISSUE}")
        if advisory in seen:
            raise ValueError(f"duplicate advisory exception: {advisory}")
        seen.add(advisory)
    if seen and now >= EXPIRES:
        raise ValueError(
            "temporary advisory exceptions expired at 2026-11-16 00:00 PST "
            f"(08:00 UTC); remove them and resolve {ISSUE}"
        )


def main():
    try:
        policy = Path(__file__).resolve().parents[1] / "deny.toml"
        with policy.open("rb") as source:
            ignored = tomllib.load(source)["advisories"].get("ignore", [])
        # No command-line or environment override: CI/publication must use now.
        check_exceptions(ignored, datetime.now(timezone.utc))
    except (OSError, KeyError, ValueError) as error:
        print(f"advisory exception check failed: {error}", file=sys.stderr)
        return 1
    print("advisory exception scope and deadline passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
