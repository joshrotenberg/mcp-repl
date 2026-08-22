def sha256:
  type == "string" and test("^sha256:[0-9a-f]{64}$");
def attestation:
  ((.annotations["vnd.docker.reference.type"] // "") ==
    "attestation-manifest");
def string_annotations:
  ((.annotations? // {}) | type) == "object" and
  all((.annotations? // {})[]; type == "string");
def descriptor:
  type == "object" and
  ((keys - [
    "mediaType", "digest", "size", "urls", "annotations", "data",
    "artifactType", "platform"
  ]) | length) == 0 and
  .mediaType == "application/vnd.oci.image.manifest.v1+json" and
  (.digest | sha256) and
  (.size | type) == "number" and .size > 0 and .size == (.size | floor) and
  (.platform | type) == "object" and
  ((.urls? // []) | type) == "array" and
  all((.urls? // [])[]; type == "string" and length > 0) and
  ((has("data") | not) or (.data | type) == "string") and
  ((has("artifactType") | not) or
    ((.artifactType | type) == "string" and .artifactType != "")) and
  string_annotations;
def platform_name:
  .platform.os + "/" + .platform.architecture;

select(type == "object") |
select((keys - ["schemaVersion", "mediaType", "manifests", "annotations"]) |
  length == 0) |
select(.schemaVersion == 2 and
  .mediaType == "application/vnd.oci.image.index.v1+json") |
select(string_annotations) |
select((.manifests | type) == "array") |
select(all(.manifests[]; descriptor)) |
select(( [.manifests[].digest] | length) ==
  ([.manifests[].digest] | unique | length)) |
(.manifests | map(select(attestation | not))) as $runnables |
(.manifests | map(select(attestation))) as $attestations |
select(($runnables | length) > 0) |
select(($attestations | length) >= ($runnables | length)) |
select(all($runnables[];
  ((.platform | keys | sort) == (["architecture", "os"] | sort)) and
  (.platform.os | type) == "string" and
  (.platform.os | test("^[a-z0-9][a-z0-9._-]*$")) and
  (.platform.architecture | type) == "string" and
  (.platform.architecture | test("^[a-z0-9][a-z0-9._-]*$")) and
  ((.annotations["vnd.docker.reference.type"]? // null) == null) and
  ((.annotations["vnd.docker.reference.digest"]? // null) == null))) |
select(all($attestations[];
  . as $descriptor |
  ((.platform | keys | sort) == (["architecture", "os"] | sort)) and
  .platform.os == "unknown" and
  .platform.architecture == "unknown" and
  (.annotations["vnd.docker.reference.digest"] | sha256) and
  any($runnables[];
    .digest == $descriptor.annotations["vnd.docker.reference.digest"]))) |
select([
  $runnables[].digest as $runnable_digest |
  any($attestations[];
    .annotations["vnd.docker.reference.digest"] == $runnable_digest)
] | all) |
[$runnables[] | {
  platform: platform_name,
  runnable_digest: .digest
}] | sort_by(.platform) |
select(length == ([.[].platform] | unique | length)) |
select(length == ([.[].runnable_digest] | unique | length))
