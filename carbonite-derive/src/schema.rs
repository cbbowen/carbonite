//! The `StaticSchema::schema_node` half of the derive: building the schema
//! tree expression for a struct or enum, honoring the serde attributes that
//! affect wire shape.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Fields, Path};

use crate::attrs::{ContainerAttrs, parse_field_attrs, parse_variant_attrs};
use crate::model::{field_node, strip_raw};
use crate::rename::RenameRule;

pub(crate) fn expand_struct(
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

pub(crate) fn expand_enum(
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
