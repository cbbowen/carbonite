//! The columnar fast-path half of the derive: `SerializeColumns` /
//! `DeserializeColumns` impls as straight-line readers and writers whose
//! column offsets are compile-time constants.
//!
//! Generated code must write byte-for-byte what carbonite's serde-driven path
//! writes, over the column layout of the generated `StaticSchema`: columns
//! depth-first, a node's own columns before its children's. Both directions
//! index off `StaticSchema::columns`, the single declaration of a type's
//! column count.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{DeriveInput, Fields, Path, parse_quote};

use crate::attrs::parse_variant_attrs;
use crate::model::{FieldModel, ParamRoles, columns_expr, field_columns, field_models, strip_raw};

pub(crate) struct ColumnarParts {
    /// The type's total column count, for `StaticSchema::columns`.
    pub(crate) columns: TokenStream2,
    /// Whether any field is `#[carbonite(serde)]`, so `columns` is a runtime
    /// value rather than a folded constant.
    pub(crate) has_fallback: bool,
    pub(crate) ser_body: TokenStream2,
    pub(crate) de_body: TokenStream2,
}

pub(crate) fn columnar_impls(
    input: &DeriveInput,
    parts: &ColumnarParts,
    roles: &ParamRoles,
    krate: &Path,
) -> TokenStream2 {
    let ColumnarParts {
        ser_body, de_body, ..
    } = parts;
    let ident = &input.ident;

    let mut ser_generics = input.generics.clone();
    roles.apply(
        &mut ser_generics,
        &parse_quote!(#krate::SerializeColumns),
        krate,
    );
    let (ser_impl_generics, ty_generics, ser_where) = ser_generics.split_for_impl();

    // The deserialize impl is generic over the input lifetime 'de, which must
    // outlive every lifetime the type borrows (mirroring serde's derive).
    let mut de_generics = input.generics.clone();
    roles.apply(
        &mut de_generics,
        &parse_quote!(#krate::DeserializeColumns<'de>),
        krate,
    );
    {
        let where_clause = de_generics.make_where_clause();
        for lifetime_def in input.generics.lifetimes() {
            let lifetime = &lifetime_def.lifetime;
            where_clause.predicates.push(parse_quote!('de: #lifetime));
        }
    }
    de_generics.params.insert(0, parse_quote!('de));
    let (de_impl_generics, _, de_where) = de_generics.split_for_impl();

    quote! {
        #[automatically_derived]
        impl #ser_impl_generics #krate::SerializeColumns for #ident #ty_generics #ser_where {
            #[allow(unused_variables, unused_mut)]
            fn serialize_columns(
                &self,
                columns: &mut [::std::vec::Vec<u8>],
            ) -> #krate::Result<()> {
                #ser_body
            }
        }

        #[automatically_derived]
        impl #de_impl_generics #krate::DeserializeColumns<'de> for #ident #ty_generics #de_where {
            #[allow(unused_variables, unused_mut)]
            fn deserialize_columns(
                cursors: &mut [#krate::ColumnCursor<'de>],
            ) -> #krate::Result<Self> {
                #de_body
            }
        }
    }
}

pub(crate) fn columnar_struct_parts(fields: &Fields, krate: &Path) -> syn::Result<ColumnarParts> {
    let models = field_models(fields)?;
    let active: Vec<(&syn::Type, bool)> = models
        .iter()
        .filter(|m| !m.skip)
        .map(|m| (m.ty, m.fallback))
        .collect();
    let columns = columns_expr(&active, krate);
    let has_fallback = active.iter().any(|(_, fallback)| *fallback);

    let ser_steps: Vec<TokenStream2> = models
        .iter()
        .filter(|m| !m.skip)
        .map(|m| {
            let member = &m.member;
            let ty = m.ty;
            if m.fallback {
                return quote! {
                    #krate::fallback::serialize::<#ty>(&self.#member, &mut __rest)?;
                };
            }
            let width = field_columns(ty, false, krate);
            quote! {
                #krate::SerializeColumns::serialize_columns(
                    &self.#member,
                    #krate::columnar::__split(&mut __rest, #width),
                )?;
            }
        })
        .collect();
    let ser_body = quote! {
        let mut __rest = columns;
        #(#ser_steps)*
        ::core::result::Result::Ok(())
    };

    let mut reads = Vec::new();
    let mut values = Vec::new();
    for (index, m) in models.iter().enumerate() {
        if m.skip {
            // serde fills skipped fields from Default on deserialize.
            values.push(quote!(::core::default::Default::default()));
        } else {
            let tmp = format_ident!("__field{index}");
            let ty = m.ty;
            if m.fallback {
                reads.push(quote! {
                    let #tmp = #krate::fallback::deserialize::<#ty>(&mut __rest)?;
                });
            } else {
                let width = field_columns(ty, false, krate);
                reads.push(quote! {
                    let #tmp = <#ty as #krate::DeserializeColumns<'de>>::deserialize_columns(
                        #krate::columnar::__split(&mut __rest, #width),
                    )?;
                });
            }
            values.push(quote!(#tmp));
        }
    }
    let ctor = constructor(fields, &models, &values);
    let de_body = quote! {
        let mut __rest = cursors;
        #(#reads)*
        ::core::result::Result::Ok(#ctor)
    };

    Ok(ColumnarParts {
        columns,
        has_fallback,
        ser_body,
        de_body,
    })
}

fn constructor(fields: &Fields, models: &[FieldModel], values: &[TokenStream2]) -> TokenStream2 {
    match fields {
        Fields::Unit => quote!(Self),
        Fields::Unnamed(_) => quote!(Self(#(#values),*)),
        Fields::Named(_) => {
            let members = models.iter().map(|m| &m.member);
            quote!(Self { #(#members: #values),* })
        }
    }
}

pub(crate) fn columnar_enum_parts(
    data: &syn::DataEnum,
    krate: &Path,
) -> syn::Result<ColumnarParts> {
    let mut active = Vec::new();
    let mut skipped = Vec::new();
    for variant in &data.variants {
        if parse_variant_attrs(&variant.attrs)?.skip {
            skipped.push(&variant.ident);
        } else {
            active.push((variant, field_models(&variant.fields)?));
        }
    }

    // Per-variant column-count expressions, and each variant's offset past
    // the tag column and all preceding variants' columns. Constant unless a
    // variant carries a `#[carbonite(serde)]` field.
    let has_fallback = active
        .iter()
        .any(|(_, models)| models.iter().any(|m| !m.skip && m.fallback));
    let counts: Vec<TokenStream2> = active
        .iter()
        .map(|(_, models)| {
            let fields: Vec<(&syn::Type, bool)> = models
                .iter()
                .filter(|m| !m.skip)
                .map(|m| (m.ty, m.fallback))
                .collect();
            columns_expr(&fields, krate)
        })
        .collect();
    let offsets: Vec<TokenStream2> = (0..active.len())
        .map(|k| {
            let preceding = &counts[..k];
            quote!((1usize #(+ #preceding)*))
        })
        .collect();
    let columns = quote!((1usize #(+ #counts)*));

    let mut ser_arms = Vec::new();
    for (k, (variant, models)) in active.iter().enumerate() {
        let tag = k as u64;
        let offset = &offsets[k];
        let (pattern, bindings) = variant_pattern(&variant.ident, &variant.fields, models);
        let steps = bindings.iter().map(|(binding, ty, fallback)| {
            if *fallback {
                return quote! {
                    #krate::fallback::serialize::<#ty>(#binding, &mut __rest)?;
                };
            }
            let width = field_columns(ty, false, krate);
            quote! {
                #krate::SerializeColumns::serialize_columns(
                    #binding,
                    #krate::columnar::__split(&mut __rest, #width),
                )?;
            }
        });
        ser_arms.push(quote! {
            #pattern => {
                #krate::columnar::write_varint(&mut columns[0usize], #tag);
                let mut __rest = &mut columns[#offset..];
                #(#steps)*
                ::core::result::Result::Ok(())
            }
        });
    }
    for ident in &skipped {
        let name = strip_raw(&ident.to_string());
        ser_arms.push(quote! {
            Self::#ident { .. } => ::core::result::Result::Err(
                #krate::columnar::__skipped_variant(#name),
            )
        });
    }
    let ser_body = if data.variants.is_empty() {
        quote!(match *self {})
    } else {
        quote!(match self { #(#ser_arms,)* })
    };

    let mut de_arms = Vec::new();
    for (k, (variant, models)) in active.iter().enumerate() {
        let tag = k as u64;
        let offset = &offsets[k];
        let vident = &variant.ident;
        let mut reads = Vec::new();
        let mut values = Vec::new();
        for (index, m) in models.iter().enumerate() {
            if m.skip {
                values.push(quote!(::core::default::Default::default()));
            } else {
                let tmp = format_ident!("__field{index}");
                let ty = m.ty;
                if m.fallback {
                    reads.push(quote! {
                        let #tmp = #krate::fallback::deserialize::<#ty>(&mut __rest)?;
                    });
                } else {
                    let width = field_columns(ty, false, krate);
                    reads.push(quote! {
                        let #tmp = <#ty as #krate::DeserializeColumns<'de>>::deserialize_columns(
                            #krate::columnar::__split(&mut __rest, #width),
                        )?;
                    });
                }
                values.push(quote!(#tmp));
            }
        }
        let ctor = match &variant.fields {
            Fields::Unit => quote!(Self::#vident),
            Fields::Unnamed(_) => quote!(Self::#vident(#(#values),*)),
            Fields::Named(_) => {
                let members = models.iter().map(|m| &m.member);
                quote!(Self::#vident { #(#members: #values),* })
            }
        };
        de_arms.push(quote! {
            #tag => {
                let mut __rest = &mut cursors[#offset..];
                #(#reads)*
                ::core::result::Result::Ok(#ctor)
            }
        });
    }
    let de_body = quote! {
        let __tag = cursors[0usize].varint()?;
        match __tag {
            #(#de_arms,)*
            __other => ::core::result::Result::Err(
                #krate::columnar::__invalid_variant(__other),
            ),
        }
    };

    Ok(ColumnarParts {
        columns,
        has_fallback,
        ser_body,
        de_body,
    })
}

/// Builds a match pattern binding every non-skipped field, returning the
/// pattern and the `(binding, type, fallback)` triples in field order.
fn variant_pattern<'a>(
    vident: &syn::Ident,
    fields: &Fields,
    models: &'a [FieldModel<'a>],
) -> (TokenStream2, Vec<(syn::Ident, &'a syn::Type, bool)>) {
    match fields {
        Fields::Unit => (quote!(Self::#vident), Vec::new()),
        Fields::Unnamed(_) => {
            let mut pats = Vec::new();
            let mut bindings = Vec::new();
            for (index, m) in models.iter().enumerate() {
                if m.skip {
                    pats.push(quote!(_));
                } else {
                    let binding = format_ident!("__binding{index}");
                    pats.push(quote!(#binding));
                    bindings.push((binding, m.ty, m.fallback));
                }
            }
            (quote!(Self::#vident(#(#pats),*)), bindings)
        }
        Fields::Named(_) => {
            let mut pats = Vec::new();
            let mut bindings = Vec::new();
            for (index, m) in models.iter().enumerate() {
                let member = &m.member;
                if m.skip {
                    pats.push(quote!(#member: _));
                } else {
                    let binding = format_ident!("__binding{index}");
                    pats.push(quote!(#member: #binding));
                    bindings.push((binding, m.ty, m.fallback));
                }
            }
            (quote!(Self::#vident { #(#pats),* }), bindings)
        }
    }
}
