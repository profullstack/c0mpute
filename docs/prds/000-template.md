---
cip: 000
title: "Short imperative title — what ships"
status: Draft
authors:
  - you@example.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-NNNN
depends-on:
blocks:
implementation:
estimate:
---

## Summary

One paragraph. What is being built, and what can a user do at the end of it
that they couldn't before? A reviewer should be able to read this and decide
whether to keep reading.

## Goals

Bulleted, concrete, testable. "Fast" is not a goal; "p99 read latency under
400 ms for a 4 MiB block" is.

## Non-goals

What this phase deliberately leaves undone, especially things a reader will
otherwise assume are included. Point at the CIP that covers each one.

## Design

The actual proposal. Be specific enough that someone else could build it:

- New API surface (routes, CLI flags, config keys, on-disk formats)
- Data structures and wire formats
- Failure modes and what happens in each
- Which crates change, and roughly how

## Acceptance criteria

A numbered checklist a reviewer can actually run. Each item is a command, a
test, or an observable behaviour — not a feeling.

1. `cargo test -p c0mpute-store` passes with N new tests covering X.
2. …

## Risks

What could make this take twice as long, or ship broken. Include the mitigation
for each, or say plainly that there isn't one yet.

## Estimate

A range, with the assumption behind it (one engineer? two? familiar with the
codebase?). Break it down if the phase has distinct chunks.

## Open questions

Things intentionally unresolved. These should shrink to zero before status
moves to Approved.
