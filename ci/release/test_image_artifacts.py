"""Exercise build-artifact provenance and publication without Docker or a registry."""

import copy
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("image_artifacts", Path(__file__).with_name("image-artifacts.py"))
ARTIFACTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ARTIFACTS)
SHA = "a" * 40
REGISTRY = "docker.io/xflops"


class ImageArtifactsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.calls = []
        self.refs = {}
        self.fail_smoke = False
        self.omit_digest = False
        for index, image in enumerate(ARTIFACTS.IMAGES):
            ref = f"{REGISTRY}/{ARTIFACTS.repository(image)}:{ARTIFACTS.tag(image, 'ci', 'amd64')}"
            self.refs[ref] = {
                "Id": f"sha256:{index + 1:064x}",
                "Architecture": "amd64",
                "Os": "linux",
                "Config": {"Labels": {"org.opencontainers.image.revision": SHA}},
            }
        self.addCleanup(patch.stopall)
        patch.object(ARTIFACTS.subprocess, "run", side_effect=self.docker).start()
        patch.object(ARTIFACTS, "pipeline", side_effect=self.pipeline).start()

    def docker(self, args, **kwargs):
        self.calls.append(args)
        stdout = ""
        if args[:3] == ["docker", "image", "inspect"]:
            stdout = json.dumps([self.refs[ref] for ref in args[3:]])
        elif args[:2] == ["docker", "tag"]:
            self.refs[args[3]] = copy.deepcopy(self.refs[args[2]])
        elif args[:2] == ["docker", "push"] and not self.omit_digest:
            repo = args[2].rsplit(":", 1)[0]
            self.refs[args[2]]["RepoDigests"] = [f"{repo}@sha256:{'b' * 64}"]
        elif args[:2] == ["docker", "run"] and self.fail_smoke:
            raise subprocess.CalledProcessError(1, args)
        return subprocess.CompletedProcess(args, 0, stdout=stdout)

    def pipeline(self, producer, consumer, output=None):
        self.calls.append(producer)
        self.calls.append(consumer)
        if output is not None:
            output.write(b"exact native image bytes")

    def save(self):
        ARTIFACTS.save(REGISTRY, "amd64", SHA, self.directory)
        self.calls.clear()

    def write_manifest(self, manifest):
        (self.directory / "images.json").write_text(json.dumps(manifest))

    def manifest(self):
        return json.loads((self.directory / "images.json").read_text())

    def assert_no_push(self):
        self.assertFalse(any(call[:2] == ["docker", "push"] for call in self.calls), self.calls)

    def test_save_archives_all_variants_and_compatibility_aliases_without_pushing(self):
        ARTIFACTS.save(REGISTRY, "amd64", SHA, self.directory)
        manifest = self.manifest()
        self.assertEqual(manifest["source_sha"], SHA)
        self.assertEqual(set(manifest["images"]), set(ARTIFACTS.IMAGES))
        self.assertEqual(manifest["archive_sha256"], ARTIFACTS.checksum(self.directory / "flame-images.tar.zst"))
        saved = next(call for call in self.calls if call[:2] == ["docker", "save"])
        for item in manifest["images"].values():
            self.assertIn(item["ref"], saved)
        for ref in ["xflops/flmrt:ci", "xflops/flmrt:ci-devel", f"{REGISTRY}/flame-console:ci", f"{REGISTRY}/flame-test:ci"]:
            self.assertIn(ref, saved)
        self.assertGreaterEqual(sum(call[:2] == ["docker", "run"] for call in self.calls), 8)
        self.assert_no_push()

    def test_smoke_failure_does_not_create_an_artifact(self):
        self.fail_smoke = True
        with self.assertRaises(subprocess.CalledProcessError):
            ARTIFACTS.save(REGISTRY, "amd64", SHA, self.directory)
        self.assertFalse((self.directory / "images.json").exists())
        self.assertFalse((self.directory / "flame-images.tar.zst").exists())
        self.assert_no_push()

    def test_save_rejects_wrong_revision_or_architecture_before_any_smoke(self):
        last = list(self.refs)[-1]
        for field, value in [("Architecture", "arm64"), ("Os", "windows"), ("Config", {"Labels": {"org.opencontainers.image.revision": "c" * 40}})]:
            with self.subTest(field=field):
                original = self.refs[last][field]
                self.refs[last][field] = value
                with self.assertRaises(ValueError):
                    ARTIFACTS.save(REGISTRY, "amd64", SHA, self.directory)
                self.refs[last][field] = original
        self.assertFalse(any(call[:2] == ["docker", "run"] for call in self.calls))
        self.assert_no_push()

    def test_push_reuses_saved_ids_and_outputs_all_native_digests_without_building(self):
        self.save()
        ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
        digests = json.loads((self.directory / "digests-amd64.json").read_text())
        self.assertEqual(set(digests), set(ARTIFACTS.IMAGES))
        pushes = [call[2] for call in self.calls if call[:2] == ["docker", "push"]]
        self.assertEqual(len(pushes), 8)
        self.assertIn(f"{REGISTRY}/flame-executor-manager:sha-{SHA}-devel-amd64", pushes)
        self.assertIn(["docker", "load"], self.calls)
        self.assertFalse(any(call[:2] in (["docker", "build"], ["docker", "buildx"], ["docker", "run"]) for call in self.calls))
        first_push = next(i for i, call in enumerate(self.calls) if call[:2] == ["docker", "push"])
        self.assertTrue(any(call[:3] == ["docker", "image", "inspect"] and len(call) == 11 for call in self.calls[:first_push]))

    def test_wrong_artifact_revision_architecture_or_incomplete_images_never_push(self):
        self.save()
        original = self.manifest()
        invalid = [
            {**original, "source_sha": "c" * 40},
            {**original, "architecture": "arm64"},
            {**original, "images": {key: value for key, value in original["images"].items() if key != ARTIFACTS.IMAGES[-1]}},
        ]
        for manifest in invalid:
            with self.subTest(manifest=manifest):
                self.write_manifest(manifest)
                with self.assertRaises(ValueError):
                    ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
        self.assert_no_push()
        self.assertFalse(any(call[:2] == ["docker", "load"] for call in self.calls))

    def test_corrupt_archive_never_loads_or_pushes(self):
        self.save()
        (self.directory / "flame-images.tar.zst").write_bytes(b"changed bytes")
        with self.assertRaisesRegex(ValueError, "checksum"):
            ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
        self.assertEqual(self.calls, [])

    def test_loaded_id_revision_or_architecture_mismatch_rejects_entire_set_before_push(self):
        self.save()
        last = list(self.refs)[7]
        original = copy.deepcopy(self.refs[last])
        for field, value in [("Id", "sha256:" + "f" * 64), ("Architecture", "arm64"), ("Config", {"Labels": {"org.opencontainers.image.revision": "c" * 40}})]:
            with self.subTest(field=field):
                self.refs[last] = {**copy.deepcopy(original), field: value}
                with self.assertRaises(ValueError):
                    ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
                self.assert_no_push()
        self.refs[last] = original

    def test_missing_image_fails_before_any_push(self):
        self.save()
        del self.refs[list(self.refs)[7]]
        with self.assertRaises(KeyError):
            ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
        self.assert_no_push()

    def test_missing_pushed_digest_does_not_emit_merge_metadata(self):
        self.save()
        self.omit_digest = True
        with self.assertRaisesRegex(ValueError, "unique pushed digest"):
            ARTIFACTS.push(REGISTRY, "amd64", SHA, self.directory)
        self.assertFalse((self.directory / "digests-amd64.json").exists())


if __name__ == "__main__":
    unittest.main()
