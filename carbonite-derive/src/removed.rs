//! `#[carbonite(removed(...))]`: the names and positions a type has retired.
//!
//! Removing a field is safe on its own — the reader skips a column it has no
//! home for — and adding one is safe on its own, given a default. Composing
//! them is not: a new field that lands on a removed field's *name*, or on its
//! *position*, reads the dead column. Nothing in either schema says the slot
//! was ever used by something else, and where the two types agree the schemas
//! are byte-identical, so no comparison of schemas can catch it.
//!
//! So it is caught here, at the type. A retired slot must stay empty, or hold
//! a `()` placeholder — which occupies no data columns, so a tombstone costs
//! one tag byte in the schema and nothing per row.

use proc_macro2::Span;
use syn::{DataEnum, Fields, Lit, Type};

use crate::attrs::{FieldAttrs, parse_carbonite_variant_attrs, parse_field_attrs};
use crate::attrs::{VariantAttrs, parse_variant_attrs};
use crate::model::{field_wire_name, variant_wire_name};
use crate::rename::RenameRule;

/// One retired slot, and the span of the literal that named it.
pub(crate) struct Retired {
    slot: Slot,
    pub(crate) span: Span,
}

enum Slot {
    /// A retired field or variant name.
    Name(String),
    /// A retired position in a tuple struct or tuple variant. Also retires
    /// the name `"<index>"`, which is how a named field claims a position it
    /// replaced (`#[serde(alias = "0")]`).
    Position(usize),
}

impl Retired {
    pub(crate) fn parse(lit: &Lit) -> syn::Result<Self> {
        let slot = match lit {
            Lit::Str(name) => Slot::Name(name.value()),
            Lit::Int(index) => Slot::Position(index.base10_parse()?),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "a retired slot is a field or variant name (`\"old\"`) or a position in a \
                     tuple (`1`)",
                ));
            }
        };
        Ok(Retired {
            slot,
            span: lit.span(),
        })
    }

    /// The name this retirement forbids. A position forbids the alias that
    /// claims it.
    fn name(&self) -> String {
        match &self.slot {
            Slot::Name(name) => name.clone(),
            Slot::Position(index) => index.to_string(),
        }
    }
}

/// The fields that reach the wire, with their parsed attributes.
fn live(fields: &Fields) -> syn::Result<Vec<(&syn::Field, FieldAttrs)>> {
    let mut out = Vec::new();
    for field in fields {
        let attrs = parse_field_attrs(&field.attrs)?;
        if !attrs.skip {
            out.push((field, attrs));
        }
    }
    Ok(out)
}

/// A `()` placeholder: the only thing allowed to sit in a retired slot.
fn is_placeholder(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty())
}

/// Checks a struct's or variant's fields against its retirements.
pub(crate) fn check_fields(
    retired: &[Retired],
    fields: &Fields,
    rule: Option<RenameRule>,
) -> syn::Result<()> {
    if retired.is_empty() {
        return Ok(());
    }
    let live = live(fields)?;
    for entry in retired {
        match &entry.slot {
            Slot::Name(name) => {
                for (field, attrs) in &live {
                    let taken = field
                        .ident
                        .as_ref()
                        .is_some_and(|ident| field_wire_name(ident, attrs, rule) == *name);
                    if taken && !is_placeholder(&field.ty) {
                        return Err(syn::Error::new_spanned(
                            field,
                            format!(
                                "the name `{name}` is retired: data written while the old field \
                                 held it still carries that column, and a field of the same \
                                 name reads it — rename this field, or make it `()` if it is \
                                 only here to hold the name down"
                            ),
                        ));
                    }
                }
            }
            // Beyond the arity there is nothing to misplace, which is the
            // healthy state for a retired trailing position.
            Slot::Position(index) => {
                let occupant = match fields {
                    Fields::Unnamed(_) => live.get(*index),
                    _ => None,
                };
                if let Some((field, _)) = occupant {
                    if !is_placeholder(&field.ty) {
                        return Err(syn::Error::new_spanned(
                            field,
                            format!(
                                "position {index} is retired: data written while the old field \
                                 held it still carries that column, and positions are matched \
                                 in order, so this field reads it — put `()` at position \
                                 {index} and move this field after it"
                            ),
                        ));
                    }
                }
            }
        }
        let name = entry.name();
        for (field, attrs) in &live {
            if attrs.aliases.contains(&name) {
                return Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "this alias claims the retired name `{name}`: data written while the old \
                         field held it still carries that column, and the alias reads it into \
                         this field"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Checks an enum against its own retirements and each variant against
/// theirs.
pub(crate) fn check_enum(
    retired: &[Retired],
    data: &DataEnum,
    container_rule: Option<RenameRule>,
    field_rule: Option<RenameRule>,
) -> syn::Result<()> {
    let mut live: Vec<(&syn::Variant, VariantAttrs)> = Vec::new();
    for variant in &data.variants {
        let attrs = parse_variant_attrs(&variant.attrs)?;
        if !attrs.skip {
            live.push((variant, attrs));
        }
    }

    for entry in retired {
        let Slot::Name(name) = &entry.slot else {
            return Err(syn::Error::new(
                entry.span,
                "an enum's variants are matched by name rather than by position — the tag on \
                 the wire indexes the writer's list — so retire the variant's name instead",
            ));
        };
        for (variant, attrs) in &live {
            let claimed = variant_wire_name(&variant.ident, attrs, container_rule) == *name
                || attrs.aliases.iter().any(|alias| alias == name);
            if claimed {
                return Err(syn::Error::new_spanned(
                    variant,
                    format!(
                        "the variant name `{name}` is retired: the tag in data written while \
                         the old variant held it resolves to this name, so those rows decode \
                         as this variant — a variant name cannot be reused, and unlike a field \
                         it has no `()` form to retire it with"
                    ),
                ));
            }
        }
    }

    for (variant, attrs) in &live {
        let retired = parse_carbonite_variant_attrs(&variant.attrs)?;
        let rule = attrs.rename_all.or(field_rule);
        check_fields(&retired, &variant.fields, rule)?;
    }
    Ok(())
}
