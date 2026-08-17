# Cleanup backlog

Findings from a full-crate review (2026-08-17), ordered by priority. Items are
checked off as they land; each entry records the problem, the evidence, and the
suggested fix so it can be picked up cold.

## 1. `compat` gives a false green for removed nested-enum variants

**Status: fixed.** `write_row` now consumes the low digit of `pick` at each
enum and hands the quotient to the payload (mixed-radix), and `variant_span`
multiplies through nesting instead of taking the max, capped at
`MAX_PROBE_ROWS` for pathological chains. Regression tests:
`dropping_a_nested_enum_variant_is_caught` and
`a_variant_three_levels_deep_is_still_exercised` in `tests/compat.rs`.

**Original finding, confirmed by test.** The sampler in `src/compat.rs` (`sample` /
`write_row` / `variant_span`) writes `variant_span` rows and uses the *same*
`pick` for every enum along a path: the outer tag is `pick % outer_len`, so a
nested enum inside outer variant `i` only ever sees picks `≡ i (mod
outer_len)` — one residue class. When the inner and outer variant counts share
a factor, some inner variants are never written, and `compat::check` passes
for a change that breaks real data.

Repro (fails today: the last assert does not hold):

```rust
enum InnerV1 { X, Y }
enum OuterV1 { A, B(InnerV1) }

enum Inner { Y }                       // Inner::X removed
enum Outer { A, B(Inner) }

let released = carbonite::Schema::<OuterV1>::new().unwrap().cast::<Outer>();
// Real old data with Outer::B(Inner::X) fails to read...
assert!(carbonite::from_slice::<Outer>(
    &carbonite::to_vec(&OuterV1::B(InnerV1::X)).unwrap()).is_err());
// ...but the check reports Ok(()).
assert!(carbonite::compat::check::<Outer>(&released).is_err());
```

**Fix:** make the pick mixed-radix. In `write_row`'s enum arm, keep
`index = pick % variants.len()` but pass `pick / variants.len()` down into the
payload, and change `variant_span` for an enum from
`variants.len().max(nested)` to `variants.len() * nested` (nested = max
payload span across variants). Then each variant is chosen `rows / len` times
with the quotient sweeping the nested spans. Deep enum chains make `rows` the
product of chain lengths — bounded by the schema, but worth a comment; the
alternative is steering with an explored-set the way `trace.rs` does. Add the
repro above as a regression test, and correct the module-doc claim ("every
variant of every enum is exercised at least once") if the fix is not exact.

## 2. Columnar preallocation amplifies input by `size_of::<T>()`

**Status: fixed.** `read_count` now bounds fixed-width elements by
`remaining / width` (exact, via `StaticSchema::FIXED_WIDTH`), and
`cautious_capacity` caps the speculative reservation for everything else at a
`MAX_PREALLOC_BYTES` budget, serde-style — the vector still grows to the real
length as elements decode. Collection `collect()`s were already safe: std's
`Result`-collecting adapter reports a zero lower bound, so they never
preallocated from the claim. Hardening tests:
`fixed_width_sequence_counts_are_bounded_by_the_element_width` and
`wide_element_claims_fail_cleanly_without_a_matching_reservation`.

**Original finding.** `checked_count` (`src/columnar.rs`) bounds a claimed count
by *total remaining bytes* — a floor of one byte per element — and
`Vec<T>::deserialize_columns` then does `Vec::with_capacity(len)`. A hostile
blob paying one byte per claimed element gets an upfront allocation of
`len * size_of::<T>()`: for a derived struct of four `u128`s, a 64 MB blob
triggers a ~4 GB reservation before decoding fails. Still O(input), but the
constant is attacker-chosen via the element type, and a capacity overflow on a
constrained target aborts — which the docs promise never happens. The serde
path is already safe (serde's visitors cap speculative preallocation); only
the monomorphized path over-trusts.

**Fix (both halves):**
- For fixed-width elements, use the existing `StaticSchema::FIXED_WIDTH` in
  `read_count` to bound by `remaining_bytes / width` — exact and free, and it
  tightens the count check itself, not just the prealloc.
- For everything else, cap the reservation by a byte budget serde-style in
  `cautious_capacity`: `len.min(MAX_PREALLOC_BYTES / size_of::<T>().max(1))`.
  The vector still grows to the real length; it just stops trusting the
  header for the reservation. Apply to the map/set impls too.
- Add a hardening test: `tests/hardening.rs` covers the byte bound but not
  the amplification.

## 3. `Shared` write-side identity is address-based; addresses can be reused

**Status: mitigated on this branch (documented + flag-gated).** Write-side
dedup keys on `(column position, pointee address)`. Sound whenever every
handle serialized in a row is alive simultaneously — true for handles
reachable from the value — but a hand-written `Serialize` impl that
manufactures temporary `Shared` values mid-row can see a dropped temporary's
address reused, silently aliasing two distinct objects in the dictionary.

Done here:
- [x] Documented the aliveness requirement: `src/shared.rs` module docs and
  both wrapper type docs, the crate-level `# Shared values` section in
  `src/lib.rs`, and the README.
- [x] Gated the wrapper API behind a `shared` cargo feature (on by default),
  as a single `#[cfg]` on an inner `wrappers` module. The dictionary-protocol
  machinery stays unconditional so blobs holding shared columns still decode
  (repeats excepted) in a build without the feature.

Possible follow-up: keeping a clone of each new handle alive for the row
scope would close the hazard outright, but the `Serialize` impl lacks a
`'static` bound to type-erase, so documentation is the practical stop.

## 4. Smaller correctness / consistency nits

- [ ] **Derive silently ignores `#[serde(field_identifier)]` /
  `#[serde(variant_identifier)]`** (`carbonite-derive/src/lib.rs`,
  `skip_meta`). These change the wire shape wholesale, so the derived schema
  is confidently wrong. Reject them explicitly, like `untagged` / `flatten` /
  `with`.
- [ ] **The two engines diverge on hostile `Duration` bytes**: the columnar
  path rejects `nanos >= 1e9` (`src/columnar.rs`) while std's serde impl
  carries the overflow into seconds. No legitimate writer produces this;
  either document the asymmetry or carry in the columnar path too.
- [ ] **`ColMap::next_value_seed` uses `debug_assert!`** (`src/de.rs`) where
  the serializing twin (`MapSerializer`, `src/ser.rs`) returns proper errors;
  in release a misbehaving visitor silently desyncs cursors. Return an error
  for symmetry.
- [ ] **Hand-written `Deserialize` impls supporting `visit_map` but not
  `visit_seq`** trace fine, match their own schema, get the fast positional
  path — then fail where the slow path would work. Add a sentence to the
  `Deserializer::new` docs pointing at `new_untraced`.

## 5. API design

- [ ] **Ownership asymmetry:** `Serializer::new(&schema)` borrows;
  `Deserializer::new(schema)` owns. Pick one convention (borrowing both
  composes better with a long-lived schema).
- [ ] **Fast-path std coverage gaps:** `PathBuf`, `IpAddr`/`SocketAddr`,
  `Range<T>`, `Bound<T>`, `Wrapping<T>`, `Box<str>`, `Box<[T]>` fields are
  compile errors against `StaticSchema` today; `#[carbonite(serde)]` works
  but costs per-field runtime dispatch for types with perfectly static
  shapes. Mechanical to add.

## 6. Maintainability

- [ ] **`SchemaNode`/`LNode` lockstep walks**: ~8
  `unreachable!("layout was built from this schema")` arms across `de.rs`,
  `ser.rs`, `compat.rs` rely on two trees staying structurally parallel. A
  fused annotated tree (layout ids attached to schema nodes, built once per
  `Serializer`/`Deserializer`) would delete the invariant class.
- [ ] **Split the derive** (1383 lines, one file) into attribute-parsing /
  schema-codegen / columnar-codegen modules; add trybuild UI tests locking in
  its error messages.
- [ ] **Add a fuzz target** (`cargo-fuzz`) over `from_slice` +
  `Schema::from_bytes` — the complement to the hand-written hardening tests;
  it would likely have found item 2.
- [ ] **Repo hygiene:** no CI config, no `LICENSE-MIT`/`LICENSE-APACHE` files
  (the manifest declares them), no CHANGELOG. CI should run test/clippy/fmt on
  stable + the 1.85 MSRV; `cargo-semver-checks` is cheap insurance given the
  `#[doc(hidden)]`-but-public surface the derive depends on.

## 7. Compatibility contract

- [ ] **Data blobs carry no version of their own**; their encoding is
  implicitly versioned by the accompanying schema. That works only if
  `SCHEMA_VERSION` is bumped for any change to the *blob* encoding too, not
  just the schema encoding. State that promise explicitly in the
  `SCHEMA_VERSION` docs.
