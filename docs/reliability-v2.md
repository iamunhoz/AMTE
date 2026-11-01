# AMTE Reliability V2 (experimental)

`reliability-v2` is feature-frozen at checkpoint `c5d649f`; `main` and
`archive/pre-reliability-v2` are excluded. CodeMirror solely owns live text,
selection and undo; Rust/SQLite solely owns durable recovery IDs/sequences and
history; Rust's descriptor-bound filesystem code solely owns manuscript saves.

Live dirty (`clean`/`dirty`), recovery (`pending`/`persisting`/`durable`/`error`)
and file (`idle`/`saving`/`saved`/`error`/`conflict`) are independent. Recovery
success never makes a file clean and save never erases history. Rust assigns a
snapshot sequence inside a committed transaction after validating backend
document binding, client edit/metadata generation, content checksum, baseline,
relative Markdown path/untitled identity and bounded UI metadata.

The `write_recovery_buffer` invoke DTO accepts **dirty snapshots only**: it has
no recovery sequence, payload filename, checksum, or clean-transition token.
Its durable acknowledgement is the only source of the backend-assigned
sequence. Successful backend save/autosave writes the trusted clean checkpoint
from its verified disk bytes before replying; frontend values cannot transition
dirty to clean. A retry after a lost acknowledgement is idempotent, while a
newer dirty client version prevents an older clean checkpoint from hiding it.
A newer dirty row with unreadable
metadata or a bad checksum returns an explicit recovery-corruption error; V2
never silently restores an older clean snapshot in its place.

## Session and Save As authority

Each activation returns an opaque backend-issued session token. Dirty recovery
writes, saves and autosaves echo it; the SQLite `active_sessions` epoch is
checked inside the same transaction that inserts the checkpoint. Switch and
discard deactivate the old epoch under the recovery mutex. The ordering is a
brief `ProjectAuthority` identity/token read followed by recovery I/O; recovery
code never acquires `ProjectAuthority`. A discard conditionally deactivates
exactly its currently active token in the same transaction before it can mark
any buffer/history terminal; an old token is an explicit no-op error and cannot
discard a project reopened by a newer session.

Save As carries the immutable untitled recovery capture, including UI/activity
metadata. After the descriptor-bound file write verifies its baseline hash, a
single transaction retires the untitled binding, creates the saved identity,
copies metadata into a clean checkpoint, selects the saved path, and records
the source-binding/destination/hash/edit-version idempotency tuple. Retired
history remains auditable but is excluded from ordinary restore; the same tuple
returns the committed result, while an occupied destination is rejected.

V2 uses `rusqlite 0.32` with bundled SQLite for reproducible offline Linux
builds and tested WAL behavior (at the cost of a larger native build). It uses
parameterized SQL, `user_version`, WAL, FULL synchronous mode, foreign keys,
busy timeout and integrity checks. The owner-private `recovery-v2` directory
has 0700 and database/WAL/SHM files 0600 permissions. The plaintext DB is not
protection from a hostile same-UID actor. `projects`, `buffers`, append-only
`snapshots`, audit `events` and `legacy_imports` retain at least 20 revisions
and never delete the latest valid dirty snapshot; over-budget writes fail.

`recovery-v1` is read-only migration input: a trusted canonical root/key is
validated first, then every manifest/payload is read with the existing
private/no-follow parser before valid buffers are imported in one SQLite
transaction. `legacy_imports` records `imported-v1` only after commit; existing
V2 rows record terminal `skipped-existing-v2`. Invalid/unreadable legacy input
creates no marker and is retryable after repair; it never modifies legacy
bytes. Startup consults committed V2 recovery/terminal migration state before
it traverses any legacy file, so later-corrupt V1 bytes cannot block a valid V2
restore. Import audit events survive retention even when their original
snapshot is pruned (the event reference becomes null). Corrupt/future V2 data
fails closed without replacement/reimport. Project activation prepares the new
root, persists its selector, and performs optional recovery discard before the
single authority swap; any failure leaves the old project IDs, buffers and
recovery binding active. Close/switch quiesces autosave and
snapshot work before Save All/Discard/Cancel. Discard records a terminal audit
event and retains snapshot bytes; native close fallback remains bounded.

Footer priority: conflict, generic error, committed-file reconciliation error,
committed-file reconciliation pending, file saving/saved, recovery
pending/durable, dirty. Errors/conflicts persist; “Draft protected” is not
“File saved.”

## SaveOutcome and reconciliation

Manual Save, autosave, Save All and Save As use one serialized `SaveOutcome`.
It contains `fileCommitted`, `recoveryCheckpointDurable`,
`selectorSessionStateDurable`, `reconciliationPending`, bounded
`reconciliationErrorCode`/`reconciliationErrorMessage`, the project/session,
document/path and file identity, write revision/baseline hash, saved client edit
version/content BLAKE3 hash, operation key, and (when the live session can be
updated) the document DTO. `fileCommitted` is true only after the staged file
was atomically renamed and re-read; it is never inferred from a recovery row.

The order is authorization snapshot (without locks across I/O), descriptor-bound
staged commit, durable SQLite `save_operations` journal record of the committed
identity/revision, clean checkpoint, in-memory session apply, selector write,
and selector journal acknowledgement. The journal key includes the session,
source/destination identity, revision, client edit version and content hash. An
exact retry verifies the on-disk identity/hash first, then completes only the
missing checkpoint/selector step; it never rewrites bytes or creates another
checkpoint. If a rename completed but the journal could not be written, the
outcome explicitly reports `OPERATION_JOURNAL_PENDING`; a later mismatch reports
`AMBIGUOUS_DISK_STATE` and never overwrites the newer disk file.

`reconcile_save_operation` is the sole non-interactive retry command. It accepts
only the opaque backend-issued operation key and current project/session
authorization—not a picker result, path, content, revision, or document ID. It
verifies the journal source/destination, document, hash, revision, and
descriptor-bound disk identity before completing the checkpoint, Save As
transition, and selector idempotently. During trusted restore only, a pending
journal may be rebound to the freshly minted token/document identity in that
activation transaction after its old live binding, opaque metadata key, saved
version/content hash, and descriptor-read disk hash/identity all match. Save
As retains its untitled source until that same verified retry creates the
destination, without reopening a picker. Forged, cross-project, stale,
source-mismatched, and disk-changed keys are rejected; old tokens cannot invoke
after rotation. Pending keys remain in
recovery metadata until reconciliation is fully durable, allowing a webview
restart to finish Save As without reopening a picker.

`fileCommitted=true` updates the client baseline for the saved version. A newer
editor version remains dirty. A pending checkpoint/selector shows the persistent
Portuguese “arquivo salvo; recuperação pendente/erro” status rather than the
plain success icon, preserves recovery history, and retries ordinary saved-file
reconciliation at most three times with backoff. Save As does not reopen a file
picker in the background. Close, project switch, and Save All require every
buffer to have both a file commit and the required reconciliation acknowledgements;
batch results are evaluated per buffer, so a committed earlier buffer is never
reported as unsaved because a later buffer failed.
Save All and autosave collect every dirty eligible buffer's outcome rather than
stopping at the first failure; transition/close remains blocked until all
required commits and reconciliation acknowledgements are durable.

## Gate

Only synthetic roots below `/tmp/opencode` may run the destructive campaign.
`npm run reliability:gate` is the bounded synthetic campaign. It rejects an externally supplied
`AMTE_RELIABILITY_ROOT` outside that directory, creates a private `mkdtemp`
root there, and removes it in `finally` after every outcome. It never uses XDG
directories, a real manuscript, or kills anything except the test child it
started. A nonzero invariant or subprocess result makes the command nonzero.

```sh
npm run reliability:gate        # bounded synthetic campaign
npm run reliability:gate:full   # bounded campaign plus production browser search
```

`reliability:gate:full` is not a release gate by itself. The documented full
release sequence additionally runs the browser benchmark, lint/build, Rust
fmt/check/test/clippy, a release build, and `git diff --check`.

Each invocation generates one cryptographically system-random unsigned 64-bit
decimal seed, prints it before any subprocess as `AMTE_RELIABILITY_SEED`, and
includes it in the JSON summary alongside three fixed seeds. Replay exactly
with `AMTE_RELIABILITY_RANDOM_SEED=<seed> npm run reliability:gate:full` or
`npm run reliability:gate:full -- --seed <seed>`; the CLI override wins. The
summary's counts are calculated from commands actually run.

The gate summary reports the commands actually executed and dynamically enumerates
the SaveOutcome and staged-file fault tests before it starts. The campaign uses three
fixed replayable seeds (`11`, `29`, `73`) plus the printed random/replay seed, 4,000 model operations, 20 buffers, six
approximately 400 KiB Unicode Markdown buffers, four real child-crash phases,
three SQLite boundary failures, and five staged-save failures. The model does
1,000 deterministic operations per seed (insert/delete/replace/undo-like,
buffer switches, and checkpoints). At every checkpoint it restarts the real
`RecoveryDb`, runs `quick_check` and `integrity_check`, and compares the
durable reference bytes, restored API metadata/content, assigned monotonic
sequence, stale-version rejection, checksum fallback, and retained history.
The bounded aggregate snapshot cap is 4 MiB, per payload cap is 1 MiB, buffer
cap is 64, and ordinary history retains 20 rows per buffer while preserving the
latest valid dirty row.

### Fault coverage and limits

* **Real processes:** a separately spawned Rust test child uses the production
  `RecoveryDb` and is forcibly aborted before its transaction, after payload
  insertion/before commit, during retention, and after commit before an
  acknowledgement. Restart may expose only the old committed bytes or (for
  post-commit ack loss) the new committed bytes; never a partial row. This is
  a bounded four-child loop and each child is waited for.
* **Real SQLite concurrency/corruption:** eight concurrent writers/readers use
  the production WAL database, then the campaign verifies no duplicate sequence
  state, retained latest content, and both integrity checks. Truncated copied
  main DB and copied mismatched WAL fixtures must fail closed and preserve the
  corrupt source bytes. Private-mode rejection is also tested.
* **Simulated SQLite failures:** `SQLITE_FULL` before transaction,
  `SQLITE_IOERR` after payload insertion, and `SQLITE_BUSY` during retention
  are deterministic test-only recovery boundaries, not a custom SQLite VFS nor
  a claim of kernel disk exhaustion. Each rolls back and leaves the prior
  committed snapshot readable. Production connections use a bounded two-second
  busy timeout; callers retry from their normal debounced recovery lifecycle.
* **Simulated file persistence failures:** test-only hooks exercise before
  stage write, after staged-file `fsync`, before rename, after rename/ack loss,
  and before directory `fsync`. They prove exact old-or-new target bytes and
  keep the recovery snapshot available for acknowledgement reconciliation.
* **SaveOutcome destructive matrix:** inert test-only fault points cover after
  rename/before the operation journal, after the journal/before the clean
  checkpoint, after that checkpoint/before selector persistence, and after the
  selector acknowledgement/before the response. Real child processes abort at
  the latter durable boundaries and restart the production SQLite code. The
  restart verifies exact manuscript bytes, a discoverable opaque operation key,
  checkpoint/selector flags, idempotent reconciliation, retained dirty history,
  and no stale clean transition. Manual Save and autosave share the backend
  operation; frontend coverage applies the identical outcome contract to manual
  Save, autosave, every Save All member, and Save As reconciliation without
  reopening its picker.
  The directory-fsync and disk-full cases are boundary simulations; no quota or
  actual full filesystem is claimed. Existing descriptor-bound tests cover
  external hash changes, stale IDs/revisions, symlinks, hardlinks, and private
  path rejection.

This remains an experimental campaign and does not declare V2 released. It is
intentionally bounded to keep local execution
predictable; increase operation counts only with a separately approved stress
run.

## Performance and release criteria

The browser benchmark serves the already-built production frontend from a
private Chromium profile under `/tmp/opencode`; it is not the application
runtime and the packaged release has no localhost dependency. Its ten-buffer,
approximately 400 KiB Unicode Markdown run fails when CodeMirror input/update
p95 exceeds **8 ms** or a buffer-switch p95 exceeds **32 ms** (two 60 Hz frames).
The benchmark reports the browser heap estimate when Chromium exposes it. These
are checkpoint thresholds, not a waiver for any correctness failure.

Release remains blocked unless `npm run reliability:gate:full` reports every
required command successful: the synthetic SQLite/crash campaign, frontend
tests/lint/build/browser benchmark and search check, Rust format/test/check/
clippy, an isolated release build, and `git diff --check`. The release
binary may be launched only with newly-created `XDG_CONFIG_HOME`,
`XDG_DATA_HOME`, `XDG_CACHE_HOME`, and `TMPDIR` below `/tmp/opencode`; it must
not be pointed at a normal profile or a real manuscript. The recovery harness
must prove exact bytes and metadata after a forced child crash and restart,
then remove every fixture and child it created. A human clean-close check is
still required if the target display stack cannot drive the native close event.
