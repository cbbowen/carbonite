//! serde's `rename_all` case rules, replicated exactly.

use syn::meta::ParseNestedMeta;

use crate::attrs::expect_str_value;

#[derive(Clone, Copy)]
pub(crate) enum RenameRule {
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
    pub(crate) fn apply_to_field(self, field: &str) -> String {
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
    pub(crate) fn apply_to_variant(self, variant: &str) -> String {
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

pub(crate) fn parse_rename_rule(meta: &ParseNestedMeta) -> syn::Result<RenameRule> {
    let value = expect_str_value(meta)?;
    RenameRule::from_str(&value)
        .ok_or_else(|| meta.error(format!("unknown rename_all rule `{value}`")))
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
