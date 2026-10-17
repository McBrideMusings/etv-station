//! The scalar `item.*` vocabulary (#133): every single-valued column of
//! `entries` an author can name, in one table.
//!
//! A pool expression filters on these ([`super::query`]), an order string sorts
//! on them ([`super::order`]), and a Rhai scorer reads them off its item map
//! (`score`). All three read this enum, so a column is filterable, sortable and
//! visible to a scorer by construction. A field's name is also its `entries`
//! column. Adding a variant to the `scalar_fields!` list puts it in
//! [`ScalarField::ALL`] automatically and fails the build in
//! [`ScalarField::kind`] and [`ScalarField::value`] until each handles it.
//!
//! Multi-valued fields (`genres`, `cast`, …), `source`, `collections` and
//! `fs_dir` are not columns on `entries` and stay in `query`.

use super::model::Entry;

/// Whether a scalar column holds text or an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Str,
    Num,
}

/// One scalar column's value read off an [`Entry`]; `None` is SQL `NULL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarValue {
    Str(Option<String>),
    Num(Option<i64>),
}

/// Declares the enum, [`ScalarField::ALL`] and [`ScalarField::name`] from one
/// list, so a variant cannot be left out of `ALL`.
macro_rules! scalar_fields {
    ($($variant:ident => $name:literal,)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ScalarField {
            $($variant,)+
        }

        impl ScalarField {
            pub const ALL: &[ScalarField] = &[$(ScalarField::$variant,)+];

            /// The config-facing name: what follows `item.` in an expression,
            /// the field in an order string, the scorer item-map key, and the
            /// `entries` column.
            pub fn name(self) -> &'static str {
                match self {
                    $(ScalarField::$variant => $name,)+
                }
            }
        }
    };
}

scalar_fields! {
    Title => "title",
    TitleSort => "title_sort",
    Show => "show",
    Type => "type",
    Year => "year",
    ReleaseDate => "release_date",
    Season => "season",
    Episode => "episode",
    AbsoluteEpisode => "absolute_episode",
    DurationMs => "duration_ms",
    ContentRating => "content_rating",
    Edition => "edition",
    Studio => "studio",
    Library => "library",
}

impl ScalarField {
    pub fn from_name(name: &str) -> Option<ScalarField> {
        Self::ALL.iter().copied().find(|f| f.name() == name)
    }

    pub fn kind(self) -> ScalarKind {
        match self {
            ScalarField::Title
            | ScalarField::TitleSort
            | ScalarField::Show
            | ScalarField::Type
            | ScalarField::ReleaseDate
            | ScalarField::ContentRating
            | ScalarField::Edition
            | ScalarField::Studio
            | ScalarField::Library => ScalarKind::Str,
            ScalarField::Year
            | ScalarField::Season
            | ScalarField::Episode
            | ScalarField::AbsoluteEpisode
            | ScalarField::DurationMs => ScalarKind::Num,
        }
    }

    /// This field's value on `entry`.
    pub fn value(self, entry: &Entry) -> ScalarValue {
        let s = |v: &Option<String>| ScalarValue::Str(v.clone());
        match self {
            ScalarField::Title => ScalarValue::Str(Some(entry.title.clone())),
            ScalarField::TitleSort => s(&entry.title_sort),
            ScalarField::Show => s(&entry.show),
            ScalarField::Type => ScalarValue::Str(Some(entry.kind.clone())),
            ScalarField::Year => ScalarValue::Num(entry.year),
            ScalarField::ReleaseDate => s(&entry.release_date),
            ScalarField::Season => ScalarValue::Num(entry.season),
            ScalarField::Episode => ScalarValue::Num(entry.episode),
            ScalarField::AbsoluteEpisode => ScalarValue::Num(entry.absolute_episode),
            ScalarField::DurationMs => ScalarValue::Num(entry.duration_ms),
            ScalarField::ContentRating => s(&entry.content_rating),
            ScalarField::Edition => s(&entry.edition),
            ScalarField::Studio => s(&entry.studio),
            ScalarField::Library => s(&entry.library),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, Source};

    #[test]
    fn every_scalar_field_is_queryable_sortable_and_scorable() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_entry(&Entry::new("fs:1", "movie", "A", Source::Plex))
            .unwrap();
        let items = crate::score::load_items(&cat, &["fs:1".to_string()]).unwrap();
        let item = items[0].clone().cast::<rhai::Map>();

        for f in ScalarField::ALL {
            let name = f.name();
            assert_eq!(ScalarField::from_name(name), Some(*f));
            assert!(
                super::super::query::translate(&match f.kind() {
                    ScalarKind::Str => format!("item.{name} != \"zzz\""),
                    ScalarKind::Num => format!("item.{name} > 0"),
                })
                .is_ok(),
                "{name} is not queryable"
            );
            assert!(
                super::super::order::order_by_clause(&[crate::config::FieldSort {
                    field: name.to_string(),
                    dir: crate::config::Dir::Asc,
                }])
                .is_ok(),
                "{name} is not sortable"
            );
            assert!(item.contains_key(name), "{name} missing from scorer item");
        }
    }
}
