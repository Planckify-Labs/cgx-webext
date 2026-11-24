//! `storage.local` / `storage.sync` / `storage.session` semantics: key selection, defaults, quotas, change records.

use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    pub total_bytes: Option<usize>,
    pub per_item_bytes: Option<usize>,
    pub max_items: Option<usize>,
}

impl Quota {
    pub const LOCAL: Quota = Quota { total_bytes: Some(10 * 1024 * 1024), per_item_bytes: None, max_items: None };
    pub const SESSION: Quota = Quota { total_bytes: Some(10 * 1024 * 1024), per_item_bytes: None, max_items: None };
    pub const SYNC: Quota = Quota { total_bytes: Some(102_400), per_item_bytes: Some(8_192), max_items: Some(512) };
    pub const UNLIMITED: Quota = Quota { total_bytes: None, per_item_bytes: None, max_items: None };
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: String,
    pub old_value: Option<Value>,
    pub new_value: Option<Value>,
}

/// Converts changes to the `{ key: { oldValue, newValue } }` object passed to `storage.onChanged`.
pub fn changes_to_json(changes: &[Change]) -> Value {
    let mut out = Map::new();
    for c in changes {
        let mut entry = Map::new();
        if let Some(o) = &c.old_value {
            entry.insert("oldValue".into(), o.clone());
        }
        if let Some(n) = &c.new_value {
            entry.insert("newValue".into(), n.clone());
        }
        out.insert(c.key.clone(), Value::Object(entry));
    }
    Value::Object(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    InvalidKeys,
    InvalidItems,
    QuotaExceeded(&'static str),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::InvalidKeys => {
                f.write_str("Invalid value for argument 'keys': expected a string, an array of strings, an object or null")
            }
            StorageError::InvalidItems => f.write_str("Invalid value for argument 'items': expected an object"),
            StorageError::QuotaExceeded(which) => write!(f, "QUOTA_BYTES quota exceeded ({which})"),
        }
    }
}

impl std::error::Error for StorageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    TrustedContexts,
    TrustedAndUntrustedContexts,
}

impl AccessLevel {
    pub fn parse(v: &Value) -> Result<AccessLevel, String> {
        let level = v.get("accessLevel").and_then(Value::as_str).ok_or_else(|| "accessOptions.accessLevel must be a string".to_string())?;
        match level {
            "TRUSTED_CONTEXTS" => Ok(AccessLevel::TrustedContexts),
            "TRUSTED_AND_UNTRUSTED_CONTEXTS" => Ok(AccessLevel::TrustedAndUntrustedContexts),
            other => Err(format!("Invalid accessLevel: '{other}'")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StorageArea {
    data: Map<String, Value>,
    quota: Quota,
    access_level: AccessLevel,
}

fn item_bytes(key: &str, value: &Value) -> usize {
    // Chrome counts the key length plus the length of the JSON-stringified value.
    key.chars().count() + value.to_string().chars().count()
}

impl StorageArea {
    pub fn new(quota: Quota) -> StorageArea {
        StorageArea { data: Map::new(), quota, access_level: AccessLevel::TrustedAndUntrustedContexts }
    }

    pub fn with_access_level(mut self, level: AccessLevel) -> StorageArea {
        self.access_level = level;
        self
    }

    pub fn access_level(&self) -> AccessLevel {
        self.access_level
    }

    pub fn set_access_level(&mut self, level: AccessLevel) {
        self.access_level = level;
    }

    pub fn allows_untrusted(&self) -> bool {
        self.access_level == AccessLevel::TrustedAndUntrustedContexts
    }

    pub fn set_quota(&mut self, quota: Quota) {
        self.quota = quota;
    }

    pub fn from_json(text: &str, quota: Quota) -> StorageArea {
        let data = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        StorageArea { data, quota, access_level: AccessLevel::TrustedAndUntrustedContexts }
    }

    pub fn to_json(&self) -> String {
        Value::Object(self.data.clone()).to_string()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn keys(&self) -> Vec<String> {
        self.data.keys().cloned().collect()
    }

    /// `get(keys)`: `null` selects everything, a string or array of strings selects those keys, and an object selects its
    /// keys using its values as defaults for missing ones.
    pub fn get(&self, keys: &Value) -> Result<Value, StorageError> {
        let mut out = Map::new();
        match keys {
            Value::Null => return Ok(Value::Object(self.data.clone())),
            Value::String(k) => {
                if let Some(v) = self.data.get(k) {
                    out.insert(k.clone(), v.clone());
                }
            }
            Value::Array(list) => {
                for k in list {
                    let k = k.as_str().ok_or(StorageError::InvalidKeys)?;
                    if let Some(v) = self.data.get(k) {
                        out.insert(k.to_string(), v.clone());
                    }
                }
            }
            Value::Object(defaults) => {
                for (k, default) in defaults {
                    out.insert(k.clone(), self.data.get(k).unwrap_or(default).clone());
                }
            }
            _ => return Err(StorageError::InvalidKeys),
        }
        Ok(Value::Object(out))
    }

    fn selected_keys(keys: &Value, all: &Map<String, Value>) -> Result<Vec<String>, StorageError> {
        match keys {
            Value::Null => Ok(all.keys().cloned().collect()),
            Value::String(k) => Ok(vec![k.clone()]),
            Value::Array(list) => list.iter().map(|k| k.as_str().map(String::from).ok_or(StorageError::InvalidKeys)).collect(),
            Value::Object(o) => Ok(o.keys().cloned().collect()),
            _ => Err(StorageError::InvalidKeys),
        }
    }

    pub fn bytes_in_use(&self, keys: &Value) -> Result<usize, StorageError> {
        let selected = Self::selected_keys(keys, &self.data)?;
        Ok(selected.iter().filter_map(|k| self.data.get(k).map(|v| item_bytes(k, v))).sum())
    }

    /// Atomically applies `items`; on any validation or quota failure nothing changes.
    pub fn set(&mut self, items: &Value) -> Result<Vec<Change>, StorageError> {
        let items = items.as_object().ok_or(StorageError::InvalidItems)?;

        let mut projected_total: usize = self.data.iter().map(|(k, v)| item_bytes(k, v)).sum();
        let mut projected_count = self.data.len();
        for (k, v) in items {
            let new_size = item_bytes(k, v);
            if let Some(limit) = self.quota.per_item_bytes
                && new_size > limit
            {
                return Err(StorageError::QuotaExceeded("QUOTA_BYTES_PER_ITEM"));
            }
            match self.data.get(k) {
                Some(old) => projected_total = projected_total - item_bytes(k, old) + new_size,
                None => {
                    projected_total += new_size;
                    projected_count += 1;
                }
            }
        }
        if let Some(limit) = self.quota.total_bytes
            && projected_total > limit
        {
            return Err(StorageError::QuotaExceeded("QUOTA_BYTES"));
        }
        if let Some(limit) = self.quota.max_items
            && projected_count > limit
        {
            return Err(StorageError::QuotaExceeded("MAX_ITEMS"));
        }

        let mut changes = vec![];
        for (k, v) in items {
            let old = self.data.insert(k.clone(), v.clone());
            if old.as_ref() != Some(v) {
                changes.push(Change { key: k.clone(), old_value: old, new_value: Some(v.clone()) });
            }
        }
        Ok(changes)
    }

    pub fn remove(&mut self, keys: &Value) -> Result<Vec<Change>, StorageError> {
        if matches!(keys, Value::Null | Value::Object(_)) {
            return Err(StorageError::InvalidKeys);
        }
        let selected = Self::selected_keys(keys, &self.data)?;
        let mut changes = vec![];
        for k in selected {
            if let Some(old) = self.data.shift_remove(&k) {
                changes.push(Change { key: k, old_value: Some(old), new_value: None });
            }
        }
        Ok(changes)
    }

    pub fn clear(&mut self) -> Vec<Change> {
        let old = std::mem::take(&mut self.data);
        old.into_iter().map(|(key, v)| Change { key, old_value: Some(v), new_value: None }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn area() -> StorageArea {
        let mut a = StorageArea::new(Quota::LOCAL);
        a.set(&json!({"a": 1, "b": "two", "c": {"x": [1, 2]}})).unwrap();
        a
    }

    #[test]
    fn get_forms() {
        let a = area();
        assert_eq!(a.get(&json!(null)).unwrap(), json!({"a": 1, "b": "two", "c": {"x": [1, 2]}}));
        assert_eq!(a.get(&json!("a")).unwrap(), json!({"a": 1}));
        assert_eq!(a.get(&json!(["a", "zz"])).unwrap(), json!({"a": 1}));
        assert_eq!(a.get(&json!({"a": 99, "zz": "dflt"})).unwrap(), json!({"a": 1, "zz": "dflt"}));
        assert_eq!(a.get(&json!([])).unwrap(), json!({}));
        assert!(a.get(&json!(5)).is_err());
        assert!(a.get(&json!([1])).is_err());
    }

    #[test]
    fn set_reports_only_real_changes() {
        let mut a = area();
        let changes = a.set(&json!({"a": 1, "b": "three", "d": null})).unwrap();
        let keys: Vec<_> = changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["b", "d"]);
        assert_eq!(changes[0].old_value, Some(json!("two")));
        assert_eq!(changes[0].new_value, Some(json!("three")));
        assert_eq!(changes[1].old_value, None);
        assert_eq!(changes_to_json(&changes), json!({"b": {"oldValue": "two", "newValue": "three"}, "d": {"newValue": null}}));
        assert!(a.set(&json!([1])).is_err());
    }

    #[test]
    fn remove_and_clear() {
        let mut a = area();
        let ch = a.remove(&json!(["a", "nope"])).unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].key, "a");
        assert!(a.remove(&json!(null)).is_err());
        assert!(a.remove(&json!({"b": 1})).is_err());
        assert_eq!(a.remove(&json!("b")).unwrap().len(), 1);
        assert_eq!(a.clear().len(), 1);
        assert!(a.is_empty());
        assert!(a.clear().is_empty());
    }

    #[test]
    fn bytes_in_use_counts_key_plus_json() {
        let a = area();
        assert_eq!(a.bytes_in_use(&json!("a")).unwrap(), 1 + 1);
        assert_eq!(a.bytes_in_use(&json!("b")).unwrap(), 1 + 5); // "two" with quotes
        assert_eq!(a.bytes_in_use(&json!(null)).unwrap(), 2 + 6 + (1 + r#"{"x":[1,2]}"#.len()));
        assert_eq!(a.bytes_in_use(&json!("missing")).unwrap(), 0);
    }

    #[test]
    fn sync_quotas_are_enforced_atomically() {
        let mut a = StorageArea::new(Quota::SYNC);
        a.set(&json!({"ok": 1})).unwrap();
        let huge = "x".repeat(8200);
        assert_eq!(a.set(&json!({"fine": 1, "big": huge})), Err(StorageError::QuotaExceeded("QUOTA_BYTES_PER_ITEM")));
        assert_eq!(a.get(&json!("fine")).unwrap(), json!({}), "a failed set must not apply anything");

        let mut many = Map::new();
        for i in 0..513 {
            many.insert(format!("k{i}"), json!(1));
        }
        assert_eq!(a.set(&Value::Object(many)), Err(StorageError::QuotaExceeded("MAX_ITEMS")));

        let mut b = StorageArea::new(Quota::SYNC);
        let chunk = "y".repeat(7000);
        for i in 0..14 {
            b.set(&json!({ format!("c{i}"): chunk })).unwrap();
        }
        assert_eq!(b.set(&json!({"c14": chunk})), Err(StorageError::QuotaExceeded("QUOTA_BYTES")));
        // Overwriting an existing item with a same-size value stays within quota.
        assert!(b.set(&json!({"c0": chunk})).is_ok());
    }

    #[test]
    fn unlimited_quota_lifts_total_limit() {
        let mut a = StorageArea::new(Quota::LOCAL);
        let big = "z".repeat(11 * 1024 * 1024);
        assert!(a.set(&json!({"big": big})).is_err());
        a.set_quota(Quota::UNLIMITED);
        assert!(a.set(&json!({"big": big})).is_ok());
    }

    /// The JavaScript shim exposes the quotas as constants (`storage.sync.QUOTA_BYTES`, ...); they must match what is enforced.
    #[test]
    fn shim_quota_constants_match_enforced_quotas() {
        let shim = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Shim/webext-shim.js")).unwrap();
        let expect = |name: &str, value: usize| {
            let line = format!("'{name}': {value},");
            assert!(shim.contains(&line), "Shim/webext-shim.js must contain `{line}`");
        };
        expect("storage.sync.QUOTA_BYTES", Quota::SYNC.total_bytes.unwrap());
        expect("storage.sync.QUOTA_BYTES_PER_ITEM", Quota::SYNC.per_item_bytes.unwrap());
        expect("storage.sync.MAX_ITEMS", Quota::SYNC.max_items.unwrap());
        expect("storage.local.QUOTA_BYTES", Quota::LOCAL.total_bytes.unwrap());
        expect("storage.session.QUOTA_BYTES", Quota::SESSION.total_bytes.unwrap());
    }

    #[test]
    fn persistence_roundtrip_and_corrupt_input() {
        let a = area();
        let b = StorageArea::from_json(&a.to_json(), Quota::LOCAL);
        assert_eq!(a.get(&json!(null)).unwrap(), b.get(&json!(null)).unwrap());
        assert!(StorageArea::from_json("not json", Quota::LOCAL).is_empty());
        assert!(StorageArea::from_json("[1,2]", Quota::LOCAL).is_empty());
    }
}
