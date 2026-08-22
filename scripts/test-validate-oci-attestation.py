#!/usr/bin/env python3
"""Focused transport tests for the OCI attestation blob validator."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
from email.message import Message
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True


VALIDATOR = Path(__file__).with_name("validate-oci-attestation.py")
SPEC = importlib.util.spec_from_file_location("oci_attestation_validator", VALIDATOR)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load OCI attestation validator")
VALIDATION = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATION)


class FakeResponse:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.headers = Message()
        self.headers["Content-Length"] = str(len(data))

    def __enter__(self) -> "FakeResponse":
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    def read(self, limit: int) -> bytes:
        return self.data[:limit]


class AuthenticatedRegistry:
    def __init__(self, blob: bytes, basic: str) -> None:
        self.blob = blob
        self.basic = basic
        self.calls: list[urllib.request.Request] = []

    def open(
        self, request: urllib.request.Request, timeout: int
    ) -> FakeResponse:
        if timeout != 30:
            raise AssertionError("validator did not use its bounded request timeout")
        self.calls.append(request)
        if len(self.calls) == 1:
            headers = Message()
            headers["WWW-Authenticate"] = (
                'Bearer realm="https://ghcr.io/token",service="ghcr.io",'
                'scope="repository:test/project:pull"'
            )
            raise urllib.error.HTTPError(
                request.full_url, 401, "Unauthorized", headers, None
            )
        if len(self.calls) == 2:
            if request.get_header("Authorization") != self.basic:
                raise AssertionError("Docker credentials were not confined to token exchange")
            return FakeResponse(json.dumps({"token": "fixture-token"}).encode())
        if len(self.calls) == 3:
            if request.get_header("Authorization") != "Bearer fixture-token":
                raise AssertionError("registry blob request did not use the scoped token")
            return FakeResponse(self.blob)
        raise AssertionError("validator issued an unexpected registry request")


class ValidatorInputSecurityTests(unittest.TestCase):
    IMAGE = "ghcr.io/test/project"
    RUNNABLE = "sha256:" + "a" * 64

    def validate_manifest_path(self, path: Path) -> list[str]:
        return VALIDATION.validate_manifest(
            self.IMAGE,
            path,
            self.RUNNABLE,
            123,
            "mcp-repl",
            "1.2.3",
        )

    def write_attestation_manifest(
        self, path: Path, predicates: tuple[str, ...]
    ) -> None:
        layers = [
            {
                "mediaType": VALIDATION.IN_TOTO_MEDIA_TYPE,
                "digest": "sha256:" + f"{index + 1:064x}",
                "size": 1,
                "annotations": {"in-toto.io/predicate-type": predicate},
            }
            for index, predicate in enumerate(predicates)
        ]
        path.write_text(
            json.dumps(
                {
                    "schemaVersion": 2,
                    "mediaType": VALIDATION.OCI_MANIFEST_MEDIA_TYPE,
                    "artifactType": VALIDATION.ATTESTATION_ARTIFACT_TYPE,
                    "config": {
                        "mediaType": VALIDATION.OCI_EMPTY_MEDIA_TYPE,
                        "digest": VALIDATION.OCI_EMPTY_DIGEST,
                        "size": 2,
                        "data": "e30=",
                    },
                    "layers": layers,
                    "subject": {
                        "mediaType": VALIDATION.OCI_MANIFEST_MEDIA_TYPE,
                        "digest": self.RUNNABLE,
                        "size": 123,
                    },
                }
            ),
            encoding="utf-8",
        )

    def test_ghcr_parser_returns_only_owner_and_repository(self) -> None:
        self.assertEqual(VALIDATION.parse_ghcr_image(self.IMAGE), "test/project")
        self.assertEqual(
            VALIDATION.parse_ghcr_image("GHCR.IO/Example/Project"),
            "Example/Project",
        )

    def test_ghcr_parser_rejects_urls_hosts_tags_and_dot_segments(self) -> None:
        invalid_images = (
            "https://ghcr.io/test/project",
            "http://ghcr.io/test/project",
            "evil.example/test/project",
            "ghcr.io.evil.example/test/project",
            "ghcr.io:443/test/project",
            "user@ghcr.io/test/project",
            "ghcr.io/test/project/extra",
            "ghcr.io/test",
            "ghcr.io//project",
            "ghcr.io/test/",
            "ghcr.io/./project",
            "ghcr.io/../project",
            "ghcr.io/test/.",
            "ghcr.io/test/..",
            "ghcr.io/%2e/project",
            "ghcr.io/test/project:latest",
            "ghcr.io/test/project@sha256:" + "a" * 64,
            "ghcr.io/test/project?host=evil.example",
            "ghcr.io/test/project#fragment",
        )
        for image in invalid_images:
            with self.subTest(image=image):
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.parse_ghcr_image(image)

    def test_non_ghcr_image_is_rejected_before_network_setup(self) -> None:
        digest = "sha256:" + hashlib.sha256(b"x").hexdigest()
        with mock.patch.object(
            VALIDATION.ssl, "create_default_context"
        ) as create_context:
            with mock.patch.object(
                VALIDATION.urllib.request, "build_opener"
            ) as build_opener:
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.fetch_registry_blob(
                        "evil.example/test/project", digest, 1, None
                    )
        create_context.assert_not_called()
        build_opener.assert_not_called()

    def test_oversized_blob_descriptor_is_rejected_before_network_setup(self) -> None:
        digest = "sha256:" + hashlib.sha256(b"x").hexdigest()
        with mock.patch.object(
            VALIDATION.ssl, "create_default_context"
        ) as create_context:
            with mock.patch.object(
                VALIDATION.urllib.request, "build_opener"
            ) as build_opener:
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.fetch_registry_blob(
                        self.IMAGE,
                        digest,
                        VALIDATION.MAX_BLOB_SIZE + 1,
                        None,
                    )
        create_context.assert_not_called()
        build_opener.assert_not_called()

    def test_regular_path_rejects_special_files_and_oversize(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            regular = root / "regular"
            regular.write_bytes(b"trusted")
            symlink = root / "symlink"
            symlink.symlink_to(regular)
            child_directory = root / "directory"
            child_directory.mkdir()
            fifo = root / "fifo"
            os.mkfifo(fifo)

            for path in (symlink, child_directory, fifo):
                with self.subTest(kind=path.name):
                    with self.assertRaises(VALIDATION.ValidationError):
                        VALIDATION.read_regular_path(path, "fixture", 1024)
            socket_stat = os.stat_result(
                (VALIDATION.stat.S_IFSOCK | 0o600, 0, 0, 1, 0, 0, 0, 0, 0, 0)
            )
            with mock.patch.object(VALIDATION.os, "fstat", return_value=socket_stat):
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.read_regular_path(regular, "fixture", 1024)
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "exceeds its size bound"
            ):
                VALIDATION.read_regular_path(regular, "fixture", 3)

    def test_regular_path_rejects_a_file_that_grows_during_read(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "growing"
            path.write_bytes(b"initial")
            original_read = os.read
            grew = False

            def grow_after_read(file_descriptor: int, count: int) -> bytes:
                nonlocal grew
                data = original_read(file_descriptor, count)
                if not grew:
                    with path.open("ab") as stream:
                        stream.write(b"-growth")
                    grew = True
                return data

            with mock.patch.object(VALIDATION.os, "read", side_effect=grow_after_read):
                with self.assertRaisesRegex(
                    VALIDATION.ValidationError, "changed while it was being read"
                ):
                    VALIDATION.read_regular_path(path, "fixture", 1024)

    def test_regular_path_rejects_a_file_that_shrinks_during_read(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "shrinking"
            path.write_bytes(b"x" * (VALIDATION.FILE_READ_CHUNK_SIZE + 32))
            original_read = os.read
            shrank = False

            def shrink_after_read(file_descriptor: int, count: int) -> bytes:
                nonlocal shrank
                data = original_read(file_descriptor, count)
                if not shrank:
                    with path.open("r+b") as stream:
                        stream.truncate(1)
                    shrank = True
                return data

            with mock.patch.object(
                VALIDATION.os, "read", side_effect=shrink_after_read
            ):
                with self.assertRaisesRegex(
                    VALIDATION.ValidationError, "changed while it was being read"
                ):
                    VALIDATION.read_regular_path(path, "fixture", 1024 * 1024)

    def test_regular_path_descriptor_survives_final_entry_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "fixture"
            path.write_bytes(b"trusted")
            replacement = root / "replacement"
            replacement.write_bytes(b"replacement")
            renamed = root / "renamed"
            original_open = os.open
            swapped = False

            def swap_after_open(
                opened_path: os.PathLike[str] | str,
                flags: int,
                mode: int = 0o600,
                *,
                dir_fd: int | None = None,
            ) -> int:
                nonlocal swapped
                self.assertEqual(mode, 0o600)
                if dir_fd is None:
                    descriptor = original_open(opened_path, flags)
                else:
                    descriptor = original_open(opened_path, flags, dir_fd=dir_fd)
                if not swapped and dir_fd is None and Path(opened_path) == path:
                    path.rename(renamed)
                    replacement.rename(path)
                    swapped = True
                return descriptor

            with mock.patch.object(VALIDATION.os, "open", side_effect=swap_after_open):
                actual = VALIDATION.read_regular_path(path, "fixture", 1024)
        self.assertTrue(swapped)
        self.assertEqual(actual, b"trusted")

    def test_directory_descriptor_survives_path_rename_and_replacement(self) -> None:
        expected_document = b'{"fixture":"alpha"}'
        alternate_document = b'{"fixture":"beta"}'
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "config"
            root.mkdir()
            (root / "config.json").write_bytes(expected_document)
            renamed = parent / "config-renamed"
            original_open = os.open
            swapped = False

            def swap_after_directory_open(
                path: os.PathLike[str] | str,
                flags: int,
                mode: int = 0o600,
                *,
                dir_fd: int | None = None,
            ) -> int:
                nonlocal swapped
                self.assertEqual(mode, 0o600)
                if dir_fd is None:
                    descriptor = original_open(path, flags)
                else:
                    descriptor = original_open(path, flags, dir_fd=dir_fd)
                if not swapped and dir_fd is None and Path(path) == root:
                    root.rename(renamed)
                    root.mkdir()
                    (root / "config.json").write_bytes(alternate_document)
                    swapped = True
                return descriptor

            with mock.patch.object(
                VALIDATION.os, "open", side_effect=swap_after_directory_open
            ):
                actual = VALIDATION.read_regular_directory_child(
                    root,
                    "config.json",
                    "fixture directory",
                    "fixture file",
                    1024,
                )
        self.assertTrue(swapped)
        self.assertEqual(actual, expected_document)

    def test_secure_file_capability_failures_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture"
            path.write_bytes(b"data")
            for flag in ("O_NOFOLLOW", "O_NONBLOCK", "O_CLOEXEC"):
                with self.subTest(flag=flag), mock.patch.object(
                    VALIDATION.os, flag, None
                ):
                    with self.assertRaisesRegex(
                        VALIDATION.ValidationError,
                        "secure file validation is unavailable",
                    ):
                        VALIDATION.read_regular_path(path, "fixture", 1024)
            with mock.patch.object(VALIDATION.os, "fstat", None):
                with self.assertRaisesRegex(
                    VALIDATION.ValidationError, "secure file operations are unavailable"
                ):
                    VALIDATION.read_regular_path(path, "fixture", 1024)
            with mock.patch.object(VALIDATION.os, "O_DIRECTORY", None):
                with self.assertRaisesRegex(
                    VALIDATION.ValidationError,
                    "secure directory validation is unavailable",
                ):
                    VALIDATION.read_regular_directory_child(
                        directory,
                        "fixture",
                        "fixture directory",
                        "fixture",
                        1024,
                    )
            with mock.patch.object(VALIDATION, "OPEN_SUPPORTS_DIR_FD", False):
                with self.assertRaisesRegex(
                    VALIDATION.ValidationError,
                    "secure directory-relative opens are unavailable",
                ):
                    VALIDATION.read_regular_directory_child(
                        directory,
                        "fixture",
                        "fixture directory",
                        "fixture",
                        1024,
                    )

    def test_docker_config_missing_and_regular_file_semantics(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "docker"
            with mock.patch.dict(os.environ, {"DOCKER_CONFIG": str(root)}):
                self.assertIsNone(VALIDATION.docker_credentials())
                root.mkdir()
                self.assertIsNone(VALIDATION.docker_credentials())
                fixture_value = os.urandom(32).hex()
                encoded = base64.b64encode(
                    f"github-actions:{fixture_value}".encode()
                ).decode()
                (root / "config.json").write_text(
                    json.dumps({"auths": {"ghcr.io": {"auth": encoded}}}),
                    encoding="utf-8",
                )
                self.assertEqual(
                    VALIDATION.docker_credentials(),
                    ("github-actions", fixture_value),
                )

    def test_docker_config_rejects_symlink_directory_fifo_and_oversize(self) -> None:
        cases = ("symlink", "directory", "fifo", "oversize")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "docker"
                root.mkdir()
                config = root / "config.json"
                if case == "symlink":
                    target = Path(directory) / "target.json"
                    target.write_text("{}", encoding="utf-8")
                    config.symlink_to(target)
                elif case == "directory":
                    config.mkdir()
                elif case == "fifo":
                    os.mkfifo(config)
                else:
                    config.write_bytes(b"x" * (VALIDATION.MAX_DOCKER_CONFIG_SIZE + 1))
                with mock.patch.dict(os.environ, {"DOCKER_CONFIG": str(root)}):
                    with self.assertRaises(VALIDATION.ValidationError):
                        VALIDATION.docker_credentials()

    def test_docker_config_rejects_a_symlink_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.mkdir()
            (target / "config.json").write_text("{}", encoding="utf-8")
            root = Path(directory) / "docker"
            root.symlink_to(target, target_is_directory=True)
            with mock.patch.dict(os.environ, {"DOCKER_CONFIG": str(root)}):
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.docker_credentials()

    def test_cache_reads_exact_regular_blob_and_rejects_size_or_hash_drift(self) -> None:
        blob = b"immutable cached attestation"
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            blob_path = cache / f"{digest.removeprefix('sha256:')}.blob"
            blob_path.write_bytes(blob)
            self.assertEqual(
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                ),
                blob,
            )

            blob_path.write_bytes(blob + b"x")
            with self.assertRaisesRegex(VALIDATION.ValidationError, "size bound"):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                )

            blob_path.write_bytes(blob[:-1])
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "size differs from its descriptor"
            ):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                )

            blob_path.write_bytes(b"x" * len(blob))
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "bytes differ from their descriptor"
            ):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                )

    def test_cache_preserves_missing_directory_and_blob_semantics(self) -> None:
        blob = b"fixture"
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing"
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "OCI blob cache does not exist"
            ):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(missing)
                )
            cache = Path(directory) / "cache"
            cache.mkdir()
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "OCI blob cache is missing"
            ):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                )

    def test_cache_rejects_symlink_directory_and_fifo_blob_entries(self) -> None:
        blob = b"fixture"
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        filename = f"{digest.removeprefix('sha256:')}.blob"
        for case in ("symlink", "directory", "fifo"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                cache = Path(directory) / "cache"
                cache.mkdir()
                blob_path = cache / filename
                if case == "symlink":
                    target = Path(directory) / "target"
                    target.write_bytes(blob)
                    blob_path.symlink_to(target)
                elif case == "directory":
                    blob_path.mkdir()
                else:
                    os.mkfifo(blob_path)
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.fetch_registry_blob(
                        self.IMAGE, digest, len(blob), str(cache)
                    )

    def test_cache_rejects_a_symlink_directory(self) -> None:
        blob = b"fixture"
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.mkdir()
            (target / f"{digest.removeprefix('sha256:')}.blob").write_bytes(blob)
            cache = Path(directory) / "cache"
            cache.symlink_to(target, target_is_directory=True)
            with self.assertRaises(VALIDATION.ValidationError):
                VALIDATION.fetch_registry_blob(
                    self.IMAGE, digest, len(blob), str(cache)
                )

    def test_manifest_preserves_missing_file_semantics(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing.json"
            with self.assertRaisesRegex(
                VALIDATION.ValidationError, "attestation manifest does not exist"
            ):
                self.validate_manifest_path(missing)

    def test_manifest_rejects_symlink_directory_fifo_and_oversize(self) -> None:
        for case in ("symlink", "directory", "fifo", "oversize"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                manifest = root / "manifest.json"
                if case == "symlink":
                    target = root / "target.json"
                    target.write_text("{}", encoding="utf-8")
                    manifest.symlink_to(target)
                elif case == "directory":
                    manifest.mkdir()
                elif case == "fifo":
                    os.mkfifo(manifest)
                else:
                    manifest.write_bytes(b"x" * (VALIDATION.MAX_MANIFEST_SIZE + 1))
                with self.assertRaises(VALIDATION.ValidationError):
                    self.validate_manifest_path(manifest)

    def test_manifest_rejects_layer_resource_amplification_before_fetch(self) -> None:
        provenance = "https://slsa.dev/provenance/v1"
        spdx = "https://spdx.dev/Document"
        cases = {
            "layer count": (provenance, spdx, provenance),
            "repeated predicate": (provenance, provenance),
        }
        for case, predicates in cases.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                manifest = Path(directory) / "manifest.json"
                self.write_attestation_manifest(manifest, predicates)
                with mock.patch.object(VALIDATION, "fetch_registry_blob") as fetch:
                    with self.assertRaises(VALIDATION.ValidationError):
                        self.validate_manifest_path(manifest)
                fetch.assert_not_called()

        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "manifest.json"
            self.write_attestation_manifest(manifest, (provenance, spdx))
            with mock.patch.object(VALIDATION, "MAX_TOTAL_ATTESTATION_SIZE", 1):
                with mock.patch.object(VALIDATION, "fetch_registry_blob") as fetch:
                    with self.assertRaisesRegex(
                        VALIDATION.ValidationError, "aggregate size bound"
                    ):
                        self.validate_manifest_path(manifest)
                fetch.assert_not_called()


class ValidatorTransportTests(unittest.TestCase):
    def test_private_registry_token_exchange_fetches_exact_blob(self) -> None:
        blob = b'{"fixture":"immutable-attestation"}'
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        encoded = base64.b64encode(
            f"github-actions:{os.urandom(32).hex()}".encode()
        ).decode()
        expected_basic = "Basic " + encoded
        registry = AuthenticatedRegistry(blob, expected_basic)

        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.json"
            config.write_text(
                json.dumps({"auths": {"ghcr.io": {"auth": encoded}}}),
                encoding="utf-8",
            )
            with mock.patch.dict(os.environ, {"DOCKER_CONFIG": directory}, clear=False):
                with mock.patch.object(
                    VALIDATION.urllib.request,
                    "build_opener",
                    return_value=registry,
                ):
                    actual = VALIDATION.fetch_registry_blob(
                        "ghcr.io/test/project", digest, len(blob), None
                    )

        self.assertEqual(actual, blob)
        self.assertEqual(len(registry.calls), 3)
        self.assertIsNone(registry.calls[0].get_header("Authorization"))
        self.assertEqual(
            registry.calls[0].full_url,
            f"https://ghcr.io/v2/test/project/blobs/{digest}",
        )
        self.assertEqual(
            registry.calls[1].full_url,
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Atest%2Fproject%3Apull",
        )

    def test_unsafe_bearer_realms_are_rejected(self) -> None:
        realms = (
            "http://ghcr.io/token",
            "https://evil.example/token",
            "https://ghcr.io:443/token",
            "https://user@ghcr.io/token",
            "https://user:opaque@ghcr.io/token",
            "https://ghcr.io/token#fragment",
        )
        for realm in realms:
            with self.subTest(realm=realm):
                with self.assertRaises(VALIDATION.ValidationError):
                    VALIDATION.bearer_challenge(
                        f'Bearer realm="{realm}",service="ghcr.io"',
                        "test/project",
                    )

    def test_cross_origin_redirect_drops_registry_authorization(self) -> None:
        request = urllib.request.Request(
            "https://ghcr.io/v2/test/project/blobs/sha256:deadbeef",
            headers={"Authorization": "Bearer secret"},
        )
        redirected = VALIDATION.SafeRedirectHandler().redirect_request(
            request,
            None,
            307,
            "Temporary Redirect",
            Message(),
            "https://objects.example.test/blob?signed=true",
        )
        self.assertIsNotNone(redirected)
        self.assertIsNone(redirected.get_header("Authorization"))

    def test_duplicate_json_keys_are_rejected(self) -> None:
        with self.assertRaises(VALIDATION.ValidationError):
            VALIDATION.load_json_bytes(b'{"subject":[],"subject":[]}', "fixture")

    def test_slsa_v1_requires_the_pinned_buildkit_identity(self) -> None:
        expected_builder_id = VALIDATION.buildkit_builder_id(
            "ghcr.io/test/project"
        )
        predicate = {
            "buildDefinition": {
                "buildType": VALIDATION.BUILDKIT_V1_BUILD_TYPE,
                "externalParameters": {},
                "internalParameters": {},
                "resolvedDependencies": [],
            },
            "runDetails": {
                "builder": {"id": expected_builder_id},
                "metadata": {},
                "byproducts": [],
            },
        }
        VALIDATION.validate_predicate_shape(
            "https://slsa.dev/provenance/v1",
            predicate,
            "mcp-repl",
            "1.2.3",
            expected_builder_id,
        )
        predicate["runDetails"]["builder"]["id"] = "https://example.invalid/builder"
        with self.assertRaises(VALIDATION.ValidationError):
            VALIDATION.validate_predicate_shape(
                "https://slsa.dev/provenance/v1",
                predicate,
                "mcp-repl",
                "1.2.3",
                expected_builder_id,
            )

    def test_slsa_v02_is_rejected(self) -> None:
        expected_builder_id = VALIDATION.buildkit_builder_id(
            "ghcr.io/test/project"
        )
        predicate = {
            "builder": {"id": expected_builder_id},
            "buildType": "https://mobyproject.org/buildkit@v1",
            "invocation": {},
            "metadata": {},
            "materials": [],
        }
        with self.assertRaises(VALIDATION.ValidationError):
            VALIDATION.validate_predicate_shape(
                "https://slsa.dev/provenance/v0.2",
                predicate,
                "mcp-repl",
                "1.2.3",
                expected_builder_id,
            )

    def test_inherited_environment_cannot_enable_fixture_cache(self) -> None:
        arguments = [
            str(VALIDATOR),
            "ghcr.io/test/project",
            "/tmp/manifest.json",
            "sha256:" + "a" * 64,
            "123",
            "mcp-repl",
            "1.2.3",
        ]
        with mock.patch.dict(
            os.environ,
            {
                "MCP_REPL_OCI_BLOB_CACHE": "/tmp/attacker-cache",
                "MCP_REPL_TESTING": "1",
            },
            clear=False,
        ):
            with mock.patch.object(sys, "argv", arguments):
                with mock.patch.object(
                    VALIDATION, "validate_manifest", return_value=[]
                ) as validate:
                    self.assertEqual(VALIDATION.main(), 0)
        self.assertIsNone(validate.call_args.args[-1])

    def test_fixture_cache_requires_explicit_cli_argument(self) -> None:
        arguments = [
            str(VALIDATOR),
            "--test-blob-cache",
            "/tmp/fixture-cache",
            "ghcr.io/test/project",
            "/tmp/manifest.json",
            "sha256:" + "a" * 64,
            "123",
            "mcp-repl",
            "1.2.3",
        ]
        with mock.patch.object(sys, "argv", arguments):
            with mock.patch.object(
                VALIDATION, "validate_manifest", return_value=[]
            ) as validate:
                self.assertEqual(VALIDATION.main(), 0)
        self.assertEqual(validate.call_args.args[-1], Path("/tmp/fixture-cache"))

    def test_spdx_dependency_edge_must_come_from_a_cargo_package(self) -> None:
        document_root = {"SPDXID": "SPDXRef-DocumentRoot", "name": "root"}
        root_package = {"SPDXID": "SPDXRef-mcp-repl", "name": "mcp-repl"}
        cargo_dependency = {"SPDXID": "SPDXRef-cargo-dependency", "name": "cargo"}
        dummy_dependency = {"SPDXID": "SPDXRef-dummy", "name": "dummy"}
        packages = [
            document_root,
            root_package,
            cargo_dependency,
            dummy_dependency,
        ]
        predicate = {
            "files": [
                {
                    "SPDXID": "SPDXRef-File-mcp-repl",
                    "fileName": "usr/local/bin/mcp-repl",
                }
            ],
            "relationships": [
                {
                    "spdxElementId": "SPDXRef-DOCUMENT",
                    "relationshipType": "DESCRIBES",
                    "relatedSpdxElement": document_root["SPDXID"],
                },
                {
                    "spdxElementId": document_root["SPDXID"],
                    "relationshipType": "CONTAINS",
                    "relatedSpdxElement": root_package["SPDXID"],
                },
                {
                    "spdxElementId": root_package["SPDXID"],
                    "relationshipType": "OTHER",
                    "relatedSpdxElement": "SPDXRef-File-mcp-repl",
                    "comment": (
                        "evident-by: indicates the package's existence is evident "
                        "by the given file"
                    ),
                },
                {
                    "spdxElementId": dummy_dependency["SPDXID"],
                    "relationshipType": "DEPENDENCY_OF",
                    "relatedSpdxElement": root_package["SPDXID"],
                },
            ],
        }
        with self.assertRaises(VALIDATION.ValidationError):
            VALIDATION.validate_spdx_graph(
                predicate,
                packages,
                root_package,
                "mcp-repl",
                {cargo_dependency["SPDXID"]},
            )

        predicate["relationships"][-1]["spdxElementId"] = cargo_dependency["SPDXID"]
        VALIDATION.validate_spdx_graph(
            predicate,
            packages,
            root_package,
            "mcp-repl",
            {cargo_dependency["SPDXID"]},
        )


if __name__ == "__main__":
    unittest.main()
