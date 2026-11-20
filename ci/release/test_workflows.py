"""Publication consumes this run's artifacts only after every explicit CI branch passes."""

import copy
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
ARCHITECTURES = {"amd64", "arm64"}
POLICIES = {"priority", "drf"}
VERIFY = {"verify-image-publishing", "verify-scripts", "verify-rust", "verify-python", "verify-codeql-python", "verify-codeql-rust"}
BUILDS = {f"build-{kind}-{arch}" for kind in ("images", "binaries") for arch in ARCHITECTURES}
POLICY_JOBS = {f"{kind}-{policy}" for kind in ("rust-e2e", "python-e2e", "python-system") for policy in POLICIES}
BENCHMARKS = {"host-shim-benchmark", "cri-shim-benchmark"}
REQUIRED = VERIFY | BUILDS | POLICY_JOBS | BENCHMARKS | {"kubernetes-e2e", "host-shim-e2e", "cri-shim-e2e"}
PUBLISHERS = {"publish-images", "publish-package"}


def workflow(name="e2e.yaml"):
    # BaseLoader preserves the Actions key `on`, rather than YAML 1.1's boolean.
    return yaml.load((WORKFLOWS / name).read_text(), Loader=yaml.BaseLoader)


def dependencies(job):
    needs = job.get("needs", [])
    return [needs] if isinstance(needs, str) else needs


def prerequisites(jobs, name):
    needs = dependencies(jobs[name])
    ancestors = set(needs)
    for dependency in needs:
        ancestors.update(prerequisites(jobs, dependency))
    return ancestors


class MainPublicationTest(unittest.TestCase):
    def setUp(self):
        self.main = workflow()
        self.jobs = self.main["jobs"]

    def test_flat_main_has_every_required_branch(self):
        self.assertEqual(set(self.jobs), REQUIRED | PUBLISHERS)
        for name, job in self.jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("strategy", job, "CI jobs must not wait on matrix joins")
                self.assertNotIn("uses", job, "CI jobs must execute directly without reusable-job joins")
                self.assertTrue(job.get("steps"))
                self.assertNotIn("matrix.", str(job))
        for filename in ("code-verify.yaml", "image-publish.yaml", "latest-release.yaml"):
            self.assertFalse((WORKFLOWS / filename).exists(), "obsolete workflow entry points must stay removed")

    def test_both_publishers_require_every_verification_and_test_branch(self):
        for name in PUBLISHERS:
            with self.subTest(publisher=name):
                job = self.jobs[name]
                self.assertEqual(set(dependencies(job)), BENCHMARKS)
                self.assertEqual(prerequisites(self.jobs, name), REQUIRED)
                self.assertEqual(job["if"], "github.ref == 'refs/heads/main' && github.event_name != 'pull_request'")
                self.assertNotIn("always()", job["if"])
                self.assertNotIn("continue-on-error", job)
                self.assertEqual(job["env"]["SOURCE_SHA"], "${{ github.sha }}")
        for name in REQUIRED:
            self.assertNotIn("if", self.jobs[name], f"{name} must succeed, not be conditionally skipped")
            self.assertNotIn("continue-on-error", self.jobs[name])

    def test_cutting_any_required_branch_is_detected(self):
        # Exercise the coverage guard against every possible lost prerequisite,
        # including verification, arm64 builds and each scheduler policy.
        for branch in REQUIRED:
            jobs = copy.deepcopy(self.jobs)
            for job in jobs.values():
                if branch in dependencies(job):
                    job["needs"] = [name for name in dependencies(job) if name != branch]
            for publisher in PUBLISHERS:
                with self.subTest(branch=branch, publisher=publisher):
                    self.assertNotEqual(prerequisites(jobs, publisher), REQUIRED)

    def test_dependencies_name_immediate_predecessors_only(self):
        for name, job in self.jobs.items():
            needs = set(dependencies(job))
            for dependency in needs:
                with self.subTest(job=name, dependency=dependency):
                    self.assertFalse(needs & prerequisites(self.jobs, dependency), "do not repeat ancestor dependencies")

    def test_main_is_the_only_push_and_pr_entry_point(self):
        self.assertEqual(set(self.main["on"]), {"push", "pull_request", "workflow_dispatch"})
        self.assertEqual(self.main["on"]["push"]["branches"], ["main"])
        for filename in WORKFLOWS.glob("*.yaml"):
            if filename.name == "e2e.yaml":
                continue
            with self.subTest(workflow=filename.name):
                events = workflow(filename.name)["on"]
                self.assertNotIn("push", events)
                self.assertNotIn("pull_request", events)

    def test_builds_wait_for_all_explicit_verification_jobs(self):
        for name in BUILDS:
            with self.subTest(job=name):
                self.assertEqual(set(dependencies(self.jobs[name])), VERIFY)
        for name in VERIFY:
            self.assertEqual(dependencies(self.jobs[name]), [])

    def test_both_scheduler_policies_run_in_each_test_phase(self):
        for kind in ("rust-e2e", "python-e2e", "python-system"):
            for policy in POLICIES:
                job = self.jobs[f"{kind}-{policy}"]
                self.assertEqual(job["env"]["CI_POLICY"], policy)
                self.assertIn("env.CI_POLICY", str(job["steps"]))
        expected = {f"{kind}-{policy}" for kind in ("rust-e2e", "python-e2e") for policy in POLICIES} | {"kubernetes-e2e", "host-shim-e2e", "cri-shim-e2e"}
        for policy in POLICIES:
            self.assertEqual(set(dependencies(self.jobs[f"python-system-{policy}"])), expected)
        for name in BENCHMARKS:
            self.assertEqual(set(dependencies(self.jobs[name])), {f"python-system-{policy}" for policy in POLICIES})

    def test_codeql_languages_remain_explicit_and_weekly_scan_is_separate(self):
        weekly = workflow("codeql.yaml")
        self.assertEqual(set(weekly["on"]), {"schedule"})
        self.assertTrue(weekly["on"]["schedule"])
        self.assertEqual(set(weekly["jobs"]), {"python", "rust"})
        for language in ("python", "rust"):
            for job in (self.jobs[f"verify-codeql-{language}"], weekly["jobs"][language]):
                self.assertNotIn("strategy", job)
                self.assertNotIn("uses", job)
                self.assertEqual(job["permissions"], {"actions": "read", "contents": "read", "security-events": "write"})
                self.assertEqual(job["steps"][0]["uses"], "actions/checkout@v7")
                action = next(step for step in job["steps"] if step.get("uses") == "./.github/actions/codeql")
                self.assertEqual(action["with"]["language"], language)
        action = yaml.load((ROOT / ".github/actions/codeql/action.yaml").read_text(), Loader=yaml.BaseLoader)
        self.assertEqual(action["runs"]["using"], "composite")
        self.assertEqual(action["inputs"]["language"]["required"], "true")
        self.assertEqual(action["runs"]["steps"][0]["with"], {"languages": "${{ inputs.language }}", "build-mode": "none"})
        self.assertEqual(action["runs"]["steps"][1]["with"]["category"], "/language:${{ inputs.language }}")

    def test_publishers_use_the_same_source_guard_and_current_run_artifacts(self):
        guards = []
        for name in PUBLISHERS:
            steps = self.jobs[name]["steps"]
            self.assertEqual(steps[0]["name"], "Verify the source is this CI run")
            guards.append(steps[0]["run"])
            downloads = []
            for step in steps:
                self.assertNotIn("docker/bake-action", step.get("uses", ""))
                for build in ("scripts/build.sh", "cargo build", "docker build ", "docker buildx build", "docker buildx bake", "uv build"):
                    self.assertNotIn(build, step.get("run", ""))
                if step.get("uses", "").startswith("actions/download-artifact@"):
                    downloads.append(step["with"]["name"])
                    self.assertNotIn("run-id", step["with"])
                    self.assertNotIn("repository", step["with"])
            expected = {f"{'flame-images' if name == 'publish-images' else 'latest'}-{arch}" for arch in ARCHITECTURES}
            self.assertEqual(set(downloads), expected)
        self.assertEqual(guards[0], guards[1])

    def test_publishing_steps_validate_before_mutating_outputs(self):
        images = [step["name"] for step in self.jobs["publish-images"]["steps"]]
        ordered = ["Download tested amd64 images", "Publish tested amd64 images", "Clear transferred amd64 images", "Download tested arm64 images", "Publish tested arm64 images", "Publish and verify manifests", "Check main has not moved on", "Promote verified manifests"]
        self.assertEqual([name for name in images if name in ordered], ordered)
        package = [step["name"] for step in self.jobs["publish-package"]["steps"]]
        ordered = ["Download amd64 assets", "Download arm64 assets", "Verify complete release assets", "Skip when main has moved on", "Point the latest tag at this commit", "Publish the release"]
        self.assertEqual([name for name in package if name in ordered], ordered)
        for step in self.jobs["publish-package"]["steps"]:
            if step["name"] in {"Point the latest tag at this commit", "Publish the release"}:
                self.assertEqual(step["if"], "steps.head.outputs.current == 'true'")

    def test_both_architectures_build_once_without_registry_writes(self):
        for kind in ("images", "binaries"):
            for arch in ARCHITECTURES:
                job = self.jobs[f"build-{kind}-{arch}"]
                self.assertEqual(job["env"]["CI_ARCH"], arch)
                expected = f"ubuntu-{'24.04' if kind == 'binaries' else '26.04'}{'-arm' if arch == 'arm64' else ''}"
                self.assertEqual(job["runs-on"], expected)
                for step in job["steps"]:
                    self.assertNotEqual(step.get("with", {}).get("push"), "true")
                    self.assertNotIn("docker push", step.get("run", ""))
                    self.assertNotIn("gh release", step.get("run", ""))
                if kind == "binaries":
                    builds = [step["run"] for step in job["steps"] if "scripts/build.sh" in step.get("run", "")]
                    self.assertEqual(builds, ["scripts/build.sh --output flame-package --tag latest"])

    def test_image_tests_consume_the_saved_amd64_artifact(self):
        for arch in ARCHITECTURES:
            uploads = [step for step in self.jobs[f"build-images-{arch}"]["steps"] if step.get("uses", "").startswith("actions/upload-artifact@")]
            self.assertTrue(any(step["with"]["name"] == "flame-images-${{ env.CI_ARCH }}" for step in uploads))
        consumers = []
        for name in REQUIRED - VERIFY - BUILDS:
            for step in self.jobs[name]["steps"]:
                if step.get("uses", "").startswith("actions/download-artifact@"):
                    artifact = step["with"]["name"]
                    self.assertNotEqual(artifact, "flame-images")
                    if artifact == "flame-images-amd64":
                        consumers.append(name)
        self.assertEqual(set(consumers), {f"{kind}-{policy}" for kind in ("rust-e2e", "python-e2e", "python-system") for policy in POLICIES} | {"kubernetes-e2e", "host-shim-benchmark"})


if __name__ == "__main__":
    unittest.main()
