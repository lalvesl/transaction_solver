# transaction_solver

[![CI](https://github.com/lalvesl/transaction_solver/actions/workflows/ci.yml/badge.svg)](https://github.com/lalvesl/transaction_solver/actions/workflows/ci.yml)

A toy payments engine. It streams a CSV of transactions, applies them to per-client
accounts, handles disputes / resolutions / chargebacks, and writes the resulting account
states to stdout as CSV.

```console
$ cargo run --release -- transactions.csv > accounts.csv
$ cat transactions.csv | cargo run --release > accounts.csv
```

Transactions can be read from a file path argument or from **stdin** (if no argument is provided or `-` is specified). The result goes to **stdout**; every diagnostic goes to **stderr**, so redirecting stdout always yields a clean CSV.

---

## Input and output

Input columns are `type`, `client`, `tx`, `amount`. Leading and trailing whitespace is
trimmed from the header and from every field, so both of these parse identically:

```csv
type, client, tx, amount
deposit, 1, 1, 1.0
```

```csv
type,client,tx,amount
deposit,1,1,1.0
```

Output columns are `client`, `available`, `held`, `total`, `locked`:

```csv
client,available,held,total,locked
1,1.5000,0.0000,1.5000,false
2,2.0000,0.0000,2.0000,false
```

Amounts are always printed with exactly four decimal places. Since input amounts with more
than four decimal places are rejected (see [D6](#d6-amounts-with-more-than-four-decimals-are-rejected)),
this is pure formatting — no value is ever rounded on the way out.

Rows are emitted in ascending client order. Ordering is explicitly irrelevant to the
specification, but a deterministic output is what makes golden-file testing possible, and
sorting at most 65,536 rows once, at the very end, costs nothing worth measuring.

---

## Transaction semantics

`amount` is only read for deposits and withdrawals. Disputes, resolutions and chargebacks
reference a prior transaction by `tx` and carry no amount of their own; if one is present
it is ignored.

| Type         | Precondition                                            | Effect                                                     |
| ------------ | ------------------------------------------------------- | ---------------------------------------------------------- |
| `deposit`    | account not locked, amount valid                        | `available += amt`, `total += amt`; kept for later dispute |
| `withdrawal` | account not locked, amount valid, `available >= amt`    | `available -= amt`, `total -= amt`                         |
| `dispute`    | referenced tx exists, same client, currently undisputed | `available -= amt`, `held += amt`, `total` unchanged       |
| `resolve`    | referenced tx exists, same client, under dispute        | `held -= amt`, `available += amt`, `total` unchanged       |
| `chargeback` | referenced tx exists, same client, under dispute        | `held -= amt`, `total -= amt`; **account locked**          |

Each stored transaction moves through a small state machine, and both terminal states are
final:

```
Undisputed ──dispute──> Disputed ──resolve────> Resolved     (terminal)
                             └────chargeback──> ChargedBack  (terminal)
```

---

## Architecture

```
src/
  main.rs      CLI wiring: open the input, run the pipeline, write the accounts
  cli.rs       argument parsing
  pipeline.rs  the streaming read loop, and where a rejected record is reported
  record.rs    a CSV row, and the validated Transaction it becomes
  amount.rs    validated monetary amounts
  engine.rs    routes a transaction to the account it belongs to
  account.rs   balances, disputable history, and every rule that moves money
  output.rs    rendering the final accounts as CSV
  error.rs     RecordError (skip the record) and FatalError (stop the run)

examples/
  generate_transactions.rs   seeded generator for the benchmark

tests/
  cli.rs       end-to-end tests against the real binary
  data/        sample inputs and their golden outputs

nix/
  rust.nix     the pinned toolchain, build platform, package and lints
  bench.nix    the generator, the benchmark inputs and the runners

bench/
  history.jsonl  one record per benchmark run
```

`flake.nix` is wiring only: it imports `nix/rust.nix` and `nix/bench.nix` and assembles
their results into outputs, so the build definition and the benchmark definition can be
read on their own.

The generator is an example rather than a second `[[bin]]` on purpose: a crate with two
binaries makes bare `cargo run` ambiguous, and the specification's `cargo run --
transactions.csv` has to keep working exactly as written.

Dependencies run one way: `main` → `pipeline` → `engine` → `account`, with `record` and
`amount` validating at the boundary and `output` reading the result. Everything below
`pipeline` is free of I/O, which is why the engine can be driven from a `&[u8]` in a unit
test and from a socket in a server without changing a line of it.

---

## Decisions and assumptions

The specification leaves a number of cases undefined. Where it does, the guiding rule was
"what would a bank do", with a bias toward never silently inventing or destroying money.

### D1. Money is `rust_decimal::Decimal`, never a float

Binary floating point cannot represent `0.1` exactly, and a payments engine that
accumulates rounding error across millions of rows is simply wrong. `Decimal` is a
128-bit fixed-point type with 28–29 significant digits, which covers four decimal places
with room to spare, and it round-trips through serde exactly as written.

All arithmetic uses the `checked_*` variants. `Decimal`'s operators panic on overflow;
overflow is unreachable with realistic balances (the type saturates around `7.9e28`), but
input is untrusted, and a payments engine must not be crashable by a crafted CSV. An
overflowing operation is reported and the row is discarded, leaving the account untouched.

### D2. `available` may go negative

The scenario in the problem statement — deposit fiat, buy and withdraw crypto, then
reverse the fiat deposit — necessarily produces a negative available balance. Refusing the
dispute would be worse: it would let a client escape a reversal simply by having spent the
money first, which is precisely the fraud we are meant to detect.

A negative available balance is exactly how a real account behaves when a deposit is
clawed back after the funds were spent: the client owes the institution. `total` and
`held` stay internally consistent throughout, and `total = available + held` holds at every
point.

### D3. A locked account is locked for everything

The specification only says the account is frozen after a chargeback. This engine takes
that literally: once `locked` is true, **every** subsequent row referencing that client is
rejected — deposits, withdrawals, disputes, resolutions and further chargebacks alike.

A frozen account is a legal hold, not a partial restriction; nothing should move without
manual intervention, which is outside this engine's scope. One consequence is worth
stating: if a second dispute is open when a chargeback freezes the account, those held
funds stay held forever. That is the correct outcome — releasing them automatically would
mean an unreviewed payout on an account already known to be compromised.

### D4. Dispute rows must match the transaction's own client

A `dispute`, `resolve` or `chargeback` whose `client` differs from the client of the
referenced transaction is rejected. The specification never mentions this, but the
alternative lets any client freeze or drain any other client's account just by guessing a
transaction ID. It also has a pleasant structural consequence — see
[Concurrency](#concurrency).

### D5. No re-disputes

Once a transaction is `Resolved` or `ChargedBack`, it can never be disputed again.

This is a deliberate simplification, and it is the one place where the engine knowingly
diverges from real fiat practice. In most jurisdictions a claim resolved against the
cardholder can be re-opened, escalated, or arbitrated — real dispute lifecycles are
multi-round. But the specification describes a single-shot lifecycle ("if the tx isn't
under dispute, you can ignore the resolve"), and modelling re-openings would require
concepts it never introduces: claim IDs, round counters, deadlines. Single-shot keeps
the state machine total, trivially auditable, and impossible to cycle indefinitely.

Because the terminal states are unreachable afterwards, a settled transaction is _deleted_
rather than marked, which is what lets memory come back (see [Efficiency](#efficiency)).
The visible consequence is small: a later reference to a resolved transaction is rejected
as an unknown transaction rather than as a settled one. The record is rejected either way;
only the wording of the diagnostic differs.

### D6. Amounts with more than four decimals are rejected

The specification guarantees at most four decimal places, so this cannot occur in valid
input. When it does occur, the row is rejected rather than rounded: silently re-scaling a
client's money is a worse failure than dropping a malformed instruction, and any rounding
policy we picked would be one we invented.

### D7. Negative amounts are rejected; zero is accepted

A negative deposit is a withdrawal wearing a disguise — it inverts the meaning of the
operation and would bypass the sufficient-funds check entirely. It is rejected.

Zero is accepted as a valid no-op. It is a representable, correctly-formatted amount that
simply moves nothing, and rejecting it would additionally drop the client from the output
(see [D9](#d9-only-valid-transactions-create-an-account)) over a row that is harmless.

### D8. Duplicate transaction IDs: first one wins

Transaction IDs are globally unique per the specification, so a repeat is a partner error.
The first occurrence is kept and the duplicate is rejected. Overwriting would silently
rewrite the amount of a transaction that may already be under dispute.

Two limits are worth stating rather than leaving to be discovered. Detection is
_per-client_, because the history is nested inside the account, so the same ID reused by
two different clients goes unnoticed. And it lasts only as long as the record does: an ID
belonging to a transaction that was resolved and dropped can be reused. Both follow from
the nesting that buys the properties described under [Efficiency](#efficiency), and
neither is reachable from input that honours the specification's own uniqueness guarantee.

### D9. Only valid transactions create an account

An account record is created when a deposit or a withdrawal names the client — including a
withdrawal that then fails for insufficient funds, since the client was legitimately
referenced and the row was well-formed.

Rows rejected as partner errors never create anything. A dispute against an unknown
transaction ID does not conjure a zero-balance account for a client we have otherwise
never seen; neither does an unparseable row. Reporting an account that never existed is
inventing data.

### D10. Unknown transaction types are rejected, not fatal

An unrecognised value in the `type` column is treated like any other malformed row: the
row is reported and skipped, and processing continues. The specification's framing —
malformed input is "an error on our partner's side" — applies to the type column as much
as to the amount.

### D11. Transaction types are matched case-insensitively

`deposit`, `Deposit` and `DEPOSIT` are the same instruction. The specification only ever
writes them in lower case and says nothing about capitalisation, but it does insist that
whitespace be tolerated, and the same reasoning applies: dropping a real movement of money
over the case a partner happened to send is the worse failure. The comparison is ASCII and
allocates nothing, so the leniency is free.

---

## The `dispute-withdraw` feature

**Default: off. Withdrawals cannot be disputed.**

Under the default build, a dispute against a withdrawal is rejected exactly like a dispute
against an unknown transaction, and withdrawals are never retained in the ledger at all.
This matches the plain reading of the specification, whose dispute arithmetic ("the
client's available funds should decrease by the amount disputed") only makes sense for a
credit.

That reading is not how a bank behaves. A withdrawal a client did not authorise — a
stolen card, a hijacked session — is the single most common fiat dispute there is, and
the outcome converges almost everywhere on the institution fronting the funds and
releasing them to the client once the claim is upheld. Building an engine that structurally
cannot express that felt wrong, so the behaviour exists behind a Cargo feature:

```console
$ cargo run --features dispute-withdraw -- transactions.csv > accounts.csv
```

It is a compile-time switch, not a runtime flag, so the default build carries **zero**
cost for it: no branch in the hot path, and no ledger entries for withdrawals.

### Semantics when enabled

A disputed withdrawal is a _provisional credit_, not a debit. The institution fronts the
disputed amount into the held bucket while the claim is investigated:

| Step                        | Effect                                                       |
| --------------------------- | ------------------------------------------------------------ |
| `dispute` of a withdrawal   | `held += amt`, `total += amt`, `available` unchanged         |
| `resolve` (claim denied)    | `held -= amt`, `total -= amt` — the money stays gone         |
| `chargeback` (claim upheld) | `held -= amt`, `available += amt`, `total` unchanged; locked |

Worked example:

```
                    available   held   total
deposit 5.0             5.0      0.0     5.0
withdrawal 5.0          0.0      0.0     0.0
dispute                 0.0      5.0     5.0     provisional credit held
  ├─ resolve            0.0      0.0     0.0     claim denied
  └─ chargeback         5.0      0.0     5.0     withdrawal reversed, account locked
```

Note that `chargeback` still means "the transaction is reversed", consistent with the
deposit case — it is just that reversing a debit returns money to the client rather than
taking it away. `total = available + held` holds at every step here too.

---

## Error handling

Errors are split into two kinds, both defined with `thiserror`:

- **Record errors** — a single row is bad. The row is skipped, a formatted diagnostic
  naming the record's line number, the transaction ID and the reason goes to stderr, and
  processing continues. Everything the specification tells us to "ignore" lands here:
  unknown transaction ID, transaction not under dispute, client mismatch, duplicate ID,
  malformed or missing amount, unknown type, insufficient funds, locked account. "Ignore"
  is interpreted as _do not apply, but do report_ — a payments engine that discards rows
  without a trace is not auditable, and these rows are exactly the signal an operator
  needs to spot a misbehaving partner.
- **Fatal errors** — the run cannot continue: missing or unreadable argument, I/O failure
  on the input, or a failed write to stdout. These abort with a message on stderr and a
  non-zero exit code.

A run that rejects rows still exits `0`. Partner errors are expected traffic, not a
failure of the engine.

There are no `unwrap`s, `expect`s or panicking arithmetic on any input-derived value
outside tests, and the crate sets `#![forbid(unsafe_code)]`.

Two panics were found while building this and designed out rather than assumed away. Both
are reachable from a crafted CSV, which is the standard a payments engine has to meet:

- `Decimal`'s arithmetic operators panic on overflow. Every balance change therefore uses
  `checked_add` / `checked_sub` and reports a record error instead. A deposit of the
  largest representable value followed by another deposit is rejected, not a crash.
- `rust_decimal` panics when asked to format with an explicit precision — `{:.4}` — above
  roughly 10²⁷, and a single deposit is enough to put a balance there. The output therefore
  renders through the plain `Display` and pads the fraction itself, which cannot fail.
  There is a test that formats `Decimal::MAX` and `Decimal::MIN`.

---

## Efficiency

**Input is streamed; output cannot be.** Records are pulled one at a time into a reusable
buffer, so the input file is never materialised: peak memory tracks the engine's state,
not the file's size. The output is a different matter, and it is worth being precise about
why. A dispute on the final line can still move an account first touched on the first
line, so no account is provably final until EOF and nothing can be emitted early. The one
exception falls out of [D3](#d3-a-locked-account-is-locked-for-everything): a locked
account can never change again, so it _could_ be flushed the moment it locks. It is not
worth doing — client IDs are `u16`, so the whole output is capped at 65,536 rows, and
there is no memory pressure on that side to relieve.

**State.** Two nested structures are retained, and nothing else:

- `Engine` holds `HashMap<u16, Account>`, allocated once at `u16::MAX` capacity.
- Each `Account` holds two `Decimal` balances and its disputable history, itself a
  `HashMap<u32, TxRecord>` of `{ amount, state }` at 20 bytes per record.

Nesting the history _inside_ the account is what makes the design work. It enforces
[D4](#d4-dispute-rows-must-match-the-transactions-own-client) structurally rather than by a
check, it shards along with the accounts (see [Concurrency](#concurrency)), and it lets a
locked account release its whole index at once, because `History::Locked` holds no map at
all.

Records are also released as soon as they stop being disputable. A resolved transaction
can never be disputed again ([D5](#d5-no-re-disputes)), so it is dropped rather than
carried for the rest of the run, and only what remains disputable is retained — by default
deposits only, since withdrawals are never recorded at all.

All of this is measured rather than asserted — see [Benchmark](#benchmark) for a
reproducible 1 GiB run of both the retention-heavy and the release-heavy case.

**On pre-sizing the client table.** `HashMap::with_capacity(u16::MAX)` reserves about
13.7 MB of address space up front, which sounds worse than it is: only the 128 KB of
control bytes are actually written, so the untouched entry array never becomes resident —
hence a 2.7 MB baseline for the entire process. In exchange the table can never rehash
mid-run, which removes both the growth spikes and any chance of a multi-megabyte table
copy landing in the middle of a hot loop.

The one genuinely unbounded dimension is the retained history: transaction IDs are `u32`,
so an adversarial file could ask us to keep billions of records. Two-pass processing would
fix it — a first pass collecting every ID that a `dispute` row names, a second retaining
only those — turning retention from O(deposits) into O(disputed transactions). It is
deliberately not implemented: it requires a seekable input, which rules out the socket case
the specification asks about, and at this size a single-pass engine is the better trade.

---

## Benchmark

```console
$ nix run .#bench            # 1 GiB, balanced
$ nix run .#bench-settled    # 1 GiB, settled
$ nix run .#bench-small      # 16 MiB, what CI runs
```

**The input is a derivation, not a file in `/tmp`.** Each profile is a store path built by
`nix build .#bench-input`, which means Nix knows it exists, rebuilds it when it is
missing, and `nix store gc` reclaims the gigabyte once nothing references it. An earlier
version generated into `$TMPDIR` from the runner script, which left a gigabyte somewhere
Nix could neither guarantee nor collect. The runner also times the **packaged** binary —
the one `nix build` produces — rather than whatever a local `cargo` last left in
`./target`.

**Reproducible by construction.** `examples/generate_transactions.rs` carries its own
SplitMix64 instead of depending on a random-number crate, whose stream is free to change
between releases. The same seed produces byte-identical input on any machine, in any year,
and a benchmark number is only comparable against another measured on the same bytes.

That is enforced rather than asserted for the small profile: `bench-input-small` is a
fixed-output derivation with its hash pinned in `nix/bench.nix`, so Nix verifies the bytes
after generating them and CI fails if the generator ever stops being deterministic. The
1 GiB inputs deliberately leave the hash off, so that changing the generator does not
break the build with a hash mismatch on a file that takes a minute to rebuild.

The generator emits _valid_ work rather than noise. A dispute always names a real,
currently-undisputed deposit belonging to that same client, and a resolve or chargeback
always names one of that client's open disputes. Once it charges a client back it stops
emitting for them, because every later record for a frozen account would only be rejected,
and that would benchmark the diagnostics instead of the engine.

Two mixes bracket the memory behaviour:

| Mix                  | Composition                                                              | What it exercises                                                                        |
| -------------------- | ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| `balanced` (default) | 62% deposit, 30% withdrawal, 5% dispute, 2.96% resolve, 0.04% chargeback | The realistic case: most deposits are never disputed, so they stay disputable to the end |
| `settled`            | 34% deposit, 33% dispute, 33% resolve                                    | Records are released as fast as they are created, so retention stays flat                |

Chargebacks are deliberately rare. Each one freezes a client permanently, and a log that
froze every account early would stop measuring anything.

### Results

1 GiB of input, 65,535 clients, release build, on 8 cores and 15 GB of RAM:

| Mix        | Records    | Wall   | Throughput               | Peak RSS | Rejected |
| ---------- | ---------- | ------ | ------------------------ | -------- | -------- |
| `balanced` | 34,531,441 | 26.7 s | 1.29M records/s, 40 MB/s | 762 MB   | 30,978   |
| `settled`  | 40,589,904 | 31.9 s | 1.27M records/s, 34 MB/s | 257 MB   | 0        |

Three things worth reading out of that table.

**The input is never held.** Both runs process a gigabyte; neither comes close to holding
one. Peak memory tracks retained history, not file size.

**Releasing settled records works.** `settled` processes 18% _more_ records than
`balanced` in the same gigabyte, and peaks at a third of the memory, because a resolved
transaction is dropped rather than carried. One honest caveat: `HashMap::remove` frees the
entry but not the table's capacity, so 257 MB is the high-water mark of concurrently-open
records rather than the current count. Removal prevents growth; only locking an account
returns memory outright, by dropping the whole map.

**Per-record cost barely moves between the two.** 1.29M against 1.27M records per second,
about 2% apart, even though `settled` spends two thirds of its records on dispute and
resolve lookups where `balanced` mostly parses and adds. The gap in MB/s is much wider —
40 against 34 — but that is a property of the input, not the engine: reference rows carry
no amount, so they are shorter, and the same gigabyte holds 18% more of them. An earlier
measurement put `settled` 16% slower per record; re-running it against the store-built
input did not reproduce that, so the honest reading is that the two are close and the
first number was noise.

The 30,978 rejections under `balanced` are almost all withdrawals refused for insufficient
funds — the generator does not track balances, only references.

The engine's own output is stable too: two runs over the same 1 GiB input produced the
identical hash `80489f21…`, confirming that nothing about hash-map iteration order leaks
into the result.

### Metrics and history

Each run reports wall time, user and system CPU, CPU utilisation, peak RSS, major and
minor page faults, voluntary and involuntary context switches, and throughput in both
records per second and MB/s.

It then appends one JSON record to `bench/history.jsonl` — override with `BENCH_HISTORY` —
and prints the delta against the previous run over the same input:

```
  vs previous  wall +9.7%, peak rss -3.2%
```

Every record carries the commit, the input's store path, the machine's core count and
kernel, all of the metrics above, and the output's SHA-256. A regression can therefore be
traced back to a commit, and a number measured on one machine is never silently compared
against one measured on another. Because an input is identified by its store path, records
for different profiles share one file without ambiguity.

---

## Concurrency

The engine is single-threaded. It is, however, deliberately structured so that scaling it
to many concurrent streams is a wiring change rather than a rewrite:

- The engine reads from any `impl Read`, not from a path — a file, a socket, or a
  `Cursor` in a test are interchangeable.
- All state lives inside the engine value. There are no globals, no statics, no shared
  mutable state, and the engine is `Send`.
- Input parsing and output rendering are separate from the state machine that applies
  transactions, so a server would reuse the middle layer untouched.

To handle thousands of concurrent TCP streams, the natural shape is to shard by client:
route each record to worker `hash(client) % N`. Per-account ordering — the one ordering
guarantee that actually matters here, since the specification only promises chronological
order within the file — is preserved automatically, because every transaction for a given
client is handled by the same worker in arrival order. No cross-shard locking is needed.

This works precisely because of [D4](#d4-dispute-rows-must-match-the-transactions-own-client):
since a dispute is only valid when it names the transaction's own client, a shard never
needs to reach a transaction belonging to another shard's client. The disputable history
already lives inside each account, so it shards with them for free. Without that rule the
history would have to be one shared, globally-locked index, and the sharding would buy
much less.

What is deliberately _not_ here: a thread pool or an async runtime. Both would add
meaningful surface area to a program whose actual job is a state machine, and neither can
be tested honestly against a single input file.

---

## Correctness

The invariant `total = available + held` is enforced structurally rather than by
convention. `Account`'s fields are private, `total` is not stored but derived from
`available + held`, and the only ways to mutate an account are a handful of methods that
each move funds between the two buckets or in and out of the account as a unit. It is not
possible to write code elsewhere in the crate that updates `available` and forgets `held`.

Types carry the rest. The transaction kind is an enum, not a string, so exhaustive matches
catch any unhandled case at compile time; the dispute state is an enum whose transitions
live in one place; and an amount is parsed and validated once, at the boundary, so the
core never handles an unvalidated value.

Testing — 54 tests in the default build, 56 with the feature enabled:

- **Unit tests** on every transition of the dispute state machine, including every
  rejected one; the account invariants; amount parsing and validation; the CSV read loop
  (whitespace, CRLF, reordered headers, omitted columns, records that fail to parse); and
  output rendering, including values large enough to have crashed a naive formatter.
- **Integration tests** run the real binary against the sample files in `tests/data/` and
  compare stdout byte for byte with a committed golden file. Each file targets a decision
  above:

  | File                       | Covers                                                                                                   |
  | -------------------------- | -------------------------------------------------------------------------------------------------------- |
  | `spec_example.csv`         | the example from the specification, verbatim                                                             |
  | `whitespace.csv`           | padded fields, an omitted trailing column                                                                |
  | `dispute_lifecycle.csv`    | dispute → resolve, dispute → chargeback, the lock                                                        |
  | `reversal_after_spend.csv` | the fraud scenario, ending in a negative balance ([D2](#d2-available-may-go-negative))                   |
  | `precision.csv`            | four-decimal arithmetic, and padding a round value                                                       |
  | `locked_account.csv`       | everything rejected after the lock, funds stranded ([D3](#d3-a-locked-account-is-locked-for-everything)) |
  | `partner_errors.csv`       | all twelve rejection paths in one run                                                                    |
  | `withdrawal_dispute.csv`   | both feature configurations, with a golden file for each                                                 |

- Further end-to-end tests assert what an automated harness depends on: that stdout stays
  parseable CSV while records are being rejected, that a missing input file exits non-zero
  with an empty stdout, and that the argument is required.

Two tools check the checks. `cargo mutants` reports **48 mutants caught, 0 missed** (58
generated, 10 unviable) — the tests constrain the logic rather than merely executing it.
Its first run was more useful than that number suggests: it found three real gaps, and
fixing them removed dead code, replaced an arithmetic underflow in the output formatter
with a `saturating_sub`, and added the two tests that now cover losing the input stream
mid-run and a record that is not valid UTF-8. `cargo deny` passes advisories, bans,
licences and sources against the policy in `deny.toml`.

```console
$ cargo nextest run                             # default build
$ cargo nextest run --features dispute-withdraw # with withdrawal disputes
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
```

---

## Toolchain and dependencies

MSRV is **1.74.0**, declared as `rust-version` in `Cargo.toml` and pinned in `flake.nix`,
so the development shell and CI cannot drift apart. It is set by the highest floor among
the dependencies:

| Crate          | Locked version | MSRV | Used for                             |
| -------------- | -------------- | ---- | ------------------------------------ |
| `clap`         | 4.5.61         | 1.74 | CLI argument parsing                 |
| `serde`        | 1.0.229        | 1.71 | deriving the record and output types |
| `rust_decimal` | 1.42.1         | 1.64 | exact fixed-point money              |
| `csv`          | 1.4.0          | 1.61 | streaming CSV reader and writer      |
| `thiserror`    | 2.0.20         | 1.61 | error types                          |

1.74 also sits comfortably inside the wider ecosystem's de-facto floor, so adding async
I/O later would not force another jump. Holding that floor costs almost nothing here: as
the table shows, every dependency still resolves to a current release.

### Why `Cargo.lock` is committed

The lockfile is committed and builds should use `--locked`. Cargo only gained an
MSRV-aware resolver in 1.84, so an _older_ Cargo asked to resolve this tree from scratch
happily selects dependency versions its own compiler cannot even parse — at the time of
writing, `clap 4.6` pulls `clap_lex 1.1`, which is edition 2024 and requires 1.85, and the
build dies on a manifest parse error rather than backtracking.

`.cargo/config.toml` therefore sets:

```toml
[resolver]
incompatible-rust-versions = "fallback"
```

so that a modern Cargo running `cargo update` keeps the 1.74 promise instead of silently
breaking it. Cargo 1.74 ignores the key without warning, so it is harmless on the pinned
toolchain.

### Nix

The flake pins the toolchain, packages the engine, and carries the benchmark.

| Command                             | What it does                                                                                                                                          |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nix build`                         | Builds the engine hermetically on the pinned 1.74 toolchain. The derivation's check phase runs the whole suite, so a green build is a green test run. |
| `nix build .#with-dispute-withdraw` | The same engine with withdrawal disputes compiled in.                                                                                                 |
| `nix flake check`                   | Everything: both variants, the suite inside each, and the clippy, rustfmt and nixfmt checks. This is exactly what CI runs.                            |
| `nix run . -- transactions.csv`     | Runs the packaged binary.                                                                                                                             |
| `nix run .#bench`                   | The reproducible 1 GiB benchmark described below. `.#bench-settled` and `.#bench-small` are the other profiles.                                       |
| `nix build .#bench-input`           | Builds a benchmark input as a store path. `.#bench-input-settled`, `.#bench-input-small` and `.#bench-generator` are the rest.                        |
| `nix develop`                       | Dev shell: the toolchain plus `cargo-nextest`, `cargo-mutants`, `cargo-deny` and the formatters.                                                      |
| `nix run .#fmt`                     | Formats Rust, Nix, TOML and Markdown.                                                                                                                 |

The package is built with `makeRustPlatform` over the same `rust-bin` toolchain the dev
shell uses, so the two cannot drift apart on compiler version. The crate also builds and
runs with plain Cargo — Nix is a convenience, not a requirement.

### Continuous integration

CI runs everything through the flake, so there is no second definition of "green" to keep
in sync: no rustup, no toolchain action, no separately pinned tool versions. `nix flake
check` on a laptop gives the same answer as the pipeline, on the same compiler.

| Job                  | What it runs                                                                                                         |
| -------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `nix flake check`    | Both variants built, the whole suite inside each derivation's check phase, and the clippy, rustfmt and nixfmt checks |
| `cargo deny`         | Advisories, bans, licences and sources, through the dev shell                                                        |
| Benchmark smoke test | The benchmark at 16 MiB                                                                                              |

Two deliberate exceptions. `cargo deny` runs through `nix develop` rather than as a flake
check, because fetching the advisory database needs network access and a build sandbox has
none — the version is still pinned by the flake. And the benchmark runs at 16 MiB rather
than the 1 GiB default: that job exists to catch the generator or the benchmark wiring
rotting, not to measure anything. A shared runner has far too much variance for the number
to mean much, so the real 1 GiB run belongs on a known machine.

Clippy is invoked twice, with and without `--all-features`, because `--all-features` alone
would only ever lint the configuration where withdrawal disputes are compiled in.
