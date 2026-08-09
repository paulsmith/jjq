# E2E test performance experiments

Date: 2026-08-09

## Goal

Reduce the wall-clock time of the complete `tests/e2e.rs` suite without
weakening its assertions, isolation, or concurrency coverage.

## Method

- Use a warm Cargo build so compilation does not dominate the result.
- Time `cargo test --test e2e` with `/usr/bin/time -p`.
- Require every non-ignored test to pass; new regression tests remain part of
  subsequent full-suite measurements.
- Compare wall time first, then user and system CPU time to distinguish useful
  work from process/filesystem overhead.
- Re-run the final candidate before keeping it.

## Results

| Experiment | Command | Result | Real | User | System |
| --- | --- | --- | ---: | ---: | ---: |
| Baseline, default test concurrency | `cargo test --test e2e` | 121 passed, 1 ignored | 259.24s | 115.16s | 353.73s |
| Limit libtest to 4 threads | `cargo test --test e2e -- --test-threads=4` | 121 passed, 1 ignored | 308.75s | 114.49s | 146.70s |
| Limit libtest to 8 threads, wait candidate applied | `cargo test --test e2e -- --test-threads=8` | 121 passed, 1 ignored | 267.35s | 116.30s | 208.94s |
| APFS recursive clone microbenchmark | 200 cached Go-project fixture copies | passed | 6.79s | 0.14s | 4.12s |
| Existing recursive `fs::copy` microbenchmark | 200 cached Go-project fixture copies | passed | 2.73s | 0.08s | 2.46s |
| Remove spawn-gate test's fixed grace sleep | focused test, warm | passed three times | 7.60-7.83s | - | - |
| Final, default concurrency with fixed-wait removal | `cargo test --test e2e` | 121 passed, 1 ignored | 251.03s | 114.40s | 359.84s |
| Atomic initialization, final corrected implementation | `cargo test --test e2e` | 124 passed, 1 ignored | 217.92s | 99.87s | 311.83s |
| Single-resolution metadata preflight, sample 1 | `cargo test --test e2e` | 125 passed, 1 ignored | 222.70s | 93.12s | 307.07s |
| Single-resolution metadata preflight, sample 2 | `cargo test --test e2e` | 125 passed, 1 ignored | 197.53s | 93.16s | 301.61s |
| Oversubscribe libtest to 32 threads | `cargo test --test e2e -- --test-threads=32` | rejected after 2 timing-sensitive failures | >102.90s | - | - |
| Persistent metadata workspace + initialized templates + queue snapshot, sample 1 | `cargo test --test e2e` | 125 passed, 1 ignored | 158.57s | 79.17s | 234.14s |
| Pre-described metadata writes + expanded initialized templates, sample 1 | `cargo test --test e2e` | 125 passed, 1 ignored | 133.14s | 72.39s | 226.25s |
| Bulk reads + queued run-all prefixes + crash-recovery fix | `cargo test --test e2e` | 126 passed, 1 ignored | 125.08s | 67.78s | 228.54s |
| Longest-first name scheduling probe | `cargo test --test e2e` | 126 passed, 1 ignored; reverted | 131.92s | 68.15s | 225.20s |
| Warm release-profile suite | `cargo test --release --test e2e` | 126 passed, 1 ignored; rejected | 135.63s | 66.61s | 225.02s |
| Queued single-item prefixes + claim/workspace batching | `cargo test --test e2e` | 126 passed, 1 ignored | 111.92s | 59.71s | 223.85s |
| Non-colocated Git-backed fixtures | `cargo test --test e2e` | 126 passed, 1 ignored; reverted | 109.17s | 59.32s | 185.00s |
| 24 libtest threads on non-colocated fixtures | `cargo test --test e2e -- --test-threads=24` | 126 passed, 1 ignored; rejected | 107.96s | 59.60s | 230.52s |
| Two concurrent 8-thread run/non-run shards | two complementary filtered `cargo test` processes | every test once; rejected | 116.07s | - | - |
| Reviewed final tree, ordinary `TMPDIR` | `cargo test --test e2e` | 129 passed, 1 ignored | 105.21s | 54.35s | 192.90s |
| Reviewed final tree, RAM-backed `TMPDIR`, sample 1 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 34.21s | 57.81s | 287.15s |
| Reviewed final tree, RAM-backed `TMPDIR`, sample 2 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 33.48s | 57.31s | 277.98s |
| Reviewed final tree, RAM-backed `TMPDIR`, sample 3 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 33.72s | 57.29s | 277.32s |
| Semantically compressed tree, ordinary `TMPDIR` | `cargo test --test e2e` | 129 passed, 1 ignored | 120.80s | 53.93s | 191.42s |
| Semantically compressed tree, fresh RAM disk, sample 1 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 34.12s | 55.63s | 236.84s |
| Semantically compressed tree, fresh RAM disk, sample 2 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 34.51s | 55.14s | 253.27s |
| Semantically compressed tree, fresh RAM disk, sample 3 | `scripts/test-e2e-fast` | 129 passed, 1 ignored | 33.88s | 55.91s | 240.41s |

## Ideas

1. Limit libtest concurrency. The baseline's system CPU time is much larger
   than wall time, consistent with many tests concurrently spawning many `jj`
   subprocesses and contending on temporary-repository I/O.
2. Make cached repository fixture copies cheaper while preserving copy-on-write
   isolation between tests.
3. Replace avoidable fixed sleeps or coarse polling in concurrency tests with
   event-driven synchronization or shorter safe polling intervals.
4. If one concurrency value is consistently faster, provide a documented,
   repeatable full-suite command rather than relying on machine-dependent
   libtest defaults.

## Decision log

- The initial `--report-time` idea was rejected because stable Rust's libtest
  only accepts it with nightly `-Z unstable-options`.
- A four-thread cap reduced system CPU by 59% but increased wall time by 19%,
  so it is not a useful default. A middle value may balance parallelism and
  filesystem/process contention better.
- An eight-thread cap also regressed wall time, by 3%, despite including the
  fixed-wait improvement. Test concurrency is therefore left at libtest's
  default.
- Oversubscribing the 16-CPU host with 32 test threads caused severe
  contention and two timing-sensitive tests to fail before the suite reached
  103 seconds. The run was stopped immediately; correctness rules out this
  approach regardless of eventual wall time.
- APFS `copyfile(RECURSIVE | CLONE)` was about 2.5 times slower than the
  existing recursive copy for these many-small-file fixtures. The candidate
  was reverted.
- The spawn-gate crash test contained the dominant fixed wait: 5.2 seconds.
  The candidate instead waits for the test-hook supervisor process to exit,
  verifies that the user check never ran, and backdates only its verified
  PID-less active-check record to 5.2 seconds ago, preserving the original
  grace-boundary assertion without waiting in real time. Other unconditional
  sleeps in the suite total only 400ms and protect intentional timing windows.
- No optimization will be kept solely from a focused-test improvement; the
  complete suite must remain green. For small changes hidden by the suite's
  high wall-clock variance, use a controlled A/B that toggles only the target
  operation and report both full-suite samples.

## Outcome

The semantically compressed final tree has a 34.12s full-suite median using
the one-command RAM-backed runner: 34.12s, 34.51s, and 33.88s. Every sample
created and detached a fresh RAM disk, ran the unchanged default test set,
and passed 129 tests with one intentional manual stress test ignored. Its
ordinary-host-`TMPDIR` control took 120.80s; an earlier same-code control was
105.21s, illustrating the host volume's substantial run-to-run variance.

Relative to the 259.24s baseline, product/test-harness changes reduced the
ordinary-disk run substantially. Moving only temporary test repositories and
jjq workspaces to the RAM-backed `TMPDIR` produced the stable sub-minute path,
for an overall reduction of 225.12s (86.8%) from baseline to the latest
34.12s median. The
project volume was 96% full with 17 GiB free during these measurements, which
made its many-small-file workload particularly expensive.

Run `scripts/test-e2e-fast` on macOS to create a disposable 1 GiB RAM disk,
run the complete suite, and detach the disk on exit. Set `JJQ_E2E_TMPDIR` to
reuse another writable temporary volume. This changes only the I/O location;
it does not filter tests, change assertions, or alter libtest concurrency.

## Command-level `jj` profile

A temporary in-process profiler timed every direct test-harness `jj` call and
every `jj` child launched by jjq. The profiler wrote one file per process so
parallel records could not interleave. The instrumentation was removed after
the run.

The profiled full suite passed all 121 tests in 254.82s and recorded 16,901
`jj` invocations. Their cumulative elapsed time was 3,052.73s; this exceeds
wall time because the suite runs tests and their child commands concurrently.
It corresponds to about 12 `jj` commands active on average during the run.

| `jj` operation | Calls | Cumulative time | Average | Share |
| --- | ---: | ---: | ---: | ---: |
| `workspace add` | 1,224 | 899.01s | 734.5ms | 29.5% |
| `describe` | 1,114 | 650.34s | 583.8ms | 21.3% |
| `workspace forget` | 1,204 | 364.34s | 302.6ms | 11.9% |
| `bookmark set` | 1,119 | 185.92s | 166.2ms | 6.1% |
| `bookmark list` | 3,130 | 184.74s | 59.0ms | 6.1% |
| `log` | 3,355 | 155.37s | 46.3ms | 5.1% |
| `bookmark create` | 450 | 132.91s | 295.4ms | 4.4% |
| `new` | 398 | 129.35s | 325.0ms | 4.2% |
| `file show` | 2,317 | 94.87s | 40.9ms | 3.1% |
| `desc` | 283 | 90.07s | 318.3ms | 3.0% |

The counts identify the main source: `meta::with_meta_workspace` creates a
temporary workspace for every metadata mutation, describes its commit, moves
`jjq/_/_`, and forgets the workspace. There were 1,114 such `describe`
operations, with nearly matching counts for workspace creation, bookmark
movement, and cleanup. Those four mutation primitives alone account for 68.8%
of cumulative `jj` time; the metadata transaction's preceding bookmark,
revision, and file reads add more.

The next optimization target should therefore be metadata transaction churn,
especially commands that perform adjacent `record_history` and workspace or
configuration metadata writes in separate `with_meta_workspace` calls.

## Atomic initialization experiment

`jjq init` previously used four complete metadata transactions: one to create
the initial metadata commit and one for each of `trunk_bookmark`,
`check_command`, and `strategy`. The e2e suite initializes about 104
repositories, making this the safest high-frequency transaction sequence to
consolidate.

Initialization now validates all inputs first and writes all three config
files in one locked metadata transaction. Its single commit description keeps
the four existing operation-log labels. The locked initializer permits a
retry to complete the bare metadata-root state left by interruption before the
first workspace commit, while rejecting completed or concurrent competing
initializations.

The `test_init_with_flags` warm median improved from 1.57s to 0.78s, or about
50%. Its regression assertion pins initialization to the metadata-root commit
plus one initialization transaction. New tests also verify that an invalid
strategy leaves no metadata, interrupted initialization is recoverable, and
two concurrent initializers produce one winner with one complete, untorn
configuration tuple.

The corrected full suite, including the three new tests, passed 124 tests in
217.92s. That is 33.11s (13.2%) faster than the prior 251.03s optimized result
and 41.32s (15.9%) faster than the original 259.24s baseline.

## Single-resolution metadata preflight experiment

Every metadata write previously ran `jj bookmark list` to test whether
`jjq/_/_` existed, then immediately ran `jj log` to resolve the same
bookmark and detect conflicts. The write path now performs one structured
`jj log` resolution with four outcomes: absent, one revision, multiple
revisions, or command/format error. Existing metadata is pinned by its full
commit ID for the subsequent file read, parent validation, and workspace
creation.

A temporary A/B switch restored only the removed `bookmark list` probe. The
benchmark initialized one repository and performed 30 alternating config
writes. Excluding the first compile-contaminated run, the new path's median
was 9.86s versus 10.57s with the old probe: 0.71s (6.8%) faster, or about 23ms
per metadata transaction. The switch and benchmark-only test were then
removed.

Two clean full-suite samples passed 125 tests in 222.70s and 197.53s. Their
25-second spread confirms that suite wall time is noisy, but the 210.12s
median is 7.80s (3.6%) below the prior 217.92s result. The lower sample is
61.71s (23.8%) faster than the original baseline.

Regression coverage verifies successful-empty, single, multiple, and malformed
resolver output; ordinary config-write recovery from an interrupted bare
metadata root; and rejection of conflicted metadata on writes. An independent
review found no correctness or concurrency regressions; its only low-severity
test gap was not injecting an actual failed `jj log` process.

## Structural transaction and fixture experiment

The next combined candidate attacks setup and metadata churn at three levels:

- a persistent metadata workspace replaces per-transaction workspace
  add/forget with a fresh child in one reusable, locked workspace;
- initialized rebase and merge repository states are generated once by the
  real jjq binary and copied for tests where initialization is setup rather
  than behavior under test; and
- queue/running/failed bookmarks are read through one coherent structured
  bookmark snapshot instead of adjacent lists and per-item resolutions.

The first full-suite sample passed all 125 tests in 158.57s, 51.55s (24.5%)
below the preceding two-sample median of 210.12s and 100.67s (38.8%) below the
original baseline. This is a substantial improvement but remains above the
60-second stopping target, so the experiment continues.

For the next iteration, metadata commit descriptions known before mutation are
set by `jj new -m`; the final bookmark-set command snapshots and publishes the
files, eliminating a separate `jj describe` process. Only sequence allocation
retains the post-mutation description because its message contains the newly
allocated ID. Six more initialized scenario templates cover fixed-failure and
run-all setup states.

That iteration passed all 125 tests in 133.14s, another 25.43s (16.0%) below
the previous sample and 126.10s (48.6%) below the original baseline.

Bulk pinned config/history reads and queued run-all prefix templates then
reduced the suite to 125.08s while adding a crash-boundary regression. An
adversarial review found that the persistent metadata workspace could expose
an unpublished crashed transaction as an unbookmarked head; recovery now
abandons only that unbookmarked descendant and preserves bookmarked ancestors.

Two scheduling/build probes were rejected. Prefixing eight long test names to
start them earlier regressed to 131.92s and was reverted. A separately warmed
release-profile test binary regressed to 135.63s, showing that Rust debug
execution is not the dominant remaining cost.

Generic one-item queued templates and combining CLAIMED history with workspace
recovery metadata removed another 13.16s, reaching 111.92s. A failed-item
template was rejected before a full run because its retained workspace path
canonicalized into the shared template directory, violating test isolation.

Non-colocated Git-backed fixtures reduced system CPU substantially but improved
wall time by only 2.75s; the change was reverted to preserve the suite's
original colocated compatibility coverage. Raising libtest to 24 threads saved
only another 1.21s. Splitting run and non-run tests into complementary
eight-thread processes made workload balance worse (the slower shard took
116.07s), so sharding was also rejected.

## Final product and harness optimizations

The retained follow-up changes reduce subprocess count without weakening the
state transitions being exercised:

- config and history reads bulk-load their files from one pinned metadata
  revision;
- each claim records its CLAIMED history event and recovery workspace data in
  one metadata transaction;
- serial and speculative queue claims use atomic bookmark rename, while the
  legacy dual-bookmark recovery path remains intact;
- internal bookmark reads and exact internal bookmark mutations use
  `--ignore-working-copy`, but candidate resolution, metadata publication,
  and working-copy move-off remain snapshot-aware;
- initialized and queued fixture prefixes are cached only for tests where
  init/push are setup, while dedicated init, push, publication, history, and
  concurrency tests continue to perform the real operations; and
- the runner spawn-gate test records the spawned child PID, allowing it to
  preserve the grace-boundary assertion without a fixed 5.2-second wait.

A controlled dirty-working-copy microbenchmark of 30 internal bookmark
create/delete pairs took 2.73s through the ordinary path and 1.54s with
`--ignore-working-copy`, a 44% reduction. The final ordinary-`TMPDIR` suite
passed 129 tests in 105.21s.

Two cache ideas were rejected for correctness or value. A failed-item template
retained absolute recovery-workspace metadata pointing into the template, so
it was removed before a full run. A successful-landed template had only one
consumer and no net setup saving. A `jj-lib` in-process rewrite was also
deferred: it would tightly couple jjq to jj's pre-1.0 internals for a much
larger implementation and compatibility surface.

## Crash safety and independent review

The persistent metadata workspace runs a precise recovery check before each
transaction. It abandons only an unbookmarked workspace commit descending
from the pinned authoritative metadata parent, so a crashed unpublished write
cannot leak into the next transaction. A proposed marker/sentinel fast path
was removed after review because its extra protocol and fsync machinery was
not necessary to meet the target; unconditional precise recovery is simpler
and preserves the crash-boundary regression.

Adversarial review found one real blocker before final timing: `jjq-meta` can
also be a pre-existing user workspace name. Initialization and `clean` now
resolve that registration and require it to point at jjq's fixed internal
metadata-cache path before forgetting or deleting anything. The regression
test creates a colliding user workspace and verifies both its registration and
files survive the actionable failure.

Additional regressions verify that two clones of one queued template have
relocated, clone-local persistent metadata workspaces and remain isolated
after config, queue, and history mutations. Snapshot normalization covers both
raw and canonical spellings of a custom `TMPDIR` while preserving surrounding
punctuation.

## RAM-backed temporary repository experiment

The same final tree was measured once with the ordinary host `TMPDIR` and
three times with a 1 GiB HFS+ RAM disk supplied only through `TMPDIR`. Cargo's
build output remained on the project disk; the environment switch relocates
the tempfile-backed fixture repositories and jjq runner/check workspaces that
dominate the workload. The ordinary control was 105.21s. RAM-backed samples
were 34.21s, 33.48s, and 33.72s, for a 33.72s median.

This is intentionally reported as an execution-environment optimization, not
as evidence for a particular fixture-copy implementation. The test binary,
default concurrency, assertions, and test selection are identical. All three
samples passed 129 tests with zero failures and the same one manual stress test
ignored. `scripts/test-e2e-fast` creates and validates an exact RAM-disk device,
mounts a process-unique volume, and detaches that device through an exit trap.

## Semantic compression follow-up

Three independent implementation agents applied the simplicity review in
non-overlapping areas, followed by cross-review:

- sequence allocation now uses a stable pre-known operation description,
  removing the last post-mutation metadata-description mode and one `jj
  describe` subprocess per allocated ID;
- one strict queue-state bookmark parser replaces three namespace regexes and
  duplicated ID-list logic;
- structured revset resolution is the single pinned primitive, config reads
  use the closed three-key contract, and single-item status no longer carries
  impossible fallback branches;
- cached fixture states remain unchanged, but 24 one-to-one `with_*` wrappers
  and repeated template-construction closures were collapsed; and
- history recording borrows event slices and shares decoding logic while
  preserving the distinct legacy error wording of `history ID` and `history
  --change`.

The product/test source diff is 228 net lines smaller than the pre-compression version;
fixture/history source alone fell by 115 lines despite adding a decoder
regression test. Cross-review caught and corrected the history error-message
compatibility drift before full testing. The final verification passed 12
unit tests, the complete 129-test e2e suite on ordinary storage, and three
complete fresh-RAM-disk samples with a 34.12s median.
