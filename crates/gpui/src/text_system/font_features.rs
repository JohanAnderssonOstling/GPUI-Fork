use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, OnceLock};

use schemars::{JsonSchema, json_schema};

/// Whether an OpenType GSUB/GPOS table defines a feature tag. Platform text
/// backends use this on borrowed table data, without copying the font or shaping.
pub fn font_layout_table_has_feature(table: &[u8], tag: [u8; 4]) -> bool {
    if table.get(..2) != Some(&[0, 1][..]) {
        return false;
    }
    let Some(offset) = table.get(6..8) else {
        return false;
    };
    let offset = usize::from(u16::from_be_bytes([offset[0], offset[1]]));
    if offset == 0 {
        return false;
    }
    let Some(features) = table.get(offset..) else {
        return false;
    };
    let Some(count) = features.get(..2) else {
        return false;
    };
    let count = usize::from(u16::from_be_bytes([count[0], count[1]]));
    features
        .get(2..2 + count * 6)
        .is_some_and(|records| records.chunks_exact(6).any(|record| record[..4] == tag))
}

/// The OpenType features that can be configured for a given font.
#[derive(Clone)]
pub struct FontFeatures(Arc<FeatureSettings>);

struct FeatureSettings {
    values: Vec<(String, u32)>,
    hash: u64,
}

impl Default for FontFeatures {
    fn default() -> Self {
        static EMPTY: OnceLock<FontFeatures> = OnceLock::new();
        EMPTY.get_or_init(|| Self::new(Vec::new())).clone()
    }
}

impl PartialEq for FontFeatures {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.hash == other.0.hash && self.0.values == other.0.values)
    }
}

impl Eq for FontFeatures {}

impl Hash for FontFeatures {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash.hash(state);
    }
}

impl FontFeatures {
    /// Create immutable settings, hashing their contents once for font-cache
    /// lookups. Clones share both the values and the cached hash.
    pub fn new(values: Vec<(String, u32)>) -> Self {
        let mut hasher = DefaultHasher::new();
        values.hash(&mut hasher);
        Self(Arc::new(FeatureSettings {
            values,
            hash: hasher.finish(),
        }))
    }

    /// Disables `calt`.
    pub fn disable_ligatures() -> Self {
        static DISABLED: OnceLock<FontFeatures> = OnceLock::new();
        DISABLED
            .get_or_init(|| Self::new(vec![("calt".into(), 0)]))
            .clone()
    }

    /// Get the tag name list of the font OpenType features
    /// only enabled or disabled features are returned
    pub fn tag_value_list(&self) -> &[(String, u32)] {
        &self.0.values
    }

    /// Returns whether the `calt` feature is enabled.
    ///
    /// Returns `None` if the feature is not present.
    pub fn is_calt_enabled(&self) -> Option<bool> {
        self.0
            .values
            .iter()
            .find(|(feature, _)| feature == "calt")
            .map(|(_, value)| *value == 1)
    }
}

impl std::fmt::Debug for FontFeatures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("FontFeatures");
        for (tag, value) in self.tag_value_list() {
            debug.field(tag, value);
        }

        debug.finish()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum FeatureValue {
    Bool(bool),
    Number(serde_json::Number),
}

impl<'de> serde::Deserialize<'de> for FontFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct FontFeaturesVisitor;

        impl<'de> Visitor<'de> for FontFeaturesVisitor {
            type Value = FontFeatures;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map of font features")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut feature_list = Vec::new();

                while let Some((key, value)) =
                    access.next_entry::<String, Option<FeatureValue>>()?
                {
                    if !is_valid_feature_tag(&key) {
                        log::error!("Incorrect font feature tag: {}", key);
                        continue;
                    }
                    if let Some(value) = value {
                        match value {
                            FeatureValue::Bool(enable) => {
                                if enable {
                                    feature_list.push((key, 1));
                                } else {
                                    feature_list.push((key, 0));
                                }
                            }
                            FeatureValue::Number(value) => {
                                if value.is_u64() {
                                    feature_list.push((key, value.as_u64().unwrap() as u32));
                                } else {
                                    log::error!(
                                        "Incorrect font feature value {} for feature tag {}",
                                        value,
                                        key
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }

                Ok(FontFeatures::new(feature_list))
            }
        }

        let features = deserializer.deserialize_map(FontFeaturesVisitor)?;
        Ok(features)
    }
}

impl serde::Serialize for FontFeatures {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;

        for (tag, value) in self.tag_value_list() {
            map.serialize_entry(tag, value)?;
        }

        map.end()
    }
}

impl JsonSchema for FontFeatures {
    fn schema_name() -> Cow<'static, str> {
        "FontFeatures".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "object",
            "patternProperties": {
                "[0-9a-zA-Z]{4}$": {
                    "type": ["boolean", "integer"],
                    "minimum": 0,
                    "multipleOf": 1
                }
            },
            "additionalProperties": false
        })
    }
}

fn is_valid_feature_tag(tag: &str) -> bool {
    tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric())
}
