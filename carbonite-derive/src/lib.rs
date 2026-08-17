//! Derive macro for carbonite's `StaticSchema` trait.
//!
//! `#[derive(Schema)]` generates a compile-time schema **identical** to what
//! carbonite's runtime tracing would discover for the same type. To keep that
//! guarantee it mirrors the serde attributes that affect the wire shape
//! (`rename`, `rename_all`, `rename_all_fields`, `skip`, `transparent`) and
//! rejects, at compile time, the ones carbonite cannot represent (`flatten`,
//! `untagged`, `tag`/`content`, `with`, `from`/`into`, `skip_serializing_if`,
//! and asymmetric skips).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::meta::ParseNestedMeta;
use syn::{Data, DeriveInput, Fields, LitStr, parse_macro_input, parse_quote};

/// Derives `carbonite::StaticSchema`: a compile-time schema matching what
/// runtime tracing would produce.
#[proc_macro_derive(Schema)]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let container = parse_container_attrs(&input.attrs)?;
    let name = container
        .rename
        .clone()
        .unwrap_or_else(|| strip_raw(&input.ident.to_string()));

    let body = match &input.data {
        Data::Struct(data) => expand_struct(input, &container, &name, &data.fields)?,
        Data::Enum(data) => expand_enum(&container, &name, data)?,
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "carbonite cannot derive Schema for unions",
            ));
        }
    };

    // Every type parameter needs a schema of its own.
    let mut generics = input.generics.clone();
    for param in generics.type_params_mut() {
        param.bounds.push(parse_quote!(::carbonite::StaticSchema));
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let ident = &input.ident;

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::carbonite::StaticSchema for #ident #ty_generics #where_clause {
            fn schema_node() -> ::carbonite::SchemaNode {
                #body
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Expansion.
// ---------------------------------------------------------------------------

fn expand_struct(
    input: &DeriveInput,
    container: &ContainerAttrs,
    name: &str,
    fields: &Fields,
) -> syn::Result<TokenStream2> {
    if container.transparent {
        let mut tys = Vec::new();
        match fields {
            Fields::Named(named) => {
                for field in &named.named {
                    if !parse_field_attrs(&field.attrs)?.skip {
                        tys.push(&field.ty);
                    }
                }
            }
            Fields::Unnamed(unnamed) => {
                for field in &unnamed.unnamed {
                    if !parse_field_attrs(&field.attrs)?.skip {
                        tys.push(&field.ty);
                    }
                }
            }
            Fields::Unit => {}
        }
        let [ty] = tys.as_slice() else {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "serde(transparent) requires exactly one non-skipped field",
            ));
        };
        return Ok(quote!(<#ty as ::carbonite::StaticSchema>::schema_node()));
    }

    match fields {
        Fields::Unit => Ok(quote! {
            ::carbonite::SchemaNode::UnitStruct { name: #name.to_owned() }
        }),
        Fields::Unnamed(unnamed) => {
            let tys = unnamed_field_types(unnamed)?;
            if unnamed.unnamed.len() == 1 && tys.len() == 1 {
                let ty = tys[0];
                Ok(quote! {
                    ::carbonite::SchemaNode::NewtypeStruct {
                        name: #name.to_owned(),
                        inner: ::std::boxed::Box::new(
                            <#ty as ::carbonite::StaticSchema>::schema_node(),
                        ),
                    }
                })
            } else {
                Ok(quote! {
                    ::carbonite::SchemaNode::TupleStruct {
                        name: #name.to_owned(),
                        fields: ::std::vec![
                            #(<#tys as ::carbonite::StaticSchema>::schema_node()),*
                        ],
                    }
                })
            }
        }
        Fields::Named(named) => {
            let entries = named_field_entries(named, container.rename_all)?;
            Ok(quote! {
                ::carbonite::SchemaNode::Struct {
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
            Fields::Unit => quote!(::carbonite::VariantNode::Unit),
            Fields::Unnamed(unnamed) => {
                let tys = unnamed_field_types(unnamed)?;
                if unnamed.unnamed.len() == 1 && tys.len() == 1 {
                    let ty = tys[0];
                    quote! {
                        ::carbonite::VariantNode::Newtype(::std::boxed::Box::new(
                            <#ty as ::carbonite::StaticSchema>::schema_node(),
                        ))
                    }
                } else {
                    quote! {
                        ::carbonite::VariantNode::Tuple(::std::vec![
                            #(<#tys as ::carbonite::StaticSchema>::schema_node()),*
                        ])
                    }
                }
            }
            Fields::Named(named) => {
                let fields = named_field_entries(named, field_rule)?;
                quote!(::carbonite::VariantNode::Struct(::std::vec![#(#fields),*]))
            }
        };
        entries.push(quote!((#variant_name.to_owned(), #shape)));
    }
    Ok(quote! {
        ::carbonite::SchemaNode::Enum {
            name: #name.to_owned(),
            variants: ::std::vec![#(#entries),*],
        }
    })
}

fn named_field_entries(
    fields: &syn::FieldsNamed,
    rule: Option<RenameRule>,
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
        let ty = &field.ty;
        entries.push(quote! {
            (#name.to_owned(), <#ty as ::carbonite::StaticSchema>::schema_node())
        });
    }
    Ok(entries)
}

fn unnamed_field_types(fields: &syn::FieldsUnnamed) -> syn::Result<Vec<&syn::Type>> {
    let mut tys = Vec::new();
    for field in &fields.unnamed {
        if !parse_field_attrs(&field.attrs)?.skip {
            tys.push(&field.ty);
        }
    }
    Ok(tys)
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
}

#[derive(Default)]
struct FieldAttrs {
    rename: Option<String>,
    skip: bool,
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
            } else if meta.path.is_ident("from")
                || meta.path.is_ident("into")
                || meta.path.is_ident("try_from")
                || meta.path.is_ident("remote")
            {
                return Err(meta.error(
                    "carbonite cannot statically derive Schema for containers using \
                     serde(from/into/try_from/remote); use runtime tracing (Schema::new) instead",
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
    for attr in attrs {
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
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}
