#!/usr/bin/env bash
# Regenerate riegeli/src/proto_generated/ from riegeli/proto/*.proto.
#
# Pinned toolchain — regeneration is only reproducible with exactly:
#   protoc           34.1             (hard-asserted by protobuf-codegen;
#                                       any other version fails loudly)
#   protobuf-codegen 4.34.1-release   (pinned in scripts/regen-proto/Cargo.lock)
#
# protoc is located via $PROTOC, or `protoc` on PATH. CI runs this in the
# proto-drift job and fails on any diff against the checked-in code.
set -euo pipefail
cd "$(dirname "$0")/.."
# Reuse the workspace target directory so the helper's build outputs do
# not dirty the tree (scripts/regen-proto/ has its own [workspace]).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(pwd)/target}"
cargo run --quiet --locked --manifest-path scripts/regen-proto/Cargo.toml
git status --short riegeli/src/proto_generated/
