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


class ValidatorTransportTests(unittest.TestCase):
    def test_private_registry_token_exchange_fetches_exact_blob(self) -> None:
        blob = b'{"fixture":"immutable-attestation"}'
        digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        username = "github-actions"
        password = "secret-token"
        encoded = base64.b64encode(f"{username}:{password}".encode()).decode()
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
            registry.calls[1].full_url,
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Atest%2Fproject%3Apull",
        )

    def test_cross_host_bearer_realm_is_rejected(self) -> None:
        with self.assertRaises(VALIDATION.ValidationError):
            VALIDATION.bearer_challenge(
                'Bearer realm="https://evil.example/token",service="ghcr.io"',
                "ghcr.io",
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
