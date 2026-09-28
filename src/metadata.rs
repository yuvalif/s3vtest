//! Per-vector JSON metadata generation, and conversion to the SDK `Document`.

use aws_smithy_types::{Document, Number};
use rand::distr::{Alphanumeric, SampleString};
use rand::Rng;
use serde_json::Value;

use crate::config::{MetadataField, ValueSpec};
use crate::sample::SeededRng;

pub struct MetadataGen {
    fields: Vec<MetadataField>,
}

impl MetadataGen {
    pub fn new(fields: &[MetadataField]) -> Self {
        Self {
            fields: fields.to_vec(),
        }
    }

    pub fn generate(&self, rng: &mut SeededRng) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::with_capacity(self.fields.len());
        for f in &self.fields {
            if f.probability < 1.0 && !rng.random_bool(f.probability) {
                continue;
            }
            m.insert(f.name.clone(), gen_value(&f.value, rng));
        }
        m
    }
}

fn gen_value(spec: &ValueSpec, rng: &mut SeededRng) -> Value {
    match spec {
        ValueSpec::Const(v) => v.clone(),
        ValueSpec::RandomString(n) => Value::String(Alphanumeric.sample_string(rng, *n)),
        ValueSpec::Range([lo, hi]) => {
            if lo.fract() == 0.0 && hi.fract() == 0.0 {
                Value::from(rng.random_range(*lo as i64..=*hi as i64))
            } else {
                Value::from(rng.random_range(*lo..=*hi))
            }
        }
        ValueSpec::Choice(items) => items[rng.random_range(0..items.len())].clone(),
    }
}

pub fn json_to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else {
                Document::Number(Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(a) => Document::Array(a.iter().map(json_to_document).collect()),
        Value::Object(o) => Document::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn generates_fields_by_spec() {
        let fields = vec![
            MetadataField {
                name: "a".into(),
                value: ValueSpec::Const(Value::from("x")),
                probability: 1.0,
            },
            MetadataField {
                name: "b".into(),
                value: ValueSpec::Range([1.0, 3.0]),
                probability: 1.0,
            },
            MetadataField {
                name: "c".into(),
                value: ValueSpec::RandomString(8),
                probability: 1.0,
            },
            MetadataField {
                name: "d".into(),
                value: ValueSpec::Choice(vec![Value::from(1)]),
                probability: 0.0,
            },
        ];
        let g = MetadataGen::new(&fields);
        let mut rng = SeededRng::seed_from_u64(7);
        let m = g.generate(&mut rng);
        assert_eq!(m["a"], "x");
        assert!(m["b"].is_i64());
        assert_eq!(m["c"].as_str().unwrap().len(), 8);
        assert!(!m.contains_key("d"));
    }
}
