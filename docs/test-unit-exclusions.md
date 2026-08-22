# `just test-unit` exclusions registry

`scripts/check-test-unit-coverage.sh` fails the build if a workspace member
(from root `Cargo.toml` `members`) is not gated by the Justfile `test-unit`
nextest branch, gated by `run_unit_tests()` in `scripts/run-tests.sh`, or
listed here with a reason. This is the single place both scripts read from —
add an entry here instead of silencing the check.

Format: one crate per bullet, name in backticks, one-line reason.

- `sprig`: bucket (c), confirmed empty by grep audit (buzz#10). No tests to gate.
- `buzz-pairing-cli`: bucket (c), confirmed empty by grep audit (buzz#10). No tests to gate.
- `git-credential-nostr`: bucket (c), confirmed empty by grep audit (buzz#10). No tests to gate.
- `buzz-pair-relay`: bucket (c), confirmed empty by grep audit (buzz#10). No tests to gate.
- `buzz-relay`: excluded per buzz#7 (11 infra/missing-schema failures, needs a dedicated integration lane) and buzz#16 (a 12th, intermittent, ~1-in-6 failure). Not unit-gate material until both are resolved.
- `buzz-search`: 3 real assertions, 19 legitimately `#[ignore = "requires Postgres"]`. Deferred to slice two with an explicit "gates 3 of 22" disclosure so a green run does not read as full coverage (buzz#10).
- `buzz-test-client`: 6 real assertions, 243 legitimately ignored across 19 binaries (each infra requirement documented in a `//!` header). Deferred to slice two with an explicit "gates 6 of 249" disclosure (buzz#10).
- `buzz-pubsub`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-audit`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-deletion`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-ws-client`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-datastore-tracing`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-relay-mesh`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-dev-mcp`: bucket (a), confirmed green and infra-free (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-media`: bucket (a), confirmed green and infra-free, full invocation (buzz#10). Deferred to a later slice to keep slice one small; no blocking issue.
- `buzz-admin`: 1 test, but has no `--lib` target (`cargo test -p buzz-admin --lib` errors with "no library targets found"). Needs the bin-test invocation shape, a different shape than the rest of this slice. Deferred.
- `countdown-bot` (`examples/countdown-bot`): an example, not a product crate. Whether examples belong in `test-unit` at all is a separate call from this slice.
