//! Serialization of the columns that hold structured values.
//!
//! Collections and enums are stored as JSON text. This module is the single
//! place that knows how a domain type becomes a column value and back, so a
//! change to a shape is found in one file rather than scattered across the
//! repositories.
//!
//! ## What this does not do
//!
//! It never invents a value. A field that is absent deserializes to its
//! default, and a field that cannot be deserialized is a
//! [`StoreError::Serialization`] — never a silently-empty collection. A task
//! whose `expected_outputs` column holds something that is not a JSON array of
//! `ExpectedOutput` is a sign the column and the type drifted, and the store
//! says so rather than returning a task with no criteria.

use director_domain::StoreError;

/// Serialize a value into the JSON text a column holds.
pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value)
        .map_err(|err| StoreError::Serialization(format!("could not serialize: {err}")))
}

/// Serialize, where `None` maps to SQL NULL rather than the JSON text `null`.
/// This is the distinction between "the field is unset" and "the field is the
/// JSON null" — for an optional column, the first is the honest encoding.
pub(crate) fn to_json_or_null<T: serde::Serialize>(
    value: Option<&T>,
) -> Result<Option<String>, StoreError> {
    value.map(to_json).transpose()
}

/// Deserialize a column's JSON text back into a value.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    serde_json::from_str(text)
        .map_err(|err| StoreError::Serialization(format!("could not deserialize: {err}")))
}

/// Deserialize a nullable column. SQL NULL is `None`; text is parsed.
pub(crate) fn from_json_or_none<T: serde::de::DeserializeOwned>(
    text: Option<&str>,
) -> Result<Option<T>, StoreError> {
    text.map(from_json).transpose()
}

/// An empty JSON array, the canonical empty value for a NOT NULL collection
/// column. A NULL collection column is a schema violation, not an empty set.
pub(crate) const EMPTY_ARRAY: &str = "[]";

/// A timestamp as RFC3339 UTC text.
pub(crate) fn timestamp(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339()
}

/// Parse a timestamp, rejecting anything that is not one.
pub(crate) fn parse_timestamp(text: &str) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|err| StoreError::Serialization(format!("bad timestamp {text:?}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::task::ExpectedOutput;

    #[test]
    fn a_collection_round_trips_as_json_text() {
        let items = vec![
            ExpectedOutput {
                criterion: "login works".into(),
                check: Some("cargo test login".into()),
            },
            ExpectedOutput {
                criterion: "no panics".into(),
                check: None,
            },
        ];
        let text = to_json(&items).unwrap();
        assert!(text.starts_with('['));
        let back: Vec<ExpectedOutput> = from_json(&text).unwrap();
        assert_eq!(back, items);
    }

    #[test]
    fn none_maps_to_null_and_back() {
        let value: Option<String> = None;
        assert_eq!(to_json_or_null(value.as_ref()).unwrap(), None);
        assert_eq!(from_json_or_none::<String>(None).unwrap(), None);

        let some = String::from("x");
        assert_eq!(to_json_or_null(Some(&some)).unwrap(), Some("\"x\"".into()));
        assert_eq!(
            from_json_or_none::<String>(Some("\"x\"")).unwrap(),
            Some("x".to_string())
        );
    }

    #[test]
    fn unparseable_json_is_an_error_not_an_empty_value() {
        // The point: a corrupted column surfaces rather than becoming [].
        let err = from_json::<Vec<ExpectedOutput>>("{not json").unwrap_err();
        assert!(matches!(err, StoreError::Serialization(_)));
    }

    #[test]
    fn timestamps_round_trip_in_utc() {
        let now = chrono::Utc::now();
        let text = timestamp(now);
        let back = parse_timestamp(&text).unwrap();
        assert_eq!(back, now);
    }

    #[test]
    fn a_bad_timestamp_is_rejected() {
        assert!(parse_timestamp("yesterday").is_err());
    }
}
