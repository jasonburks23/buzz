#!/usr/bin/env bash
# =============================================================================
# check-test-unit-coverage.sh — comparator for the `just test-unit` gate
# =============================================================================
# The unit-test gate is two hand-maintained crate lists (the nextest branch in
# the Justfile and run_unit_tests() in scripts/run-tests.sh) with no automatic
# relationship to root Cargo.toml `members`. A crate added to the workspace is
# invisible to the gate by default (buzz#5, buzz#8). This script closes that
# gap: it fails loudly, naming the crate, if any workspace member is not
# accounted for in {Justfile nextest list, run-tests.sh list, exclusions
# registry}, and separately fails if the two hand-maintained lists gate a
# different crate set from each other (the exact way a two-branch recipe can
# pass in one shell and not the other).
#
# Run as its own `just` step, before the nextest branch, in both entry points.
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

CARGO_TOML="Cargo.toml"
JUSTFILE="Justfile"
RUN_TESTS_SH="scripts/run-tests.sh"
EXCLUSIONS="docs/test-unit-exclusions.md"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

fail=0

# ---- 1. Workspace members, as bare crate names (== directory basename for
#         every member in this workspace; verified, not assumed) ------------

members=$(awk '/^members = \[/,/^\]/' "${CARGO_TOML}" \
  | grep -oE '"[^"]+"' \
  | tr -d '"' \
  | awk -F/ '{print $NF}' \
  | sort -u)

# ---- 2. Crates gated by the Justfile nextest branch -------------------------

nextest_block=$(awk '/^test-unit:/,/^# Run integration tests only/' "${JUSTFILE}")
nextest_crates=$(echo "${nextest_block}" \
  | grep -vE '^\s*#' \
  | grep -oE '\-p [A-Za-z0-9_-]+' \
  | awk '{print $2}' \
  | sort -u)

# ---- 3. Crates gated by run_unit_tests() in run-tests.sh --------------------

run_tests_block=$(awk '/^run_unit_tests\(\) \{/,/^\}/' "${RUN_TESTS_SH}")
run_tests_crates=$(echo "${run_tests_block}" \
  | grep -vE '^\s*#' \
  | grep -oE '\-p [A-Za-z0-9_-]+' \
  | awk '{print $2}' \
  | sort -u)

# ---- 4. Excluded crates, with a reason, from the registry -------------------

if [[ ! -f "${EXCLUSIONS}" ]]; then
  echo -e "${RED}[coverage-check] missing ${EXCLUSIONS}${NC}" >&2
  exit 1
fi

excluded_crates=$(grep -oE '^\- `[A-Za-z0-9_-]+`' "${EXCLUSIONS}" \
  | grep -oE '`[A-Za-z0-9_-]+`' \
  | tr -d '`' \
  | sort -u)

# ---- 5. Every workspace member must be accounted for somewhere -------------

accounted=$(printf '%s\n%s\n%s\n' "${nextest_crates}" "${run_tests_crates}" "${excluded_crates}" \
  | grep -v '^$' \
  | sort -u)

unaccounted=$(comm -23 <(echo "${members}") <(echo "${accounted}"))

if [[ -n "${unaccounted}" ]]; then
  echo -e "${RED}[coverage-check] workspace member(s) not gated and not excluded:${NC}" >&2
  while IFS= read -r crate; do
    echo -e "${RED}  - ${crate}${NC}" >&2
  done <<< "${unaccounted}"
  echo "Add each to the Justfile test-unit recipe, scripts/run-tests.sh run_unit_tests(), or ${EXCLUSIONS} with a reason." >&2
  fail=1
fi

# ---- 6. The two hand-maintained lists must gate the same crate set ---------
#         (independent of exclusions: this catches "wired in one branch,
#         forgotten in the other" even when every crate is individually
#         accounted for somewhere).

only_in_nextest=$(comm -23 <(echo "${nextest_crates}") <(echo "${run_tests_crates}"))
only_in_run_tests=$(comm -13 <(echo "${nextest_crates}") <(echo "${run_tests_crates}"))

if [[ -n "${only_in_nextest}" || -n "${only_in_run_tests}" ]]; then
  echo -e "${RED}[coverage-check] the nextest branch and the run-tests.sh branch gate different crate sets:${NC}" >&2
  if [[ -n "${only_in_nextest}" ]]; then
    echo "  only in Justfile nextest branch:" >&2
    while IFS= read -r crate; do echo "    - ${crate}" >&2; done <<< "${only_in_nextest}"
  fi
  if [[ -n "${only_in_run_tests}" ]]; then
    echo "  only in scripts/run-tests.sh run_unit_tests():" >&2
    while IFS= read -r crate; do echo "    - ${crate}" >&2; done <<< "${only_in_run_tests}"
  fi
  echo "Both branches must gate the same crates or the fallback silently covers less." >&2
  fail=1
fi

if [[ "${fail}" -eq 0 ]]; then
  echo -e "${GREEN}[coverage-check] every workspace member is gated or excluded with a reason; both branches agree${NC}"
fi

exit "${fail}"
