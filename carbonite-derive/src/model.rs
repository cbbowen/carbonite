//! The shared field model: which fields reach the wire, how each one's
//! schema and column count are obtained, and what bounds the generated impls
//! place on generic parameters.

use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, Index, Member, Path, parse_quote};

use crate::attrs::{parse_field_attrs, parse_variant_attrs};

pub(crate) struct FieldModel<'a> {
    pub(crate) member: Member,
    pub(crate) ty: &'a syn::Type,
    pub(crate) skip: bool,
    /// `#[carbonite(serde)]`; see [`field_node`].
    pub(crate) fallback: bool,
}

pub(crate) fn field_models(fields: &Fields) -> syn::Result<Vec<FieldModel<'_>>> {
    let list: Vec<&syn::Field> = match fields {
        Fields::Unit => Vec::new(),
        Fields::Named(named) => named.named.iter().collect(),
        Fields::Unnamed(unnamed) => unnamed.unnamed.iter().collect(),
    };
    list.into_iter()
        .enumerate()
        .map(|(index, field)| {
            let attrs = parse_field_attrs(&field.attrs)?;
            let member = match &field.ident {
                Some(ident) => Member::Named(ident.clone()),
                None => Member::Unnamed(Index::from(index)),
            };
            Ok(FieldModel {
                member,
                ty: &field.ty,
                skip: attrs.skip,
                fallback: attrs.fallback,
            })
        })
        .collect()
}

/// One field's schema node. A `#[carbonite(serde)]` field has no compile-time
/// schema, so its node comes from a memoized runtime trace of the field type —
/// which is exactly what tracing the containing type would have produced for
/// it, keeping the derived schema identical to the traced one.
pub(crate) fn field_node(ty: &syn::Type, fallback: bool, krate: &Path) -> TokenStream2 {
    if fallback {
        quote!(#krate::fallback::node::<#ty>())
    } else {
        quote!(<#ty as #krate::StaticSchema>::schema_node())
    }
}

/// One field's column count.
pub(crate) fn field_columns(ty: &syn::Type, fallback: bool, krate: &Path) -> TokenStream2 {
    if fallback {
        quote!(#krate::fallback::columns::<#ty>())
    } else {
        quote!(<#ty as #krate::StaticSchema>::columns())
    }
}

/// `(0usize + <T0>::columns() + <T1>::columns() + ...)` over the given
/// `(type, fallback)` pairs.
pub(crate) fn columns_expr(fields: &[(&syn::Type, bool)], krate: &Path) -> TokenStream2 {
    let widths = fields
        .iter()
        .map(|(ty, fallback)| field_columns(ty, *fallback, krate));
    quote!((0usize #(+ #widths)*))
}

/// How the fields use each generic parameter, which decides the bounds the
/// generated impls place on it.
pub(crate) struct ParamRoles {
    /// Parameters reached through a `#[carbonite(serde)]` field, which travel
    /// through serde rather than the columnar traits.
    fallback: Vec<syn::Ident>,
    /// Parameters reached *only* that way, which therefore need no schema of
    /// their own.
    fallback_only: Vec<syn::Ident>,
}

impl ParamRoles {
    pub(crate) fn collect(input: &DeriveInput) -> syn::Result<Self> {
        let uses = field_uses(&input.data)?;
        let mut fallback = Vec::new();
        let mut fallback_only = Vec::new();
        for param in input.generics.type_params() {
            let mentioned = |want_fallback: bool| {
                uses.iter().any(|(ty, is_fallback)| {
                    *is_fallback == want_fallback && mentions(ty, &param.ident)
                })
            };
            if mentioned(true) {
                fallback.push(param.ident.clone());
                if !mentioned(false) {
                    fallback_only.push(param.ident.clone());
                }
            }
        }
        Ok(ParamRoles {
            fallback,
            fallback_only,
        })
    }

    /// Bounds every type parameter for one generated impl: `primary` is that
    /// impl's own trait (`StaticSchema`, `SerializeColumns`,
    /// `DeserializeColumns<'de>`), which fallback-only parameters skip.
    pub(crate) fn apply(&self, generics: &mut syn::Generics, primary: &Path, krate: &Path) {
        for param in generics.type_params_mut() {
            if self.fallback.contains(&param.ident) {
                param
                    .bounds
                    .push(parse_quote!(#krate::fallback::SerdeField));
            }
            if !self.fallback_only.contains(&param.ident) {
                param.bounds.push(parse_quote!(#primary));
            }
        }
    }
}

/// Restates what a `#[carbonite(serde)]` field type must provide, so a type
/// that fails the requirement is reported against a trait that says so rather
/// than through whatever the helper calls happen to surface first (a borrowing
/// type otherwise lands on "implementation of `Deserialize` is not general
/// enough").
///
/// Skipped for field types mentioning a type parameter, whose own impl bound
/// already carries the same message. A field type that *borrows* is exactly the
/// case worth naming, so the assertions sit in a function carrying the type's
/// lifetimes.
pub(crate) fn fallback_assertions(input: &DeriveInput, krate: &Path) -> syn::Result<TokenStream2> {
    let params: Vec<&syn::Ident> = input.generics.type_params().map(|p| &p.ident).collect();
    let checks: Vec<TokenStream2> = field_uses(&input.data)?
        .iter()
        .filter(|(ty, fallback)| *fallback && !params.iter().any(|param| mentions(ty, param)))
        .map(|(ty, _)| quote_spanned!(ty.span() => __assert_serde_field::<#ty>();))
        .collect();
    if checks.is_empty() {
        return Ok(TokenStream2::new());
    }

    // Only the lifetimes: the checks never mention a type parameter, and
    // carrying unused ones would draw lints in the caller's crate.
    let lifetimes: Vec<&syn::LifetimeParam> = input.generics.lifetimes().collect();
    let generics = (!lifetimes.is_empty()).then(|| quote!(<#(#lifetimes),*>));
    Ok(quote! {
        const _: () = {
            fn __assert_serde_field<T: #krate::fallback::SerdeField>() {}
            #[allow(dead_code)]
            fn __assert_carbonite_serde_fields #generics () {
                #(#checks)*
            }
        };
    })
}

/// The `(type, fallback)` pairs of every field that reaches the wire, across a
/// struct's fields or all of an enum's live variants.
pub(crate) fn field_uses(data: &Data) -> syn::Result<Vec<(&syn::Type, bool)>> {
    let mut out = Vec::new();
    match data {
        Data::Struct(data) => collect_uses(&data.fields, &mut out)?,
        Data::Enum(data) => {
            for variant in &data.variants {
                if parse_variant_attrs(&variant.attrs)?.skip {
                    continue;
                }
                collect_uses(&variant.fields, &mut out)?;
            }
        }
        Data::Union(_) => {}
    }
    Ok(out)
}

fn collect_uses<'a>(fields: &'a Fields, out: &mut Vec<(&'a syn::Type, bool)>) -> syn::Result<()> {
    for model in field_models(fields)? {
        if !model.skip {
            out.push((model.ty, model.fallback));
        }
    }
    Ok(())
}

/// Whether `ty` mentions the identifier `param` anywhere.
pub(crate) fn mentions(ty: &syn::Type, param: &syn::Ident) -> bool {
    fn scan(tokens: TokenStream2, needle: &syn::Ident) -> bool {
        tokens.into_iter().any(|token| match token {
            proc_macro2::TokenTree::Ident(ident) => ident == *needle,
            proc_macro2::TokenTree::Group(group) => scan(group.stream(), needle),
            _ => false,
        })
    }
    scan(quote!(#ty), param)
}

pub(crate) fn strip_raw(ident: &str) -> String {
    ident.strip_prefix("r#").unwrap_or(ident).to_owned()
}
