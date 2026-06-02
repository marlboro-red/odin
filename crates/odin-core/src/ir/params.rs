//! Workflow input parameter schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single declared input parameter. The validator checks a
/// [`crate::api::RunInput`]'s `params` against these at run start.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ParamSpec {
    /// Value type. Defaults to `string`.
    #[serde(rename = "type", default)]
    pub ty: ParamType,
    /// Whether the caller must supply this param.
    #[serde(default)]
    pub required: bool,
    /// Default value when not supplied. Contradicts `required: true` (`ODIN022`, warning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Minimal parameter value type. Richer typing is a future seam (add variants).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ParamType {
    /// A UTF-8 string (the default).
    #[default]
    String,
    /// A JSON number.
    Number,
    /// A boolean.
    Bool,
}

impl ParamType {
    /// Whether `value` conforms to this declared type. Enforced at run start
    /// ([`crate::error::Error::Input`]) and on a param `default` at validate time (`ODIN030`).
    #[must_use]
    pub fn matches(self, value: &Value) -> bool {
        match self {
            ParamType::String => value.is_string(),
            ParamType::Number => value.is_number(),
            ParamType::Bool => value.is_boolean(),
        }
    }

    /// The lowercase type name (`"string"` / `"number"` / `"bool"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Number => "number",
            ParamType::Bool => "bool",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ParamSpec, ParamType};

    #[test]
    fn parses_with_defaults() {
        let spec: ParamSpec = serde_yaml_ng::from_str("type: number\nrequired: true\n").unwrap();
        assert_eq!(spec.ty, ParamType::Number);
        assert!(spec.required);
        assert!(spec.default.is_none());
    }

    #[test]
    fn type_defaults_to_string() {
        let spec: ParamSpec = serde_yaml_ng::from_str("description: a thing\n").unwrap();
        assert_eq!(spec.ty, ParamType::String);
    }
}
