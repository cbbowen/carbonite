//! Identifier types deserialize from another type's field names, so any
//! schema would misdescribe them.

#[derive(serde::Deserialize, carbonite::Schema)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    Id,
    Name,
}

fn main() {}
