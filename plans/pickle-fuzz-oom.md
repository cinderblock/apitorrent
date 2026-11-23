# Fuzz target `pickle` runs out of memory

## Goal

A crafted Deluge pickle can't make `apitorrent_deluge::pickle::load` allocate without
bound. It fails loudly (`PickleError::TooLarge { limit }`), never truncates. Real Deluge
state files (pokol: about 672 torrents) still load. The found input becomes a regression
test.

## Environment

- Branch `fix/pickle-fuzz-oom` (worktree), pushed to master when done.
- Fuzzing runs on sentinel via `rbuild` from `fuzz/`.
- CI: `.github/workflows/ci.yml` job `fuzz`, 60 s per target, `-rss_limit_mb=2048`,
  AddressSanitizer (the cargo-fuzz default), empty corpus each run.

## Decisions already made

- No deploy to pokol.
- Fix: the memory limit scales with input size, `clamp(len × 256, 1 MiB, 384 MiB)`
  (`memory_limit`). Kept the copy-on-`DUP`/memo design (a shared-node design would hand
  callers a DAG that blows up when walked).

## Plan

1. [x] Claim the ISSUES.md entry (`926efab`).
2. [x] Reproduce on sentinel.
3. [x] Fix: input-proportional limit.
4. [x] Regression tests; 8 × 600 s fuzz run clean.
5. [ ] fmt, clippy, tests; commit; push master; watch CI. (current)

## Findings

- The parser already had a 384 MiB budget (`d426801`), included in the failing commit
  `2925b9a`. No single input exceeded it.
- Reproduced with 8 parallel fuzzers from empty corpora, 300 s: 4 of 8 hit
  `out-of-memory (used: 2052–2104Mb; limit: 2048Mb)`, while the live heap at the time was
  only 55–159 MB (`quarantined: 220981246 bytes`). So the RSS was allocator retention
  from earlier inputs.
- Three of the four saved inputs (196–264 bytes, `DUP` + `NEWOBJ` doubling) each build
  the full 384 MiB budget in about 1.4 s; under a 16 MiB limit they stop in about 50 ms. The fourth (115 bytes) is harmless: it was just running when RSS crossed.
- Realistic synthetic states (`make_state.py` classes, protocol 2): 672 torrents = 308 KB
  → 12.8 MB budget used (41×); 5,000 torrents with up to 2,000 files = 3.8 MB → 243 MB
  (63×). The old doc comment ("tens of thousands of torrents") was wrong; fixed.
- Worst plausible real ratio: a memoized tuple of 1-byte opcodes, about 224×. Hence 256.
- A 16 MiB floor was not enough: 1 of 8 fuzzers still hit 2049 MB at about 70 s, live
  heap 29 MB. ASan keeps freed memory per size class, so RSS tracks each input's peak.
  Fuzz inputs are at most 4 KB, so the floor sets that peak: 1 MiB fixed it.
- With the 1 MiB floor: 8 × 600 s from empty corpora, no OOM, no leak reports, peak RSS
  674–1448 MB, and 8–16 M runs each (was about 0.7 M in 600 s). RSS still creeps slowly
  (allocator retention); CI's 60 s run peaks around 550 MB.
- CI's second input (run 45109815845, in `ff0a2b1`): 192 bytes that *load*, as almost
  384 MiB, in about 1 s. Now `TooLarge` at 1 MiB in about 2 ms. In the regression test.
- Two concurrent `rbuild` runs from the same snapshot collide on the ref (`reference
  already exists`): run one at a time per worktree.

## Progress

- [x] Claim committed
- [x] Fix and tests pass locally (`cargo test -p apitorrent-deluge`, clippy on the crate)
- [x] Fixed fuzz run clean
- [ ] Pushed, CI green
