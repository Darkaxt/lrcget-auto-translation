# Frontend Security Fix

## Specification

Resolve the six npm audit findings blocking PR #12 without weakening audit
checks or changing translation/export behavior. Replace Tailwind 3's vulnerable
scanner dependency graph with Tailwind 4, update brace-expansion compatibly,
and preserve the existing palette, dark mode, controls, and layout.
No application installation or release is included.

## Stages

1. Dependency and styling migration: COMPLETE. Acceptance: zero audit findings,
   passing frontend build/lint/tests, and browser verification of representative
   controls and light/dark styling against the pre-migration CSS.
2. Integration and cleanup: ACTIVE. Acceptance: verified changes committed
   and pushed to PR #12, required GitHub checks passing, merge state verified,
   and expendable verification artifacts removed.

## Constraints

Do not suppress advisories, force incompatible transitive overrides, mutate
installed application data, or retain expendable build output.

## Verification

- `npm audit --json`: zero findings; `braces` removed, brace-expansion 5.0.12.
- `npm run build`, `npm run lint`, `npm test`: pass; existing lint warnings remain.
- Browser comparison against pre-migration production CSS: no differences in
  representative controls, including scoped playback/editor/track controls,
  across light/dark, desktop/mobile, and hover/focus states. Fully-rounded
  radii are compared by their rendered meaning rather than the numeric sentinel.
- Normal brace expansion and deeply nested/chained adversarial input probes pass.
- Final integration requires the Frontend and Windows application GitHub checks.
