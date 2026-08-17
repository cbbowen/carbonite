//! Derive macro for carbonite's compile-time schemas.
//!
//! `#[derive(Schema)]` implements three traits: `StaticSchema` — a schema
//! **identical** to what carbonite's runtime tracing would discover for the
//! same type, plus its column count — and the `SerializeColumns` /
//! `DeserializeColumns` fast paths over that layout.
//!
//! To keep the tracing-equivalence guarantee it mirrors the serde attributes
//! that affect the wire shape (`rename`, `rename_all`, `rename_all_fields`,
//! `skip`, `transparent`) and rejects, at compile time, the ones carbonite
//! cannot represent (`flatten`, `untagged`, `tag`/`content`, `with`, `remote`,
//! `skip_serializing_if`, and asymmetric skips).
//!
//! Three carbonite attributes of its own:
//!
//! - `#[carbonite(crate = "...")]` on the container points the generated code
//!   at a renamed carbonite dependency.
//! - `#[carbonite(as = "Repr")]` on the container replaces the field-based
//!   derivation entirely: the schema and both columnar directions come from
//!   `Repr`, which is how `#[serde(from)]` / `#[serde(into)]` /
//!   `#[serde(try_from)]` are supported — serde's pair may name two different
//!   shapes, so carbonite is told the wire type once.
//! - `#[carbonite(serde)]` on a field opts that field out of the compile-time
//!   machinery: its schema comes from a runtime trace and its data goes
//!   through the serde path, which is what makes foreign types that only ship
//!   serde impls usable in a derived struct. See `carbonite::fallback`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, quote_spanned};
use syn::meta::ParseNestedMeta;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, Index, LitStr, Member, Path, parse_macro_input, parse_quote};

/// Derives `carbonite::StaticSchema` (a compile-time schema matching what
/// runtime tracing would produce) along with the `SerializeColumns` and
/// `DeserializeColumns` fast paths.
#[proc_macro_derive(Schema, attributes(carbonite))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let CarboniteAttrs { krate, repr } = parse_carbonite_attrs(&input.attrs)?;
    let container = parse_container_attrs(&input.attrs)?;

    // `#[carbonite(as = "...")]` replaces the whole field-based derivation: the
    // fields are not what reaches the wire.
    if let Some(repr) = &repr {
        return expand_as(input, &container, repr, &krate);
    }
    if let Some(conversion) = container.de_repr.as_ref().or(container.ser_repr.as_ref()) {
        return Err(syn::Error::new_spanned(
            &conversion.ty,
            format!(
                "carbonite cannot infer a schema from serde({}) alone, because serde(from) and \
                 serde(into) may name different shapes while carbonite has one schema for both \
                 directions; declare the wire type with `#[carbonite(as = \"{}\")]`",
                conversion.attr, conversion.text,
            ),
        ));
    }

    let name = container
        .rename
        .clone()
        .unwrap_or_else(|| strip_raw(&input.ident.to_string()));

    let (body, parts) = match &input.data {
        Data::Struct(data) => (
            expand_struct(input, &container, &name, &data.fields, &krate)?,
            columnar_struct_parts(&data.fields, &krate)?,
        ),
        Data::Enum(data) => (
            expand_enum(&container, &name, data, &krate)?,
            columnar_enum_parts(data, &krate)?,
        ),
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "carbonite cannot derive Schema for unions",
            ));
        }
    };

    // Every type parameter needs a schema of its own, unless it is only ever
    // reached through a `#[carbonite(serde)]` field.
    let roles = ParamRoles::collect(input)?;
    let mut generics = input.generics.clone();
    roles.apply(&mut generics, &parse_quote!(#krate::StaticSchema), &krate);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let ident = &input.ident;

    // A fallback field's width comes from a runtime trace, so the sum is no
    // longer a constant expression. Memoize it when the type is not generic
    // (statics cannot be per-instantiation), since callers ask per row.
    let columns = &parts.columns;
    let columns_body = if parts.has_fallback && input.generics.params.is_empty() {
        quote! {
            static __COLUMNS: ::std::sync::OnceLock<usize> = ::std::sync::OnceLock::new();
            *__COLUMNS.get_or_init(|| #columns)
        }
    } else {
        quote!(#columns)
    };

    let columnar = columnar_impls(input, &parts, &roles, &krate);
    let assertions = fallback_assertions(input, &krate)?;

    Ok(quote! {
        #assertions

        #[automatically_derived]
        impl #impl_generics #krate::StaticSchema for #ident #ty_generics #where_clause {
            fn schema_node() -> #krate::SchemaNode {
                #body
            }

            #[inline]
            fn columns() -> usize {
                #columns_body
            }
        }

        #columnar
    })
}

// ---------------------------------------------------------------------------
// `#[carbonite(as = "Repr")]`: the type is represented as another type.
//
// serde's `from`/`into` pair can name two different shapes, and carbonite has
// one schema for both directions, so the attribute states the wire type once:
// the schema, the column count, and both columnar directions all come from
// `Repr`, and the container's own fields never reach the wire. Reading converts
// through `TryFrom` (which covers `From` via its blanket impl, so serde(from)
// and serde(try_from) share one code path) and writing through `Into` on a
// clone, exactly as serde's own `from`/`into` codegen does.
// ---------------------------------------------------------------------------

fn expand_as(
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

/// The container's own carbonite attributes.
struct CarboniteAttrs {
    /// `#[carbonite(crate = "...")]`, defaulting to `::carbonite`.
    krate: Path,
    /// `#[carbonite(as = "...")]`: the type this one is represented as on the
    /// wire, in *both* directions.
    repr: Option<syn::Type>,
}

fn parse_carbonite_attrs(attrs: &[syn::Attribute]) -> syn::Result<CarboniteAttrs> {
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

// ---------------------------------------------------------------------------
// Columnar fast-path impls (SerializeColumns / DeserializeColumns).
//
// These generate straight-line readers/writers whose column offsets are
// compile-time constants. They must write byte-for-byte what carbonite's
// serde-driven path writes, over the column layout of the generated
// StaticSchema: columns depth-first, a node's own columns before its
// children's. Both directions index off `StaticSchema::COLUMNS`, the single
// declaration of a type's column count.
// ---------------------------------------------------------------------------

struct ColumnarParts {
    /// The type's total column count, for `StaticSchema::columns`.
    columns: TokenStream2,
    /// Whether any field is `#[carbonite(serde)]`, so `columns` is a runtime
    /// value rather than a folded constant.
    has_fallback: bool,
    ser_body: TokenStream2,
    de_body: TokenStream2,
}

fn columnar_impls(
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

struct FieldModel<'a> {
    member: Member,
    ty: &'a syn::Type,
    skip: bool,
    /// `#[carbonite(serde)]`; see [`field_node`].
    fallback: bool,
}

fn field_models(fields: &Fields) -> syn::Result<Vec<FieldModel<'_>>> {
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
fn field_node(ty: &syn::Type, fallback: bool, krate: &Path) -> TokenStream2 {
    if fallback {
        quote!(#krate::fallback::node::<#ty>())
    } else {
        quote!(<#ty as #krate::StaticSchema>::schema_node())
    }
}

/// One field's column count.
fn field_columns(ty: &syn::Type, fallback: bool, krate: &Path) -> TokenStream2 {
    if fallback {
        quote!(#krate::fallback::columns::<#ty>())
    } else {
        quote!(<#ty as #krate::StaticSchema>::columns())
    }
}

/// `(0usize + <T0>::columns() + <T1>::columns() + ...)` over the given
/// `(type, fallback)` pairs.
fn columns_expr(fields: &[(&syn::Type, bool)], krate: &Path) -> TokenStream2 {
    let widths = fields
        .iter()
        .map(|(ty, fallback)| field_columns(ty, *fallback, krate));
    quote!((0usize #(+ #widths)*))
}

/// How the fields use each generic parameter, which decides the bounds the
/// generated impls place on it.
struct ParamRoles {
    /// Parameters reached through a `#[carbonite(serde)]` field, which travel
    /// through serde rather than the columnar traits.
    fallback: Vec<syn::Ident>,
    /// Parameters reached *only* that way, which therefore need no schema of
    /// their own.
    fallback_only: Vec<syn::Ident>,
}

impl ParamRoles {
    fn collect(input: &DeriveInput) -> syn::Result<Self> {
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
    fn apply(&self, generics: &mut syn::Generics, primary: &Path, krate: &Path) {
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
fn fallback_assertions(input: &DeriveInput, krate: &Path) -> syn::Result<TokenStream2> {
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
fn field_uses(data: &Data) -> syn::Result<Vec<(&syn::Type, bool)>> {
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
fn mentions(ty: &syn::Type, param: &syn::Ident) -> bool {
    fn scan(tokens: TokenStream2, needle: &syn::Ident) -> bool {
        tokens.into_iter().any(|token| match token {
            proc_macro2::TokenTree::Ident(ident) => ident == *needle,
            proc_macro2::TokenTree::Group(group) => scan(group.stream(), needle),
            _ => false,
        })
    }
    scan(quote!(#ty), param)
}

fn columnar_struct_parts(fields: &Fields, krate: &Path) -> syn::Result<ColumnarParts> {
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

fn columnar_enum_parts(data: &syn::DataEnum, krate: &Path) -> syn::Result<ColumnarParts> {
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

// ---------------------------------------------------------------------------
// Expansion.
// ---------------------------------------------------------------------------

fn expand_struct(
    input: &DeriveInput,
    container: &ContainerAttrs,
    name: &str,
    fields: &Fields,
    krate: &Path,
) -> syn::Result<TokenStream2> {
    if container.transparent {
        let mut inner = Vec::new();
        match fields {
            Fields::Named(named) => {
                for field in &named.named {
                    let attrs = parse_field_attrs(&field.attrs)?;
                    if !attrs.skip {
                        inner.push((&field.ty, attrs.fallback));
                    }
                }
            }
            Fields::Unnamed(unnamed) => {
                for field in &unnamed.unnamed {
                    let attrs = parse_field_attrs(&field.attrs)?;
                    if !attrs.skip {
                        inner.push((&field.ty, attrs.fallback));
                    }
                }
            }
            Fields::Unit => {}
        }
        let [(ty, fallback)] = inner.as_slice() else {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "serde(transparent) requires exactly one non-skipped field",
            ));
        };
        return Ok(field_node(ty, *fallback, krate));
    }

    match fields {
        Fields::Unit => Ok(quote! {
            #krate::SchemaNode::UnitStruct { name: #name.to_owned() }
        }),
        Fields::Unnamed(unnamed) => {
            let inner = unnamed_fields(unnamed)?;
            if unnamed.unnamed.len() == 1 && inner.len() == 1 {
                let (ty, fallback) = inner[0];
                let node = field_node(ty, fallback, krate);
                Ok(quote! {
                    #krate::SchemaNode::NewtypeStruct {
                        name: #name.to_owned(),
                        inner: ::std::boxed::Box::new(#node),
                    }
                })
            } else {
                let nodes = inner
                    .iter()
                    .map(|(ty, fallback)| field_node(ty, *fallback, krate));
                Ok(quote! {
                    #krate::SchemaNode::TupleStruct {
                        name: #name.to_owned(),
                        fields: ::std::vec![#(#nodes),*],
                    }
                })
            }
        }
        Fields::Named(named) => {
            let entries = named_field_entries(named, container.rename_all, krate)?;
            Ok(quote! {
                #krate::SchemaNode::Struct {
                    name: #name.to_owned(),
                    fields: ::std::vec![#(#entries),*],
                }
            })
        }
    }
}

fn expand_enum(
    container: &ContainerAttrs,
    name: &str,
    data: &syn::DataEnum,
    krate: &Path,
) -> syn::Result<TokenStream2> {
    let mut entries = Vec::new();
    for variant in &data.variants {
        let attrs = parse_variant_attrs(&variant.attrs)?;
        if attrs.skip {
            continue;
        }
        let variant_name = attrs.rename.clone().unwrap_or_else(|| {
            let ident = strip_raw(&variant.ident.to_string());
            match container.rename_all {
                Some(rule) => rule.apply_to_variant(&ident),
                None => ident,
            }
        });
        // Field-name casing inside a struct variant: variant-level
        // rename_all wins over container-level rename_all_fields.
        let field_rule = attrs.rename_all.or(container.rename_all_fields);
        let shape = match &variant.fields {
            Fields::Unit => quote!(#krate::VariantNode::Unit),
            Fields::Unnamed(unnamed) => {
                let inner = unnamed_fields(unnamed)?;
                if unnamed.unnamed.len() == 1 && inner.len() == 1 {
                    let (ty, fallback) = inner[0];
                    let node = field_node(ty, fallback, krate);
                    quote! {
                        #krate::VariantNode::Newtype(::std::boxed::Box::new(#node))
                    }
                } else {
                    let nodes = inner
                        .iter()
                        .map(|(ty, fallback)| field_node(ty, *fallback, krate));
                    quote! {
                        #krate::VariantNode::Tuple(::std::vec![#(#nodes),*])
                    }
                }
            }
            Fields::Named(named) => {
                let fields = named_field_entries(named, field_rule, krate)?;
                quote!(#krate::VariantNode::Struct(::std::vec![#(#fields),*]))
            }
        };
        entries.push(quote!((#variant_name.to_owned(), #shape)));
    }
    Ok(quote! {
        #krate::SchemaNode::Enum {
            name: #name.to_owned(),
            variants: ::std::vec![#(#entries),*],
        }
    })
}

fn named_field_entries(
    fields: &syn::FieldsNamed,
    rule: Option<RenameRule>,
    krate: &Path,
) -> syn::Result<Vec<TokenStream2>> {
    let mut entries = Vec::new();
    for field in &fields.named {
        let attrs = parse_field_attrs(&field.attrs)?;
        if attrs.skip {
            continue;
        }
        let ident = strip_raw(&field.ident.as_ref().expect("named field").to_string());
        let name = attrs.rename.clone().unwrap_or_else(|| match rule {
            Some(rule) => rule.apply_to_field(&ident),
            None => ident,
        });
        let node = field_node(&field.ty, attrs.fallback, krate);
        entries.push(quote! {
            (#name.to_owned(), #node)
        });
    }
    Ok(entries)
}

/// The `(type, fallback)` pairs of the non-skipped positional fields.
fn unnamed_fields(fields: &syn::FieldsUnnamed) -> syn::Result<Vec<(&syn::Type, bool)>> {
    let mut out = Vec::new();
    for field in &fields.unnamed {
        let attrs = parse_field_attrs(&field.attrs)?;
        if !attrs.skip {
            out.push((&field.ty, attrs.fallback));
        }
    }
    Ok(out)
}

fn strip_raw(ident: &str) -> String {
    ident.strip_prefix("r#").unwrap_or(ident).to_owned()
}

// ---------------------------------------------------------------------------
// serde attribute parsing.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ContainerAttrs {
    rename: Option<String>,
    rename_all: Option<RenameRule>,
    rename_all_fields: Option<RenameRule>,
    transparent: bool,
    /// `serde(from)` / `serde(try_from)`: what the type deserializes *from*.
    de_repr: Option<Conversion>,
    /// `serde(into)`: what the type serializes *into*.
    ser_repr: Option<Conversion>,
}

/// A type named by one of serde's conversion attributes.
struct Conversion {
    ty: syn::Type,
    /// Which attribute named it, for error messages.
    attr: &'static str,
    /// The type exactly as it was spelled, for error messages.
    text: String,
}

#[derive(Default)]
struct FieldAttrs {
    rename: Option<String>,
    skip: bool,
    /// `#[carbonite(serde)]`: take this field's schema from a runtime trace
    /// and route its data through the serde path.
    fallback: bool,
}

#[derive(Default)]
struct VariantAttrs {
    rename: Option<String>,
    rename_all: Option<RenameRule>,
    skip: bool,
}

fn parse_container_attrs(attrs: &[syn::Attribute]) -> syn::Result<ContainerAttrs> {
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

fn parse_field_attrs(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrs> {
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

fn parse_variant_attrs(attrs: &[syn::Attribute]) -> syn::Result<VariantAttrs> {
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

fn expect_str_value(meta: &ParseNestedMeta) -> syn::Result<String> {
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

fn parse_rename_rule(meta: &ParseNestedMeta) -> syn::Result<RenameRule> {
    let value = expect_str_value(meta)?;
    RenameRule::from_str(&value)
        .ok_or_else(|| meta.error(format!("unknown rename_all rule `{value}`")))
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

// ---------------------------------------------------------------------------
// serde's rename_all case rules, replicated exactly.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RenameRule {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameRule {
    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "lowercase" => RenameRule::Lower,
            "UPPERCASE" => RenameRule::Upper,
            "PascalCase" => RenameRule::Pascal,
            "camelCase" => RenameRule::Camel,
            "snake_case" => RenameRule::Snake,
            "SCREAMING_SNAKE_CASE" => RenameRule::ScreamingSnake,
            "kebab-case" => RenameRule::Kebab,
            "SCREAMING-KEBAB-CASE" => RenameRule::ScreamingKebab,
            _ => return None,
        })
    }

    /// Applies to a field ident, assumed `snake_case` (serde's assumption).
    fn apply_to_field(self, field: &str) -> String {
        match self {
            RenameRule::Lower | RenameRule::Snake => field.to_owned(),
            RenameRule::Upper | RenameRule::ScreamingSnake => field.to_ascii_uppercase(),
            RenameRule::Pascal => field.split('_').map(capitalize).collect(),
            RenameRule::Camel => uncapitalize_first(&RenameRule::Pascal.apply_to_field(field)),
            RenameRule::Kebab => field.replace('_', "-"),
            RenameRule::ScreamingKebab => field.to_ascii_uppercase().replace('_', "-"),
        }
    }

    /// Applies to a variant ident, assumed `PascalCase` (serde's assumption).
    fn apply_to_variant(self, variant: &str) -> String {
        match self {
            RenameRule::Pascal => variant.to_owned(),
            RenameRule::Lower => variant.to_ascii_lowercase(),
            RenameRule::Upper => variant.to_ascii_uppercase(),
            RenameRule::Camel => uncapitalize_first(variant),
            RenameRule::Snake => pascal_to_snake(variant),
            RenameRule::ScreamingSnake => pascal_to_snake(variant).to_ascii_uppercase(),
            RenameRule::Kebab => pascal_to_snake(variant).replace('_', "-"),
            RenameRule::ScreamingKebab => pascal_to_snake(variant)
                .to_ascii_uppercase()
                .replace('_', "-"),
        }
    }
}

fn capitalize(segment: &str) -> String {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn uncapitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn pascal_to_snake(variant: &str) -> String {
    let mut out = String::with_capacity(variant.len() + 2);
    for (i, ch) in variant.char_indices() {
        if i > 0 && ch.is_uppercase() {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}
