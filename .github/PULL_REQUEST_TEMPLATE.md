## What and why

<!-- What changed, and what problem it solves. Link an issue if there is one. -->

## How it was verified

<!--
Delete what does not apply. Anything touching the API, scheduler, agent, or artifact path
needs more than unit tests — see CONTRIBUTING.md.
-->

- [ ] `make check`
- [ ] `make test`
- [ ] `apps/web`: `pnpm check`, `pnpm exec tsc --noEmit`, `pnpm test`
- [ ] `make dogfood` against a live stack
- [ ] Manual check (say what):

## Checklist

- [ ] Docs updated in this change, if behaviour changed
- [ ] `CHANGELOG.md` entry under `## [Unreleased]`, if user-visible
- [ ] New schema is a **new** numbered migration; new columns are nullable or defaulted
- [ ] `fiber-proto` changes are mirrored in `apps/web/src/lib/api.ts` and the docs
- [ ] New effects on the execution path are safe to run twice (steps are at-least-once)
