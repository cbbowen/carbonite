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
//! identifier types, `skip_serializing_if`, and asymmetric skips).
//!
//! Three carbonite attributes of its own:
//!
//! - `#[carbonite(crate = "...")]` on the container points the generated code
//!   at a renamed carbonite dependency.
//! - `#[carbonite(as = "Repr")]` on the container replaces the field-based
//!   derivation entirely: the schema and both columnar directions come from
//!   `Repr`, which is how `#[serde(from)]` / `#[serde(into)]` /
//!   `#[serde(try_from)]` are supported — serde's pair may name two different
//!   shapes, so carbonite is told the wire type once. See [`as_repr`].
//! - `#[carbonite(serde)]` on a field opts that field out of the compile-time
//!   machinery: its schema comes from a runtime trace and its data goes
//!   through the serde path, which is what makes foreign types that only ship
//!   serde impls usable in a derived struct. See `carbonite::fallback`.
//!
//! The modules split along the derive's phases: [`attrs`] parses and vets the
//! attributes, [`model`] decides which fields reach the wire and what each
//! generic parameter needs, [`schema`] builds the schema-tree expression, and
//! [`columnar`] generates the fast-path readers and writers.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, parse_macro_input, parse_quote};

mod as_repr;
mod attrs;
mod columnar;
mod model;
mod rename;
mod schema;

use attrs::{CarboniteAttrs, parse_carbonite_attrs, parse_container_attrs};
use model::{ParamRoles, fallback_assertions, strip_raw};

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
        return as_repr::expand_as(input, &container, repr, &krate);
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
            schema::expand_struct(input, &container, &name, &data.fields, &krate)?,
            columnar::columnar_struct_parts(&data.fields, &krate)?,
        ),
        Data::Enum(data) => (
            schema::expand_enum(&container, &name, data, &krate)?,
            columnar::columnar_enum_parts(data, &krate)?,
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

    let columnar = columnar::columnar_impls(input, &parts, &roles, &krate);
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
