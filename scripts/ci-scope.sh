#!/usr/bin/env bash
set -euo pipefail
rust=false
image=false
python=false
case "${GITHUB_EVENT_NAME:?event is required}" in
  workflow_dispatch) full=true ;;
  pull_request|push) full=false ;;
  *) echo 'Unsupported CI event' >&2; exit 1 ;;
esac
if [[ "$GITHUB_EVENT_NAME" == push && "${GITHUB_REF:-}" == refs/tags/* ]]; then full=true; fi
if [[ "$full" == true ]]; then
  rust=true; image=true; python=true
else
  [[ "${BASE_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || { echo 'A full base commit is required' >&2; exit 1; }
  changed_files="$(mktemp)"
  trap 'rm -f "$changed_files"' EXIT
  if [[ "$GITHUB_EVENT_NAME" == pull_request ]]; then
    git diff --name-only --no-renames -z "$BASE_SHA...HEAD" >"$changed_files"
  else
    git diff --name-only --no-renames -z "$BASE_SHA" HEAD >"$changed_files"
  fi
  while IFS= read -r -d '' path; do
    case "$path" in
      scripts/ci-scope.sh|.github/workflows/*) rust=true; image=true; python=true ;;
      scripts/test_ci_scope.py) python=true ;;
      Cargo.toml|Cargo.lock|rust-toolchain|rust-toolchain.toml|.cargo/*) rust=true; image=true ;;
      rustfmt.toml|.rustfmt.toml|clippy.toml|.clippy.toml) rust=true ;;
      crates/*/tests/*|crates/*/benches/*) rust=true ;;
      crates/*/*.md) ;;
      crates/*) rust=true; image=true ;;
      smoke/expected-tools.txt) rust=true; image=true ;;
      Dockerfile|.dockerignore|LICENSE|NOTICE|smoke/*) image=true ;;
    esac
  done <"$changed_files"
fi
printf 'rust=%s\nimage=%s\npython=%s\n' "$rust" "$image" "$python" >>"${GITHUB_OUTPUT:?output file is required}"
