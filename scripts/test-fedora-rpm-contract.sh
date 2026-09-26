#!/usr/bin/env bash
# Safe on a developer machine: only exercises paths that must refuse mutation.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SMOKE="$ROOT/scripts/test-fedora-rpm.sh"
[[ -f "$SMOKE" ]] || { echo 'FAIL: Fedora installed smoke is missing' >&2; exit 1; }
bash -n "$SMOKE"

guard_mutation() { echo 'MUTATION_ATTEMPTED' >&2; return 97; }
dnf() { guard_mutation; }
rpm() { guard_mutation; }
useradd() { guard_mutation; }
userdel() { guard_mutation; }
install() { guard_mutation; }
mktemp() { guard_mutation; }
export -f guard_mutation dnf rpm useradd userdel install mktemp

for opt_in in '' 0 yes 01; do
  if output="$(MULTICORE_DISPOSABLE_RPM_TEST="$opt_in" bash "$SMOKE" --package /missing.rpm 2>&1)"; then
    echo 'FAIL: smoke accepted missing/invalid disposable opt-in' >&2; exit 1
  fi
  [[ "$output" == *'MULTICORE_DISPOSABLE_RPM_TEST=1'* && "$output" != *MUTATION_ATTEMPTED* ]] || {
    echo 'FAIL: missing opt-in must refuse before any mutation or package query' >&2; exit 1
  }
done
for argument in --unknown --package; do
  if output="$(MULTICORE_DISPOSABLE_RPM_TEST=1 bash "$SMOKE" "$argument" 2>&1)"; then
    echo 'FAIL: smoke accepted invalid arguments' >&2; exit 1
  fi
  [[ "$output" == *'Usage:'* && "$output" != *MUTATION_ATTEMPTED* ]] || {
    echo 'FAIL: invalid arguments reached a mutation' >&2; exit 1
  }
done

echo 'PASS: Fedora smoke opt-in and argument guards refuse before mutation'
