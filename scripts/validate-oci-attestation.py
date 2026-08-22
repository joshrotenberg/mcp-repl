#!/usr/bin/env python3
"""Fetch and validate every in-toto blob in one BuildKit attestation manifest."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import ssl
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


SHA256_RE = re.compile(r"^sha256:([0-9a-f]{64})$")
IMAGE_RE = re.compile(
    r"^(?P<registry>[A-Za-z0-9.-]+(?::[1-9][0-9]{0,4})?)/"
    r"(?P<repository>[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*)$"
)
MAX_BLOB_SIZE = 42 * 1024 * 1024
STATEMENT_TYPE = "https://in-toto.io/Statement/v1"
IN_TOTO_MEDIA_TYPE = "application/vnd.in-toto+json"
ATTESTATION_ARTIFACT_TYPE = "application/vnd.docker.attestation.manifest.v1+json"
OCI_MANIFEST_MEDIA_TYPE = "application/vnd.oci.image.manifest.v1+json"
OCI_EMPTY_MEDIA_TYPE = "application/vnd.oci.empty.v1+json"
OCI_EMPTY_DIGEST = (
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
)
SUPPORTED_PREDICATES = {
    "https://slsa.dev/provenance/v1",
    "https://spdx.dev/Document",
}
BUILDKIT_V1_BUILD_TYPE = (
    "https://github.com/moby/buildkit/blob/master/"
    "docs/attestations/slsa-definitions.md"
)
PACKAGE_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")
VERSION_RE = re.compile(
    r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$"
)


class ValidationError(Exception):
    """A fail-closed validation error suitable for a concise CI diagnostic."""


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValidationError(f"JSON contains duplicate key {key!r}")
        result[key] = value
    return result


def load_json_bytes(data: bytes, label: str) -> Any:
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValidationError(f"{label} is not UTF-8 JSON") from error
    try:
        return json.loads(text, object_pairs_hook=reject_duplicate_keys)
    except (json.JSONDecodeError, ValidationError) as error:
        raise ValidationError(f"{label} is not unambiguous JSON: {error}") from error


def exact_keys(value: dict[str, Any], allowed: set[str], label: str) -> None:
    unexpected = set(value) - allowed
    if unexpected:
        names = ", ".join(sorted(unexpected))
        raise ValidationError(f"{label} has unexpected field(s): {names}")


def is_positive_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def buildkit_builder_id(image: str) -> str:
    match = IMAGE_RE.fullmatch(image)
    if match is None or match.group("registry").lower() != "ghcr.io":
        raise ValidationError("BuildKit provenance requires a GHCR repository")
    repository = match.group("repository")
    if len(repository.split("/")) != 2:
        raise ValidationError("BuildKit provenance requires a GitHub repository image")
    return f"https://github.com/{repository}/.github/workflows/container-build.yml"


def string_annotations(value: Any, label: str) -> dict[str, str]:
    if value is None:
        return {}
    if not isinstance(value, dict) or not all(
        isinstance(key, str) and isinstance(item, str)
        for key, item in value.items()
    ):
        raise ValidationError(f"{label} annotations must be a string map")
    return value


def validate_descriptor(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValidationError(f"{label} is not an OCI descriptor")
    exact_keys(
        value,
        {
            "mediaType",
            "digest",
            "size",
            "urls",
            "annotations",
            "data",
            "artifactType",
            "platform",
        },
        label,
    )
    if not isinstance(value.get("mediaType"), str) or not value["mediaType"]:
        raise ValidationError(f"{label} has no media type")
    if not isinstance(value.get("digest"), str) or not SHA256_RE.fullmatch(
        value["digest"]
    ):
        raise ValidationError(f"{label} has no canonical SHA-256 digest")
    if not is_positive_integer(value.get("size")):
        raise ValidationError(f"{label} has no positive integral size")
    urls = value.get("urls", [])
    if not isinstance(urls, list) or not all(
        isinstance(item, str) and item for item in urls
    ):
        raise ValidationError(f"{label} has invalid fallback URLs")
    if "data" in value and not isinstance(value["data"], str):
        raise ValidationError(f"{label} has invalid embedded data")
    if "artifactType" in value and (
        not isinstance(value["artifactType"], str) or not value["artifactType"]
    ):
        raise ValidationError(f"{label} has invalid artifact type")
    if "platform" in value and not isinstance(value["platform"], dict):
        raise ValidationError(f"{label} has invalid platform")
    string_annotations(value.get("annotations"), label)
    return value


def validate_spdx_package(package: Any) -> dict[str, Any]:
    if not isinstance(package, dict):
        raise ValidationError("SPDX packages must be objects")
    spdx_id = package.get("SPDXID")
    name = package.get("name")
    version = package.get("versionInfo")
    if (
        not isinstance(spdx_id, str)
        or not re.fullmatch(r"SPDXRef-[A-Za-z0-9.-]+", spdx_id)
        or not isinstance(name, str)
        or not name
        or (version is not None and (not isinstance(version, str) or not version))
    ):
        raise ValidationError("SPDX package has an invalid identity")
    external_refs = package.get("externalRefs", [])
    if not isinstance(external_refs, list):
        raise ValidationError("SPDX package externalRefs is not an array")
    for reference in external_refs:
        if not isinstance(reference, dict) or not all(
            isinstance(reference.get(field), str) and reference[field]
            for field in ("referenceCategory", "referenceType", "referenceLocator")
        ):
            raise ValidationError("SPDX package has an invalid external reference")
    return package


def validate_spdx_graph(
    predicate: dict[str, Any],
    packages: list[dict[str, Any]],
    root_package: dict[str, Any],
    expected_package: str,
    cargo_dependency_ids: set[str],
) -> None:
    files_raw = predicate.get("files")
    if not isinstance(files_raw, list) or not files_raw:
        raise ValidationError("SPDX predicate has no file evidence")
    files: list[dict[str, Any]] = []
    for file_entry in files_raw:
        if not isinstance(file_entry, dict):
            raise ValidationError("SPDX files must be objects")
        spdx_id = file_entry.get("SPDXID")
        file_name = file_entry.get("fileName")
        if (
            not isinstance(spdx_id, str)
            or not re.fullmatch(r"SPDXRef-[A-Za-z0-9.-]+", spdx_id)
            or not isinstance(file_name, str)
            or not file_name
        ):
            raise ValidationError("SPDX file has an invalid identity")
        files.append(file_entry)

    package_by_id = {package["SPDXID"]: package for package in packages}
    file_by_id = {file_entry["SPDXID"]: file_entry for file_entry in files}
    if len(file_by_id) != len(files) or set(package_by_id) & set(file_by_id):
        raise ValidationError("SPDX predicate repeats an element SPDXID")
    known_ids = {"SPDXRef-DOCUMENT", *package_by_id, *file_by_id}

    relationships_raw = predicate.get("relationships")
    if not isinstance(relationships_raw, list) or not relationships_raw:
        raise ValidationError("SPDX predicate has no relationships")
    relationships: list[dict[str, Any]] = []
    for relationship in relationships_raw:
        if not isinstance(relationship, dict) or not all(
            isinstance(relationship.get(field), str) and relationship[field]
            for field in (
                "spdxElementId",
                "relationshipType",
                "relatedSpdxElement",
            )
        ):
            raise ValidationError("SPDX relationship is malformed")
        comment = relationship.get("comment")
        if comment is not None and (not isinstance(comment, str) or not comment):
            raise ValidationError("SPDX relationship comment is invalid")
        if (
            relationship["spdxElementId"] not in known_ids
            or relationship["relatedSpdxElement"] not in known_ids
        ):
            raise ValidationError("SPDX relationship references an unknown element")
        relationships.append(relationship)

    described_roots = [
        relationship["relatedSpdxElement"]
        for relationship in relationships
        if relationship["spdxElementId"] == "SPDXRef-DOCUMENT"
        and relationship["relationshipType"] == "DESCRIBES"
        and relationship["relatedSpdxElement"] in package_by_id
    ]
    if len(described_roots) != 1:
        raise ValidationError("SPDX document does not describe exactly one root package")
    described_root = described_roots[0]
    mcp_id = root_package["SPDXID"]
    if described_root == mcp_id or not any(
        relationship["spdxElementId"] == described_root
        and relationship["relationshipType"] == "CONTAINS"
        and relationship["relatedSpdxElement"] == mcp_id
        for relationship in relationships
    ):
        raise ValidationError("SPDX root does not contain the release package")

    evidence_comment = (
        "evident-by: indicates the package's existence is evident by the given file"
    )
    evidence_ids = {
        relationship["relatedSpdxElement"]
        for relationship in relationships
        if relationship["spdxElementId"] == mcp_id
        and relationship["relationshipType"] == "OTHER"
        and relationship.get("comment") == evidence_comment
        and relationship["relatedSpdxElement"] in file_by_id
    }
    expected_file = f"usr/local/bin/{expected_package}"
    evidence_files = [
        file_by_id[spdx_id]
        for spdx_id in evidence_ids
        if file_by_id[spdx_id]["fileName"] == expected_file
    ]
    if len(evidence_files) != 1:
        raise ValidationError("SPDX release package lacks exact binary file evidence")

    dependencies = {
        relationship["spdxElementId"]
        for relationship in relationships
        if relationship["relationshipType"] == "DEPENDENCY_OF"
        and relationship["relatedSpdxElement"] == mcp_id
        and relationship["spdxElementId"] in cargo_dependency_ids
    }
    if not dependencies:
        raise ValidationError("SPDX release package has no dependency relationships")


def validate_predicate_shape(
    predicate_type: str,
    predicate: Any,
    expected_package: str,
    expected_version: str,
    expected_builder_id: str,
) -> None:
    if not isinstance(predicate, dict):
        raise ValidationError(f"{predicate_type} predicate is not an object")
    if predicate_type == "https://spdx.dev/Document":
        if (
            predicate.get("SPDXID") != "SPDXRef-DOCUMENT"
            or predicate.get("spdxVersion") != "SPDX-2.3"
            or predicate.get("dataLicense") != "CC0-1.0"
            or not isinstance(predicate.get("documentNamespace"), str)
            or not predicate["documentNamespace"]
            or not isinstance(predicate.get("creationInfo"), dict)
        ):
            raise ValidationError("SPDX predicate is not a complete SPDX document")
        packages_raw = predicate.get("packages")
        if not isinstance(packages_raw, list) or len(packages_raw) < 2:
            raise ValidationError("SPDX predicate has no dependency inventory")
        packages = [validate_spdx_package(package) for package in packages_raw]
        spdx_ids = [package["SPDXID"] for package in packages]
        if len(spdx_ids) != len(set(spdx_ids)):
            raise ValidationError("SPDX predicate repeats a package SPDXID")
        root_packages = [
            package for package in packages if package["name"] == expected_package
        ]
        if len(root_packages) != 1:
            raise ValidationError(
                f"SPDX predicate does not contain exactly one {expected_package} package"
            )
        root_package = root_packages[0]
        if root_package.get("sourceInfo") != (
            "acquired package info from rust cargo manifest: "
            f"/usr/local/bin/{expected_package}"
        ):
            raise ValidationError("SPDX root package lacks cargo-auditable source evidence")
        expected_purl = f"pkg:cargo/{expected_package}@{expected_version}"
        expected_reference = {
            "referenceCategory": "PACKAGE-MANAGER",
            "referenceType": "purl",
            "referenceLocator": expected_purl,
        }
        matching_purls = [
            (package, reference)
            for package in packages
            for reference in package.get("externalRefs", [])
            if reference == expected_reference
        ]
        cargo_dependency_ids = {
            package["SPDXID"]
            for package in packages
            if package is not root_package
            if any(
                reference.get("referenceCategory") == "PACKAGE-MANAGER"
                and reference.get("referenceType") == "purl"
                and reference.get("referenceLocator", "").startswith(
                    "pkg:cargo/"
                )
                for reference in package.get("externalRefs", [])
            )
        }
        if (
            root_package.get("versionInfo") != expected_version
            or matching_purls != [(root_package, expected_reference)]
            or not cargo_dependency_ids
        ):
            raise ValidationError(
                "SPDX Cargo inventory does not match the release root and dependencies"
            )
        validate_spdx_graph(
            predicate,
            packages,
            root_package,
            expected_package,
            cargo_dependency_ids,
        )
    elif predicate_type == "https://slsa.dev/provenance/v1":
        build_definition = predicate.get("buildDefinition")
        run_details = predicate.get("runDetails")
        if (
            not isinstance(build_definition, dict)
            or not isinstance(run_details, dict)
            or build_definition.get("buildType") != BUILDKIT_V1_BUILD_TYPE
            or not isinstance(build_definition.get("externalParameters"), dict)
            or not isinstance(build_definition.get("internalParameters"), dict)
            or not isinstance(build_definition.get("resolvedDependencies"), list)
            or not isinstance(run_details.get("builder"), dict)
            or run_details["builder"].get("id") != expected_builder_id
            or not isinstance(run_details.get("metadata"), dict)
            or not isinstance(run_details.get("byproducts"), list)
        ):
            raise ValidationError("SLSA v1 predicate is incomplete")
    else:
        raise ValidationError(f"unsupported in-toto predicate {predicate_type!r}")


def validate_statement(
    data: bytes,
    expected_predicate: str,
    runnable: str,
    expected_package: str,
    expected_version: str,
    expected_builder_id: str,
) -> None:
    statement = load_json_bytes(data, "attestation blob")
    if not isinstance(statement, dict):
        raise ValidationError("attestation blob is not an in-toto statement object")
    if set(statement) != {"_type", "subject", "predicateType", "predicate"}:
        raise ValidationError("in-toto statement does not have the exact v1 fields")
    if statement["_type"] != STATEMENT_TYPE:
        raise ValidationError("in-toto statement has the wrong type")
    if statement["predicateType"] != expected_predicate:
        raise ValidationError(
            "in-toto predicateType differs from its immutable layer annotation"
        )

    subjects = statement["subject"]
    runnable_hex = SHA256_RE.fullmatch(runnable).group(1)  # validated by caller
    if not isinstance(subjects, list) or len(subjects) != 1:
        raise ValidationError("in-toto statement must have exactly one subject")
    subject = subjects[0]
    if not isinstance(subject, dict) or set(subject) != {"name", "digest"}:
        raise ValidationError("in-toto subject has unexpected fields")
    if not isinstance(subject["name"], str) or not subject["name"]:
        raise ValidationError("in-toto subject name is empty")
    if subject["digest"] != {"sha256": runnable_hex}:
        raise ValidationError("in-toto subject is not the expected runnable manifest")
    validate_predicate_shape(
        expected_predicate,
        statement["predicate"],
        expected_package,
        expected_version,
        expected_builder_id,
    )


def docker_credentials(registry: str) -> tuple[str, str] | None:
    config_root = Path(os.environ.get("DOCKER_CONFIG", Path.home() / ".docker"))
    config_path = config_root / "config.json"
    try:
        config_stat = config_path.lstat()
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(config_stat.st_mode) or not stat.S_ISREG(config_stat.st_mode):
        raise ValidationError("Docker credential config is not a regular file")
    try:
        config = load_json_bytes(config_path.read_bytes(), "Docker credential config")
    except OSError as error:
        raise ValidationError("could not read Docker credential config") from error
    if not isinstance(config, dict) or not isinstance(config.get("auths", {}), dict):
        raise ValidationError("Docker credential config has invalid auths")
    candidates = (
        registry,
        f"https://{registry}",
        f"https://{registry}/v1/",
        f"https://{registry}/v2/",
    )
    entry: Any = None
    for candidate in candidates:
        if candidate in config.get("auths", {}):
            entry = config["auths"][candidate]
            break
    if entry is None:
        return None
    if not isinstance(entry, dict):
        raise ValidationError("Docker registry credential entry is invalid")
    encoded = entry.get("auth")
    if not isinstance(encoded, str) or not encoded:
        raise ValidationError("Docker registry credential entry has no inline auth")
    try:
        decoded = base64.b64decode(encoded, validate=True).decode("utf-8")
    except (ValueError, UnicodeDecodeError) as error:
        raise ValidationError("Docker registry credential auth is malformed") from error
    if ":" not in decoded:
        raise ValidationError("Docker registry credential auth has no password boundary")
    username, password = decoded.split(":", 1)
    if not username or not password:
        raise ValidationError("Docker registry credential is empty")
    return username, password


class SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Reject HTTPS downgrade and never forward credentials cross-origin."""

    def redirect_request(
        self,
        request: urllib.request.Request,
        file_pointer: Any,
        code: int,
        message: str,
        headers: Any,
        new_url: str,
    ) -> urllib.request.Request | None:
        old = urllib.parse.urlsplit(request.full_url)
        new = urllib.parse.urlsplit(new_url)
        if new.scheme != "https":
            raise ValidationError("registry blob redirect attempted to leave HTTPS")
        redirected = super().redirect_request(
            request, file_pointer, code, message, headers, new_url
        )
        if redirected is not None and (old.scheme, old.netloc) != (
            new.scheme,
            new.netloc,
        ):
            redirected.remove_header("Authorization")
        return redirected


def bearer_challenge(value: str, registry: str, repository: str) -> str:
    scheme, separator, parameters = value.partition(" ")
    if separator != " " or scheme.lower() != "bearer":
        raise ValidationError("registry did not return a Bearer authentication challenge")
    try:
        parsed = urllib.request.parse_keqv_list(
            urllib.request.parse_http_list(parameters)
        )
    except (TypeError, ValueError) as error:
        raise ValidationError("registry Bearer challenge is malformed") from error
    realm = parsed.get("realm")
    if not isinstance(realm, str):
        raise ValidationError("registry Bearer challenge has no realm")
    realm_url = urllib.parse.urlsplit(realm)
    if (
        realm_url.scheme != "https"
        or realm_url.netloc != registry
        or realm_url.username is not None
        or realm_url.password is not None
        or realm_url.fragment
    ):
        raise ValidationError("registry Bearer realm is not same-host HTTPS")
    query: dict[str, str] = {}
    service = parsed.get("service")
    scope = parsed.get("scope", f"repository:{repository}:pull")
    if (service is not None and not isinstance(service, str)) or not isinstance(
        scope, str
    ):
        raise ValidationError("registry Bearer challenge has invalid parameters")
    if service:
        query["service"] = service
    query["scope"] = scope
    return realm + ("&" if realm_url.query else "?") + urllib.parse.urlencode(query)


def request_bytes(
    opener: urllib.request.OpenerDirector,
    url: str,
    authorization: str | None = None,
    expected_size: int | None = None,
) -> bytes:
    headers = {"Accept": "application/octet-stream", "User-Agent": "mcp-repl-release"}
    if authorization is not None:
        headers["Authorization"] = authorization
    request = urllib.request.Request(url, headers=headers)
    try:
        with opener.open(request, timeout=30) as response:
            content_length = response.headers.get("Content-Length")
            if content_length is not None:
                if not content_length.isdigit():
                    raise ValidationError("registry returned an invalid Content-Length")
                if expected_size is not None and int(content_length) != expected_size:
                    raise ValidationError("registry blob Content-Length differs from descriptor")
            limit = expected_size if expected_size is not None else MAX_BLOB_SIZE
            data = response.read(limit + 1)
    except ValidationError:
        raise
    except urllib.error.HTTPError:
        raise
    except (OSError, urllib.error.URLError) as error:
        raise ValidationError(f"registry request failed: {error}") from error
    if len(data) > limit:
        raise ValidationError("registry response exceeds its expected size")
    return data


def fetch_registry_blob(
    image: str, digest: str, expected_size: int, cache: str | None
) -> bytes:
    digest_hex = SHA256_RE.fullmatch(digest).group(1)
    if cache is not None:
        cache_path = Path(cache)
        try:
            cache_stat = cache_path.lstat()
        except FileNotFoundError as error:
            raise ValidationError("OCI blob cache does not exist") from error
        if stat.S_ISLNK(cache_stat.st_mode) or not stat.S_ISDIR(cache_stat.st_mode):
            raise ValidationError("OCI blob cache is not a non-symlink directory")
        blob_path = cache_path / f"{digest_hex}.blob"
        try:
            blob_stat = blob_path.lstat()
        except FileNotFoundError as error:
            raise ValidationError(f"OCI blob cache is missing {digest}") from error
        if stat.S_ISLNK(blob_stat.st_mode) or not stat.S_ISREG(blob_stat.st_mode):
            raise ValidationError(f"cached OCI blob {digest} is not a regular file")
        if blob_stat.st_size > expected_size:
            raise ValidationError(f"cached OCI blob {digest} exceeds descriptor size")
        try:
            data = blob_path.read_bytes()
        except OSError as error:
            raise ValidationError(f"could not read cached OCI blob {digest}") from error
    else:
        match = IMAGE_RE.fullmatch(image)
        if match is None:
            raise ValidationError("image must be an untagged registry/repository name")
        registry = match.group("registry")
        repository = match.group("repository")
        url = f"https://{registry}/v2/{repository}/blobs/{digest}"
        context = ssl.create_default_context()
        opener = urllib.request.build_opener(
            urllib.request.HTTPSHandler(context=context), SafeRedirectHandler()
        )
        try:
            data = request_bytes(opener, url, expected_size=expected_size)
        except urllib.error.HTTPError as error:
            if error.code != 401:
                raise ValidationError(
                    f"registry blob request failed with HTTP {error.code}"
                ) from error
            challenge = error.headers.get("WWW-Authenticate", "")
            token_url = bearer_challenge(challenge, registry, repository)
            credentials = docker_credentials(registry)
            basic = None
            if credentials is not None:
                encoded = base64.b64encode(
                    f"{credentials[0]}:{credentials[1]}".encode("utf-8")
                ).decode("ascii")
                basic = f"Basic {encoded}"
            try:
                token_bytes = request_bytes(opener, token_url, authorization=basic)
            except urllib.error.HTTPError as token_error:
                raise ValidationError(
                    f"registry token request failed with HTTP {token_error.code}"
                ) from token_error
            token_document = load_json_bytes(token_bytes, "registry token response")
            if not isinstance(token_document, dict):
                raise ValidationError("registry token response is not an object")
            token = token_document.get("token", token_document.get("access_token"))
            if not isinstance(token, str) or not token:
                raise ValidationError("registry token response has no token")
            try:
                data = request_bytes(
                    opener,
                    url,
                    authorization=f"Bearer {token}",
                    expected_size=expected_size,
                )
            except urllib.error.HTTPError as blob_error:
                raise ValidationError(
                    f"authenticated registry blob request failed with HTTP {blob_error.code}"
                ) from blob_error

    if len(data) != expected_size:
        raise ValidationError(f"OCI blob {digest} size differs from its descriptor")
    actual = hashlib.sha256(data).hexdigest()
    if actual != digest_hex:
        raise ValidationError(f"OCI blob {digest} bytes differ from their descriptor")
    return data


def validate_manifest(
    image: str,
    manifest_path: Path,
    runnable: str,
    runnable_size: int,
    expected_package: str,
    expected_version: str,
    blob_cache: Path | None = None,
) -> list[str]:
    runnable_match = SHA256_RE.fullmatch(runnable)
    if (
        IMAGE_RE.fullmatch(image) is None
        or runnable_match is None
        or not is_positive_integer(runnable_size)
    ):
        raise ValidationError("expected image or runnable descriptor is invalid")
    if not PACKAGE_RE.fullmatch(expected_package) or not VERSION_RE.fullmatch(
        expected_version
    ):
        raise ValidationError("expected package identity is invalid")
    expected_builder_id = buildkit_builder_id(image)
    try:
        manifest_stat = manifest_path.lstat()
    except FileNotFoundError as error:
        raise ValidationError("attestation manifest does not exist") from error
    if stat.S_ISLNK(manifest_stat.st_mode) or not stat.S_ISREG(manifest_stat.st_mode):
        raise ValidationError("attestation manifest is not a regular file")
    try:
        manifest = load_json_bytes(manifest_path.read_bytes(), "attestation manifest")
    except OSError as error:
        raise ValidationError("could not read attestation manifest") from error
    if not isinstance(manifest, dict):
        raise ValidationError("attestation manifest is not an object")
    exact_keys(
        manifest,
        {
            "schemaVersion",
            "mediaType",
            "artifactType",
            "config",
            "layers",
            "subject",
            "annotations",
        },
        "attestation manifest",
    )
    if manifest.get("schemaVersion") != 2 or manifest.get("mediaType") != OCI_MANIFEST_MEDIA_TYPE:
        raise ValidationError("attestation manifest has the wrong OCI schema or media type")
    string_annotations(manifest.get("annotations"), "attestation manifest")
    artifact_type = manifest.get("artifactType")
    if artifact_type is not None and artifact_type != ATTESTATION_ARTIFACT_TYPE:
        raise ValidationError("attestation manifest has an unexpected artifact type")

    config = validate_descriptor(manifest.get("config"), "attestation config")
    if config["mediaType"] == OCI_EMPTY_MEDIA_TYPE:
        if (
            config["digest"] != OCI_EMPTY_DIGEST
            or config["size"] != 2
            or config.get("data", "e30=") != "e30="
        ):
            raise ValidationError("attestation manifest has an invalid empty config")
    elif config["mediaType"] in {
        "application/vnd.oci.image.config.v1+json",
        "application/vnd.docker.container.image.v1+json",
    }:
        if "data" in config:
            raise ValidationError("legacy attestation config embeds unexpected data")
    else:
        raise ValidationError("attestation manifest has an unsupported config media type")

    layers = manifest.get("layers")
    if not isinstance(layers, list) or not layers:
        raise ValidationError("attestation manifest has no layers")
    validated_layers = [
        validate_descriptor(layer, f"attestation layer {index}")
        for index, layer in enumerate(layers)
    ]
    if len({layer["digest"] for layer in validated_layers}) != len(validated_layers):
        raise ValidationError("attestation manifest repeats a layer digest")

    subject = manifest.get("subject")
    if subject is not None:
        subject = validate_descriptor(subject, "attestation manifest subject")
        if (
            subject["mediaType"] != OCI_MANIFEST_MEDIA_TYPE
            or subject["digest"] != runnable
            or subject["size"] != runnable_size
        ):
            raise ValidationError("attestation manifest subject is not the runnable descriptor")
    elif artifact_type is not None:
        raise ValidationError("OCI-artifact attestation manifest has no subject")

    if any(layer["mediaType"] != IN_TOTO_MEDIA_TYPE for layer in validated_layers):
        raise ValidationError("attestation manifest has a non-in-toto layer")
    in_toto_layers = validated_layers
    predicates: list[str] = []
    for layer in in_toto_layers:
        annotations = string_annotations(layer.get("annotations"), "in-toto layer")
        predicate = annotations.get("in-toto.io/predicate-type")
        if predicate not in SUPPORTED_PREDICATES:
            raise ValidationError("in-toto layer has no supported predicate annotation")
        if layer["size"] > MAX_BLOB_SIZE:
            raise ValidationError("in-toto layer exceeds the BuildKit attestation size bound")
        blob = fetch_registry_blob(
            image, layer["digest"], layer["size"], str(blob_cache) if blob_cache else None
        )
        validate_statement(
            blob,
            predicate,
            runnable,
            expected_package,
            expected_version,
            expected_builder_id,
        )
        predicates.append(predicate)
    return sorted(set(predicates))


def main() -> int:
    arguments = sys.argv[1:]
    blob_cache: Path | None = None
    if arguments[:1] == ["--test-blob-cache"]:
        if len(arguments) < 3:
            arguments = []
        else:
            blob_cache = Path(arguments[1])
            arguments = arguments[2:]
    if len(arguments) != 6:
        print(
            "usage: validate-oci-attestation.py "
            "[--test-blob-cache <directory>] "
            "<image> <attestation-manifest.json> <runnable-digest> <runnable-size> "
            "<package> <version>",
            file=sys.stderr,
        )
        return 2
    image, manifest, runnable, runnable_size_text, package, version = arguments
    if not re.fullmatch(r"[1-9][0-9]*", runnable_size_text):
        raise ValidationError("runnable size must be a positive canonical integer")
    predicates = validate_manifest(
        image,
        Path(manifest),
        runnable,
        int(runnable_size_text),
        package,
        version,
        blob_cache,
    )
    for predicate in predicates:
        print(predicate)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValidationError as error:
        print(f"oci-attestation: {error}", file=sys.stderr)
        raise SystemExit(1) from None
