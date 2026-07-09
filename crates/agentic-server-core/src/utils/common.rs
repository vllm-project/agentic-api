use chrono::Utc;
use uuid::Uuid;

#[must_use]
pub fn uuid7_str(prefix: &str) -> String {
    format!("{}{}", prefix, Uuid::now_v7())
}

#[must_use]
pub fn utcnow_str() -> i64 {
    Utc::now().timestamp()
}

/// Serialize any type to JSON string.
///
/// Strict serialization - returns error if serialization fails.
/// Used in persistence operations where we control the data types.
///
/// # Errors
///
/// Returns `serde_json::Error` if serialization fails.
pub fn serialize_to_string<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// Deserialize JSON string to any type.
///
/// Strict deserialization - returns error if deserialization fails.
/// Used when we need explicit error handling for data integrity.
///
/// # Errors
///
/// Returns `serde_json::Error` if deserialization fails.
pub fn deserialize_from_str<T: serde::de::DeserializeOwned>(json_str: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(json_str)
}

/// Deserialize JSON string to any type with Default fallback.
///
/// Graceful deserialization - returns default value on error or empty string.
/// Used in read operations where we accept corrupted data gracefully.
#[must_use]
pub fn deserialize_from_str_or_default<T: serde::de::DeserializeOwned + Default>(json_str: &str) -> T {
    serde_json::from_str(json_str).unwrap_or_default()
}

/// Deserialize JSON string to any type, returning None on error.
///
/// Optional deserialization - returns None if JSON is invalid.
/// Convenience function for cases where None represents missing data.
#[must_use]
pub fn deserialize_from_str_opt<T: serde::de::DeserializeOwned>(json_str: &str) -> Option<T> {
    serde_json::from_str(json_str).ok()
}

/// Deserialize optional JSON String to any type, returning default on error or if None.
///
/// Graceful optional deserialization - returns default value for T if missing or invalid.
#[must_use]
pub fn deserialize_from_string_opt_or_default<T: serde::de::DeserializeOwned + Default>(
    json_str: &Option<String>,
) -> T {
    json_str
        .as_ref()
        .and_then(|s| deserialize_from_str_opt::<T>(s))
        .unwrap_or_default()
}

/// Deserialize optional JSON String to any type, returning None on error or if None.
///
/// Optional deserialization - returns None if missing or invalid JSON.
#[must_use]
pub fn deserialize_from_string_opt<T: serde::de::DeserializeOwned>(json_str: &Option<String>) -> Option<T> {
    json_str.as_ref().and_then(|s| deserialize_from_str_opt::<T>(s))
}

/// Deserialize a `serde_json::Value` into `T`.
///
/// # Errors
///
/// Returns `serde_json::Error` if the value's shape does not match `T`.
pub fn deserialize_from_value<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}

/// Deserialize a `serde_json::Value` into `T`, returning `None` on type mismatch.
#[must_use]
pub fn deserialize_from_value_opt<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Option<T> {
    serde_json::from_value(value).ok()
}

/// Serialize any type to JSON bytes, returning an empty `Vec` on error.
#[must_use]
pub fn serialize_to_vec_or_default<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}
