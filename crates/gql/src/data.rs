use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;

/// A GraphQL answer value. An object keeps the order of its selection, as GitHub answers it.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Leaf(Value),
    List(Vec<Data>),
    Object(Vec<(String, Data)>),
}

impl Data {
    pub(crate) const NULL: Data = Data::Leaf(Value::Null);

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Data::Leaf(Value::Null))
    }
}

impl Serialize for Data {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Data::Leaf(v) => v.serialize(s),
            Data::List(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Data::Object(fields) => {
                let mut map = s.serialize_map(Some(fields.len()))?;
                for (k, v) in fields {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}
