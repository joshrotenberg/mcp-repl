"""Deterministic coverage of the approved Pacific deadline and narrow scope."""

from datetime import datetime, timedelta, timezone
from pathlib import Path
import tempfile
import unittest

from check_advisory_expiry import check_exceptions, check_source


class AdvisoryExpiryTests(unittest.TestCase):
    # Use the approved local calendar date independently of the UTC constant.
    cutoff = datetime(2026, 12, 14, tzinfo=timezone(timedelta(hours=-8)))
    approved = ["RUSTSEC-2026-0370", "RUSTSEC-2026-0371", "RUSTSEC-2026-0372"]

    def test_valid_before_cutoff(self):
        check_exceptions(self.approved, self.cutoff - timedelta(microseconds=1))

    def test_rejects_at_and_after_cutoff_in_either_timezone(self):
        for now in [
            self.cutoff,
            self.cutoff.astimezone(timezone.utc),
            self.cutoff + timedelta(microseconds=1),
            datetime(2026, 12, 15, 4, 21, tzinfo=timezone.utc),
        ]:
            with self.subTest(now=now), self.assertRaisesRegex(ValueError, "expired"):
                check_exceptions(self.approved, now)

    def test_partial_removal_does_not_extend_remaining_exception(self):
        exception = [{"id": self.approved[0], "reason": "tracked in #309"}]
        check_exceptions(exception, self.cutoff - timedelta(seconds=1))
        with self.assertRaisesRegex(ValueError, "expired"):
            check_exceptions(exception, self.cutoff)

    def test_removed_exceptions_pass_after_cutoff(self):
        check_exceptions([], self.cutoff + timedelta(days=365))

    def test_rejects_unapproved_and_duplicate_ids(self):
        for ignored in [self.approved + ["RUSTSEC-2026-0001"], self.approved * 2]:
            with self.subTest(ignored=ignored), self.assertRaises(ValueError):
                check_exceptions(ignored, self.cutoff - timedelta(days=1))

    def test_rejects_malformed_configuration(self):
        for ignored in [None, "RUSTSEC-2026-0370", [None], [{}], [{"id": []}]]:
            with self.subTest(ignored=ignored), self.assertRaises(ValueError):
                check_exceptions(ignored, self.cutoff - timedelta(days=1))

    def test_rejects_naive_time(self):
        with self.assertRaisesRegex(ValueError, "timezone-aware"):
            check_exceptions(self.approved, datetime(2026, 12, 13))

    def test_old_tags_cannot_bypass_expiry_without_ignores_or_guard(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ["zvariant", "zbus_names", "zvariant_derive"]:
                (root / "Cargo.lock").write_text(f'[[package]]\nname = "{name}"\n')
                for policy in [None, "[advisories]\nignore = []\n"]:
                    with self.subTest(name=name, policy=policy):
                        if policy is None:
                            (root / "deny.toml").unlink(missing_ok=True)
                        else:
                            (root / "deny.toml").write_text(policy)
                        check_source(root, self.cutoff - timedelta(seconds=1))
                        with self.assertRaisesRegex(ValueError, "expired"):
                            check_source(root, self.cutoff)

    def test_source_without_affected_packages_or_ignores_passes_after_cutoff(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.lock").write_text('[[package]]\nname = "mcp-repl"\n')
            check_source(root, self.cutoff + timedelta(days=1))

    def test_missing_or_malformed_subject_lockfile_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaises(OSError):
                check_source(root, self.cutoff)
            (root / "Cargo.lock").write_text("invalid TOML")
            with self.assertRaises(ValueError):
                check_source(root, self.cutoff)


if __name__ == "__main__":
    unittest.main()
