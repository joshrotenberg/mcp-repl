"""Keep the deadline checks attached to the workflow publication boundaries."""

from pathlib import Path
import re
import unittest


WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"
GUARD = "python3 .release-policy/scripts/check_advisory_expiry.py --source-root "


def job(file, name):
    source = (WORKFLOWS / file).read_text()
    match = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [\w-]+:|\Z)", source, re.M | re.S)
    assert match, f"missing job {name}"
    return match[1]


class WorkflowExpiryTests(unittest.TestCase):
    def test_release_source_cannot_write_workflow_caches(self):
        for file in ["release.yml", "container-build.yml"]:
            with self.subTest(file=file):
                source = (WORKFLOWS / file).read_text()
                # Enforce the native token restriction with no job override.
                self.assertEqual(re.findall(r"^[ \t]*cache-mode:.*$", source, re.M),
                                 ["cache-mode: read"])
        self.assertIn("save-if: false", job("release.yml", "build-local-artifacts"))

    def test_each_publication_command_has_a_fresh_guard(self):
        for file, name, command, root in [
            ("release.yml", "plan", "          dist ${{", "."),
            ("release.yml", "host", "          dist host ", "."),
            ("release.yml", "host", "          gh release create ", "."),
            ("release.yml", "publish-homebrew-formula", "          git push", "release-source"),
            ("container-build.yml", "publish", "            skopeo copy ", "."),
        ]:
            with self.subTest(file=file, job=name, command=command):
                section = job(file, name)
                steps = re.split(r"^      - ", section, flags=re.M)
                step = next(step for step in steps if command in step)
                self.assertIn(GUARD + root, step[:step.index(command)])
                self.assertIn("ref: main\n          path: .release-policy", section)

    def test_action_publications_have_immediately_preceding_guard(self):
        for file, name, action in [
            ("release.yml", "host", "uses: actions/attest@"),
            ("release-plz.yml", "release", "uses: release-plz/action@"),
        ]:
            with self.subTest(file=file):
                steps = re.split(r"^      - ", job(file, name), flags=re.M)
                index = next(i for i, step in enumerate(steps) if action in step)
                self.assertIn("run: " + GUARD + ".", steps[index - 1])

    def test_native_jobs_use_one_resolved_tag_source(self):
        plan = job("release.yml", "plan")
        self.assertIn("format('refs/tags/{0}', inputs.tag)", plan)
        self.assertIn('echo "sha=$(git rev-parse HEAD)"', plan)
        self.assertIn("github.ref != 'refs/heads/main'", plan)
        for name in ["build-local-artifacts", "build-global-artifacts", "host", "publish-homebrew-formula"]:
            with self.subTest(job=name):
                self.assertIn("ref: ${{ needs.plan.outputs.source-sha }}", job("release.yml", name))
        self.assertIn('RELEASE_COMMIT: "${{ needs.plan.outputs.source-sha }}"', job("release.yml", "host"))
        self.assertIn('--ref main --field "tag=$RELEASE_TAG"', job("release-plz.yml", "release"))

    def test_container_build_is_read_only_and_publication_checks_each_tag(self):
        prepare = job("container-build.yml", "prepare")
        self.assertIn("push: false", prepare)
        self.assertNotIn("packages: write", prepare)
        self.assertNotIn("docker://", prepare)
        self.assertIn("provenance: mode=max", prepare)
        self.assertIn("sbom: true", prepare)
        publish = job("container-build.yml", "publish")
        self.assertIn("needs: prepare", publish)
        self.assertIn("if: inputs.push", publish)
        self.assertIn("ref: ${{ needs.prepare.outputs.sha }}", publish)
        loop = publish.split("while IFS= read -r image_tag; do", 1)[1].split("done <<<", 1)[0]
        self.assertIn(GUARD + ".", loop)
        self.assertIn("skopeo copy --all --preserve-digests", loop)


if __name__ == "__main__":
    unittest.main()
