//! `#[carbonite(as = "Repr")]`: the type is represented as another type.
//!
//! serde's `from`/`into` pair can name two different shapes, and carbonite has
//! one schema for both directions, so the attribute states the wire type once:
//! the schema, the column count, and both columnar directions all come from
//! `Repr`, and the container's own fields never reach the wire. Reading
//! converts through `TryFrom` (which covers `From` via its blanket impl, so
//! serde(from) and serde(try_from) share one code path) and writing through
//! `Into` on a clone, exactly as serde's own `from`/`into` codegen does.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{DeriveInput, Path, parse_quote};

use crate::attrs::ContainerAttrs;
use crate::model::{field_uses, mentions};

pub(crate) fn expand_as(
    input: &DeriveInput,
    container: &ContainerAttrs,
    repr: &syn::Type,
    krate: &Path,
) -> syn::Result<TokenStream2> {
    if container.transparent {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "serde(transparent) and carbonite(as) describe different wire types for the same \
             container; keep only one",
        ));
    }
    // The fields play no part in the wire shape, so any per-field carbonite
    // attribute on them is dead configuration.
    for (ty, fallback) in field_uses(&input.data)? {
        if fallback {
            return Err(syn::Error::new_spanned(
                ty,
                "carbonite(serde) has no effect here: with carbonite(as) on the container, the \
                 fields do not reach the wire",
            ));
        }
    }
    // serde must convert in *both* directions, or its own impls disagree with
    // the schema this attribute declares.
    match (&container.de_repr, &container.ser_repr) {
        (Some(de), None) => {
            return Err(syn::Error::new_spanned(
                &de.ty,
                format!(
                    "serde({}) converts only when reading, so this type still *writes* its own \
                     fields and would not match the declared schema; add \
                     `#[serde(into = \"{}\")]`",
                    de.attr, de.text,
                ),
            ));
        }
        (None, Some(ser)) => {
            return Err(syn::Error::new_spanned(
                &ser.ty,
                format!(
                    "serde(into) converts only when writing, so this type still *reads* into its \
                     own fields and would not match the declared schema; add \
                     `#[serde(from = \"{}\")]`",
                    ser.text,
                ),
            ));
        }
        _ => {}
    }

    let ident = &input.ident;
    let (_, ty_generics, _) = input.generics.split_for_impl();

    let mut static_generics = input.generics.clone();
    static_generics
        .make_where_clause()
        .predicates
        .push(parse_quote!(#repr: #krate::StaticSchema));
    let (static_impl_generics, _, static_where) = static_generics.split_for_impl();

    let mut ser_generics = input.generics.clone();
    {
        let predicates = &mut ser_generics.make_where_clause().predicates;
        predicates.push(parse_quote!(#repr: #krate::SerializeColumns));
        predicates.push(parse_quote!(Self: ::core::clone::Clone + ::core::convert::Into<#repr>));
    }
    let (ser_impl_generics, _, ser_where) = ser_generics.split_for_impl();

    let mut de_generics = input.generics.clone();
    {
        let predicates = &mut de_generics.make_where_clause().predicates;
        predicates.push(parse_quote!(#repr: #krate::DeserializeColumns<'de>));
        predicates.push(parse_quote!(Self: ::core::convert::TryFrom<#repr>));
        predicates.push(
            parse_quote!(<Self as ::core::convert::TryFrom<#repr>>::Error: ::core::fmt::Display),
        );
        for lifetime_def in input.generics.lifetimes() {
            let lifetime = &lifetime_def.lifetime;
            predicates.push(parse_quote!('de: #lifetime));
        }
    }
    de_generics.params.insert(0, parse_quote!('de));
    let (de_impl_generics, _, de_where) = de_generics.split_for_impl();

    let assertions = repr_assertions(input, container, repr);

    Ok(quote! {
        #assertions

        #[automatically_derived]
        impl #static_impl_generics #krate::StaticSchema for #ident #ty_generics #static_where {
            fn schema_node() -> #krate::SchemaNode {
                <#repr as #krate::StaticSchema>::schema_node()
            }

            #[inline]
            fn columns() -> usize {
                <#repr as #krate::StaticSchema>::columns()
            }

            const FIXED_WIDTH: ::core::option::Option<usize> =
                <#repr as #krate::StaticSchema>::FIXED_WIDTH;
        }

        #[automatically_derived]
        impl #ser_impl_generics #krate::SerializeColumns for #ident #ty_generics #ser_where {
            fn serialize_columns(
                &self,
                columns: &mut [::std::vec::Vec<u8>],
            ) -> #krate::Result<()> {
                let __repr: #repr = ::core::convert::Into::into(::core::clone::Clone::clone(self));
                #krate::SerializeColumns::serialize_columns(&__repr, columns)
            }
        }

        #[automatically_derived]
        impl #de_impl_generics #krate::DeserializeColumns<'de> for #ident #ty_generics #de_where {
            fn deserialize_columns(
                cursors: &mut [#krate::ColumnCursor<'de>],
            ) -> #krate::Result<Self> {
                let __repr =
                    <#repr as #krate::DeserializeColumns<'de>>::deserialize_columns(cursors)?;
                ::core::convert::TryFrom::try_from(__repr)
                    .map_err(#krate::columnar::__conversion_failed)
            }
        }
    })
}

/// Checks that the type serde converts through really is the one
/// `#[carbonite(as = "...")]` declares — as a function that hands one back as
/// the other, so the compiler resolves both paths instead of comparing them
/// textually. Skipped when either mentions a type parameter, which is out of
/// scope here.
fn repr_assertions(
    input: &DeriveInput,
    container: &ContainerAttrs,
    repr: &syn::Type,
) -> TokenStream2 {
    let params: Vec<&syn::Ident> = input.generics.type_params().map(|p| &p.ident).collect();
    let lifetimes: Vec<&syn::LifetimeParam> = input.generics.lifetimes().collect();
    let generics = (!lifetimes.is_empty()).then(|| quote!(<#(#lifetimes),*>));

    let checks: Vec<TokenStream2> = [&container.de_repr, &container.ser_repr]
        .into_iter()
        .flatten()
        .filter(|conversion| {
            !params
                .iter()
                .any(|param| mentions(&conversion.ty, param) || mentions(repr, param))
        })
        .enumerate()
        .map(|(index, conversion)| {
            let name = format_ident!("__assert_carbonite_repr{index}");
            let serde_repr = &conversion.ty;
            quote_spanned! { repr.span() =>
                #[allow(dead_code)]
                fn #name #generics (repr: #serde_repr) -> #repr {
                    repr
                }
            }
        })
        .collect();
    if checks.is_empty() {
        return TokenStream2::new();
    }
    quote!(const _: () = { #(#checks)* };)
}
