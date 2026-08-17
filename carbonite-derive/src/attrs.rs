//! Parsing of the `#[serde(...)]` and `#[carbonite(...)]` attributes the
//! derive honors — and compile-time rejection of the serde features carbonite
//! cannot represent.

use proc_macro2::TokenStream as TokenStream2;
use syn::meta::ParseNestedMeta;
use syn::{LitStr, Path, parse_quote};

use crate::rename::{RenameRule, parse_rename_rule};

/// The container's own carbonite attributes.
pub(crate) struct CarboniteAttrs {
    /// `#[carbonite(crate = "...")]`, defaulting to `::carbonite`.
    pub(crate) krate: Path,
    /// `#[carbonite(as = "...")]`: the type this one is represented as on the
    /// wire, in *both* directions.
    pub(crate) repr: Option<syn::Type>,
}

pub(crate) fn parse_carbonite_attrs(attrs: &[syn::Attribute]) -> syn::Result<CarboniteAttrs> {
    let mut out = CarboniteAttrs {
        krate: parse_quote!(::carbonite),
        repr: None,
    };
    for attr in attrs {
        if !attr.path().is_ident("carbonite") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                out.krate = meta.value()?.parse::<LitStr>()?.parse()?;
                Ok(())
            } else if meta.path.is_ident("as") {
                out.repr = Some(meta.value()?.parse::<LitStr>()?.parse()?);
                Ok(())
            } else {
                Err(meta.error(
                    "unrecognized carbonite container attribute; the two are \
                     `crate = \"...\"` and `as = \"...\"`",
                ))
            }
        })?;
    }
    Ok(out)
}

#[derive(Default)]
pub(crate) struct ContainerAttrs {
    pub(crate) rename: Option<String>,
    pub(crate) rename_all: Option<RenameRule>,
    pub(crate) rename_all_fields: Option<RenameRule>,
    pub(crate) transparent: bool,
    /// `serde(from)` / `serde(try_from)`: what the type deserializes *from*.
    pub(crate) de_repr: Option<Conversion>,
    /// `serde(into)`: what the type serializes *into*.
    pub(crate) ser_repr: Option<Conversion>,
}

/// A type named by one of serde's conversion attributes.
pub(crate) struct Conversion {
    pub(crate) ty: syn::Type,
    /// Which attribute named it, for error messages.
    pub(crate) attr: &'static str,
    /// The type exactly as it was spelled, for error messages.
    pub(crate) text: String,
}

#[derive(Default)]
pub(crate) struct FieldAttrs {
    pub(crate) rename: Option<String>,
    pub(crate) skip: bool,
    /// `#[carbonite(serde)]`: take this field's schema from a runtime trace
    /// and route its data through the serde path.
    pub(crate) fallback: bool,
}

#[derive(Default)]
pub(crate) struct VariantAttrs {
    pub(crate) rename: Option<String>,
    pub(crate) rename_all: Option<RenameRule>,
    pub(crate) skip: bool,
}

pub(crate) fn parse_container_attrs(attrs: &[syn::Attribute]) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                out.rename = Some(expect_str_value(&meta)?);
            } else if meta.path.is_ident("rename_all") {
                out.rename_all = Some(parse_rename_rule(&meta)?);
            } else if meta.path.is_ident("rename_all_fields") {
                out.rename_all_fields = Some(parse_rename_rule(&meta)?);
            } else if meta.path.is_ident("transparent") {
                out.transparent = true;
            } else if meta.path.is_ident("untagged")
                || meta.path.is_ident("tag")
                || meta.path.is_ident("content")
            {
                return Err(meta.error(
                    "carbonite cannot derive Schema for untagged or internally/adjacently \
                     tagged enums; only externally tagged (default) enums have a columnar layout",
                ));
            } else if meta.path.is_ident("from") || meta.path.is_ident("try_from") {
                let attr = if meta.path.is_ident("from") {
                    "from"
                } else {
                    "try_from"
                };
                out.de_repr = Some(parse_conversion(&meta, attr)?);
            } else if meta.path.is_ident("into") {
                out.ser_repr = Some(parse_conversion(&meta, "into")?);
            } else if meta.path.is_ident("field_identifier")
                || meta.path.is_ident("variant_identifier")
            {
                return Err(meta.error(
                    "carbonite cannot derive Schema for a serde identifier type: it deserializes \
                     from the names of another type's fields or variants rather than from data of \
                     its own, so a schema would misdescribe it",
                ));
            } else if meta.path.is_ident("remote") {
                return Err(meta.error(
                    "carbonite cannot derive Schema for serde(remote): the generated code is a \
                     module of conversion functions rather than impls on this type, so there is \
                     nothing to hang a schema on",
                ));
            } else {
                skip_meta(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

pub(crate) fn parse_field_attrs(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    let mut fallback_attr = None;
    for attr in attrs {
        if attr.path().is_ident("carbonite") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("serde") {
                    out.fallback = true;
                    fallback_attr = Some(attr);
                    Ok(())
                } else {
                    Err(meta.error(
                        "unrecognized carbonite field attribute; the only one is `serde`, \
                         which routes this field through the serde path",
                    ))
                }
            })?;
            continue;
        }
        if !attr.path().is_ident("serde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                out.rename = Some(expect_str_value(&meta)?);
            } else if meta.path.is_ident("skip") {
                out.skip = true;
            } else if meta.path.is_ident("skip_serializing")
                || meta.path.is_ident("skip_deserializing")
            {
                return Err(meta.error(
                    "carbonite requires symmetric fields; use serde(skip) to omit a field \
                     from both directions",
                ));
            } else if meta.path.is_ident("skip_serializing_if") {
                return Err(meta.error(
                    "carbonite rejects skip_serializing_if: columnar rows must be complete",
                ));
            } else if meta.path.is_ident("flatten") {
                return Err(meta.error(
                    "carbonite cannot represent serde(flatten); it requires a \
                     self-describing format",
                ));
            } else if meta.path.is_ident("with")
                || meta.path.is_ident("serialize_with")
                || meta.path.is_ident("deserialize_with")
            {
                return Err(meta.error(
                    "carbonite cannot statically determine the schema of a field using \
                     serde(with); use runtime tracing (Schema::new) instead",
                ));
            } else {
                skip_meta(&meta)?;
            }
            Ok(())
        })?;
    }
    if let Some(attr) = fallback_attr.filter(|_| out.skip) {
        return Err(syn::Error::new_spanned(
            attr,
            "this field is `serde(skip)`, so it has no columns and nothing for \
             carbonite(serde) to route",
        ));
    }
    Ok(out)
}

pub(crate) fn parse_variant_attrs(attrs: &[syn::Attribute]) -> syn::Result<VariantAttrs> {
    let mut out = VariantAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                out.rename = Some(expect_str_value(&meta)?);
            } else if meta.path.is_ident("rename_all") {
                out.rename_all = Some(parse_rename_rule(&meta)?);
            } else if meta.path.is_ident("skip") {
                out.skip = true;
            } else if meta.path.is_ident("skip_serializing")
                || meta.path.is_ident("skip_deserializing")
            {
                return Err(meta.error(
                    "carbonite requires symmetric variants; use serde(skip) to omit a \
                     variant from both directions",
                ));
            } else if meta.path.is_ident("untagged") {
                return Err(meta.error("carbonite cannot represent untagged variants"));
            } else if meta.path.is_ident("with")
                || meta.path.is_ident("serialize_with")
                || meta.path.is_ident("deserialize_with")
            {
                return Err(meta.error(
                    "carbonite cannot statically determine the schema of a variant using \
                     serde(with); use runtime tracing (Schema::new) instead",
                ));
            } else {
                skip_meta(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

pub(crate) fn expect_str_value(meta: &ParseNestedMeta) -> syn::Result<String> {
    match meta.value() {
        Ok(value) => Ok(value.parse::<LitStr>()?.value()),
        Err(_) => Err(meta.error(
            "carbonite requires a single name here; split serialize/deserialize forms \
             are not supported",
        )),
    }
}

fn parse_conversion(meta: &ParseNestedMeta, attr: &'static str) -> syn::Result<Conversion> {
    let literal = meta.value()?.parse::<LitStr>()?;
    Ok(Conversion {
        ty: literal.parse()?,
        attr,
        text: literal.value(),
    })
}

/// Consumes and ignores an attribute we don't act on (`default`, `alias`,
/// `bound`, …), whatever its form: bare path, `name = value`, or `name(...)`.
fn skip_meta(meta: &ParseNestedMeta) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        let _: syn::Expr = meta.value()?.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in meta.input);
        let _: TokenStream2 = content.parse()?;
    }
    Ok(())
}
