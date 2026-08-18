# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The wire format's own compatibility promises live with `SCHEMA_VERSION` and
`FORMAT_VERSION` in the crate docs and are independent of the crate version.

## [Unreleased]

### Added

- Fast-path (`StaticSchema` + columnar) coverage for `PathBuf`/`Path`, the
  `IpAddr` and `SocketAddr` families, `Range`/`RangeInclusive`/`Bound`,
  `Wrapping`, `[T]`, `Box<str>`, and `Box<[T]>`, so they work as derived-struct
  fields without `#[carbonite(serde)]`.
- Every engine (`Serializer`, `Deserializer`, `SelfDescribingSerializer`) now
  accepts its schema either borrowed (`&schema`) or owned (`schema`).
- A `shared` cargo feature (on by default) gating the `Shared`/`SharedArc`
  wrapper API. Blobs holding shared columns still decode with the feature off
  (repeats excepted).
- `#[carbonite(removed(...))]` on a container or variant records retired field
  names, variant names, and tuple positions, and the derive rejects anything
  that claims one again — by name, by position, or by `#[serde(alias)]`.
  Removing a field and adding one are each compatible changes whose
  composition is not: the new field reads the removed one's column, and where
  the two types agree the schemas are byte-identical, so no schema comparison
  can detect it. Retirements are compile-time only and do not reach the wire.
- `Option<T>` now widens to a sequence: a field that was an `Option` reads into
  `Vec<T>` (or any other sequence reader), `None` as empty and `Some` as one
  element. The wire is unchanged — a presence byte and a varint count of `0`/`1`
  are the same byte — so old files read as they stand. The reverse is still
  refused: narrowing a sequence back to an `Option` would decode row by row.

### Fixed

- `compat::check` now exercises every variant of enums nested inside other
  enums' variants; previously a removed nested variant could pass the check
  while real data using it failed to read.
- The columnar reader no longer sizes preallocations from a claimed count that
  the input has only loosely justified: fixed-width elements are bounded by
  their exact byte width, and other reservations are capped by a byte budget.
- The columnar `Duration` reader now carries overflowing nanoseconds into the
  seconds exactly as std's serde impl does, so the two engines agree on every
  input.
- `#[derive(Schema)]` rejects `#[serde(field_identifier)]` /
  `#[serde(variant_identifier)]` at compile time instead of deriving a schema
  that misdescribes the type.

## [1.0.0]

### Added

- Initial release: schema-separated columnar serialization built on serde —
  runtime schema tracing, `#[derive(Schema)]` compile-time schemas with
  monomorphized columnar fast paths, self-describing framing, name-matched
  schema evolution, `compat` checks for released schemas, `Shared`/`SharedArc`
  identity-preserving wrappers, and the `glam` integration feature.
