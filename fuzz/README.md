# Fuzz targets

Pure-logic fuzzing of `program/src/state.rs` — the ring-buffer bitmap and the
`BookRefMut` order lifecycle — with no `AccountInfo`, no CPI, no Solana
runtime involved. That decoupling is deliberate in the program itself
(`state.rs`'s module doc: "that keeps the matching logic runnable off-chain
against a copy of the account"), which is exactly what makes it fuzzable.

## Setup

```sh
cargo install cargo-fuzz   # once
```

Requires the `nightly` toolchain (for the sanitizer instrumentation), which
`cargo +nightly fuzz` invokes automatically — no separate install needed if
`rustup toolchain install nightly` has already been run.

## Running

```sh
cargo +nightly fuzz run bitmap
cargo +nightly fuzz run book_ops
```

Add `-- -max_total_time=180` (seconds) for a bounded run instead of running
until interrupted. A crash writes a reproducer to `fuzz/artifacts/<target>/`
and prints the failing input; replay it with:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>
```

## `bitmap` target

Differential fuzz of `Bitmap` (`set`/`clear`/`get`/`next_at_or_after`/
`prev_at_or_before`) against a `BTreeSet<u32>` oracle, across randomized
anchor residues. `tests/tests/bitmap.rs` already brute-forces this with
hand-picked offsets and a fixed sweep; this explores operation *sequences*
libFuzzer's coverage guidance finds on its own, concentrated on the
two-segment physical/tick-order wraparound split the `Bitmap` doc comment
describes — the likeliest place for an off-by-one to hide.

## `book_ops` target

Drives a sequence of `BookRefMut` operations (`upsert_order`, `cancel_order`,
`take`, `mass_quote_side`, `reanchor_step`, `reap_expired`) against one
freshly-initialized in-memory market (4 seats, 256-node pool — small enough
that pool-full and per-seat order-cap conditions are actually reachable
within a fuzz budget). After **every** operation it asserts, directly from
the account bytes:

- bitmap/level agreement (a tick's bit is set iff that level's head is
  non-`NIL`) and no tick set on both sides
- every level's linked FIFO sums to its `total_lots`, with a cycle guard
- the free list and every seat's order chain are cycle-free, correctly
  counted, and partition the live nodes with no node reachable from two
  lists
- every seat's `order_count` matches its chain's actual length, and its
  locked balances match what its resting orders require
- `best_bid < best_ask` whenever both exist
- **money conservation**: seats are funded once at setup (directly, the
  logic-layer equivalent of a `Deposit`, tracked as a virtual vault total)
  and never funded again mid-run, so `sum(seat balances) + fees_accrued`
  must equal that virtual vault after every single operation, forever. Any
  drift here means atoms were created or destroyed somewhere in the order
  lifecycle.

`take()` is documented as settling only the maker leg — its real callers
(`PlaceTake`, `PlaceLimit`, `Swap`) settle the taker leg immediately
afterward in the same instruction, and Solana reverts the whole instruction,
`take()`'s effects included, if that settlement fails. The `Take` op
reproduces both halves: it settles the taker leg itself (against a seat, or
against the virtual vault for the seatless/`NO_SEAT` case, mirroring
`Swap`'s CPIs) and snapshots/restores the whole buffer on any failure to
reproduce that all-or-nothing behavior. No other op needs this — every other
`BookRefMut` method is internally self-consistent even when it returns an
error partway through (each mutating sub-step is either a no-op on failure
or a locked↔free transfer within one seat), so an observed partial
application never itself breaks a structural or conservation invariant.

Two access points exist purely to make this target possible and are gated
behind the `test-utils` feature, same as the crate's existing test-only
accessors: `BookRefMut::take_for_test` (a thin wrapper — `take`'s
`pub(crate) TakeParams` aren't part of the account/instruction ABI and stay
that way) and `test_utils::is_invalid_state` (lets the harness treat the
book-corruption sentinel as a hard failure while treating every other `Err`
as ordinary rejected input).

## Status as of the last run

Both targets ran clean: `bitmap` for ~6.5M executions / 150s, `book_ops` for
~114k executions / 183s (a slower per-exec cost, expected — it walks the
full 8,192-tick window and every seat chain after each of up to 400 ops).
Neither found a crash or an invariant violation. `book_ops`' coverage
plateaued around 1,896 edges before the run ended, so a longer campaign
would likely need corpus persistence (`-artifact_prefix=`, a saved corpus
directory reused across runs) to keep making progress rather than
re-discovering the same states — worth doing before relying on this as a
release gate rather than a point-in-time check.
