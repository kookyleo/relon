---
name: Release checklist
about: Track a Relon public release candidate.
title: "Release checklist: vX.Y.Z"
labels: release
assignees: ""
---

# Release Checklist

## Scope

- [ ] Version and target release date are set.
- [ ] `CHANGELOG.md` names stable core, preview surface, and explicitly not-promised surface.
- [ ] Release notes match `docs/*/guide/release-tiers.md`.

## Gates

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cd docs && npm run docs:build`
- [ ] `./scripts/verify.sh`
- [ ] `./scripts/release-verify.sh`
- [ ] `./scripts/package-dry-run.sh` passes on a clean release branch.

## Release Surface Audit

- [ ] Tier 1/Tier 2/Tier 3 backend wording is consistent across README, docs home, release tiers, performance, host integration, and CLI help.
- [ ] `Backend::Auto + TrustLevel::Trusted` is documented as rejected in the first public release; trusted imports or staged host fns point to `Backend::TreeWalk`.
- [ ] `docs/package.json` and `docs/package-lock.json` describe Relon docs as `0.1`, not `1.0` or `2.0`.
- [ ] Threat Model links are present from README, docs home, introduction, use cases, and playground.
- [ ] User docs do not imply an OS sandbox or multi-tenant boundary from capability/budget posture alone.
- [ ] `Context::sandboxed()` documentation states that it is not a tenant boundary by itself.

## Stdlib Audit

- [ ] Stable user API manifest contains only the first-release module API and language builtins.
- [ ] `docs/*/guide/spec.md` stdlib catalog matches the Stable user API manifest and does not list `std/string.glob_match` as stable.
- [ ] Implementation intrinsics are listed separately and are not recommended user API.
- [ ] `ensure.*` is documented as schema-internal.
- [ ] Legacy runtime-only drift, including `string.glob_match`, is not described as stable stdlib.

## Diagnostics Audit

- [ ] Diagnostics Contract lists every `relon::...` diagnostic namespace used in source.
- [ ] Common CLI diagnostic examples still match the golden contract test.
- [ ] Resource-limit diagnostics describe limit/actual when available, and say when a backend trap cannot provide exact consumed values.
- [ ] First release still makes no JSON diagnostics promise.

## Examples And Docs

- [ ] Example headers include runnable command, recommended backend, and expected output path.
- [ ] Example commands match golden outputs.
- [ ] `cargo test -p relon --examples` compiles and runs host embedding examples.
- [ ] English and Chinese sidebars both include new or changed pages.
- [ ] Docs build is green in CI.
