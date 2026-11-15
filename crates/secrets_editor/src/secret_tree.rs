use std::fmt;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::environments::SecretsFormat;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct KeyPath(pub Vec<Segment>);

impl fmt::Display for KeyPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(key) if position == 0 => write!(formatter, "{key}")?,
                Segment::Key(key) => write!(formatter, ".{key}")?,
                Segment::Index(index) => write!(formatter, "[{index}]")?,
            }
        }
        Ok(())
    }
}

impl KeyPath {
    /// None for keys sops paths cannot address: segments are split on `[` and never unescaped.
    pub(crate) fn sops_path(&self) -> Option<String> {
        let mut path = String::new();
        for segment in &self.0 {
            match segment {
                Segment::Key(key) if key.contains(['"', '[', ']']) => return None,
                Segment::Key(key) => path.push_str(&format!("[\"{key}\"]")),
                Segment::Index(index) => path.push_str(&format!("[{index}]")),
            }
        }
        Some(path)
    }

    pub(crate) fn has_index(&self) -> bool {
        self.0
            .iter()
            .any(|segment| matches!(segment, Segment::Index(_)))
    }

    pub(crate) fn overlaps(&self, other: &KeyPath) -> bool {
        self.0.starts_with(&other.0) || other.0.starts_with(&self.0)
    }

    pub(crate) fn parse(text: &str, format: SecretsFormat) -> Option<KeyPath> {
        let text = text.trim();
        if text.is_empty() || text.contains('"') {
            return None;
        }
        if format == SecretsFormat::Dotenv {
            let invalid = text.contains('=')
                || text.contains(char::is_whitespace)
                || text.starts_with("sops_");
            let key = KeyPath(vec![Segment::Key(text.to_string())]);
            return (!invalid && key.sops_path().is_some()).then_some(key);
        }

        let mut segments = Vec::new();
        for part in text.split('.') {
            let (name, mut rest) = match part.find('[') {
                Some(position) => part.split_at(position),
                None => (part, ""),
            };
            if name.is_empty() {
                return None;
            }
            segments.push(Segment::Key(name.to_string()));
            while !rest.is_empty() {
                let (number, after) = rest.strip_prefix('[')?.split_once(']')?;
                segments.push(Segment::Index(number.parse().ok()?));
                rest = after;
            }
        }
        if segments.first() == Some(&Segment::Key("sops".to_string())) {
            return None;
        }
        let key = KeyPath(segments);
        key.sops_path().is_some().then_some(key)
    }
}

pub(crate) fn leaves(value: &Value) -> Vec<(KeyPath, Value)> {
    let mut result = Vec::new();
    collect_leaves(value, &mut Vec::new(), &mut result);
    result
}

fn collect_leaves(value: &Value, prefix: &mut Vec<Segment>, result: &mut Vec<(KeyPath, Value)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                prefix.push(Segment::Key(key.clone()));
                collect_leaves(child, prefix, result);
                prefix.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                prefix.push(Segment::Index(index));
                collect_leaves(child, prefix, result);
                prefix.pop();
            }
        }
        scalar => result.push((KeyPath(prefix.clone()), scalar.clone())),
    }
}

pub(crate) fn encrypted_keys(text: &str, format: SecretsFormat) -> Result<Vec<KeyPath>> {
    if format == SecretsFormat::Dotenv {
        return Ok(text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once('=').map(|(key, _)| key))
            .filter(|key| !key.starts_with("sops_"))
            .map(|key| KeyPath(vec![Segment::Key(key.to_string())]))
            .collect());
    }

    let Value::Object(mut map) = serde_yaml::from_str::<Value>(text)? else {
        bail!("expected a mapping at the top level");
    };
    map.shift_remove("sops");
    Ok(leaves(&Value::Object(map))
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

/// Public age keys the file is encrypted for, read from the plaintext sops metadata.
pub(crate) fn age_recipients(text: &str, format: SecretsFormat) -> Vec<String> {
    if format == SecretsFormat::Dotenv {
        return text
            .lines()
            .filter_map(|line| {
                let (_, recipient) = line
                    .strip_prefix("sops_age__list_")?
                    .split_once("__map_recipient=")?;
                Some(recipient.to_string())
            })
            .collect();
    }
    let Ok(document) = serde_yaml::from_str::<Value>(text) else {
        return Vec::new();
    };
    document
        .pointer("/sops/age")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.get("recipient")?.as_str()?.to_string()))
        .collect()
}

pub(crate) fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn edited_value(original: Option<&Value>, text: &str) -> Value {
    if matches!(
        original,
        Some(Value::Number(_) | Value::Bool(_) | Value::Null)
    ) && let Ok(parsed @ (Value::Number(_) | Value::Bool(_) | Value::Null)) =
        serde_json::from_str::<Value>(text.trim())
    {
        return parsed;
    }
    Value::String(text.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key_path(segments: &[Segment]) -> KeyPath {
        KeyPath(segments.to_vec())
    }

    fn key(name: &str) -> Segment {
        Segment::Key(name.to_string())
    }

    #[test]
    fn test_display() {
        assert_eq!(
            key_path(&[key("db"), key("password")]).to_string(),
            "db.password"
        );
        assert_eq!(
            key_path(&[key("list"), Segment::Index(0)]).to_string(),
            "list[0]"
        );
        assert_eq!(
            key_path(&[key("list"), Segment::Index(0), key("name")]).to_string(),
            "list[0].name"
        );
    }

    #[test]
    fn test_sops_path() {
        assert_eq!(
            key_path(&[key("db"), Segment::Index(2), key("a\\b")]).sops_path(),
            Some(r#"["db"][2]["a\b"]"#.to_string())
        );
        assert_eq!(key_path(&[key("a\"b")]).sops_path(), None);
        assert_eq!(key_path(&[key("a[b")]).sops_path(), None);
    }

    #[test]
    fn test_parse() {
        assert_eq!(KeyPath::parse("  ", SecretsFormat::Yaml), None);
        assert_eq!(
            KeyPath::parse(" TOKEN ", SecretsFormat::Dotenv),
            Some(key_path(&[key("TOKEN")]))
        );
        assert_eq!(KeyPath::parse("A=B", SecretsFormat::Dotenv), None);
        assert_eq!(KeyPath::parse("A B", SecretsFormat::Dotenv), None);
        assert_eq!(KeyPath::parse("sops_mac", SecretsFormat::Dotenv), None);
        assert_eq!(
            KeyPath::parse("db.password", SecretsFormat::Yaml),
            Some(key_path(&[key("db"), key("password")]))
        );
        assert_eq!(
            KeyPath::parse("list[0].name", SecretsFormat::Json),
            Some(key_path(&[key("list"), Segment::Index(0), key("name")]))
        );
        assert_eq!(
            KeyPath::parse("grid[1][2]", SecretsFormat::Yaml),
            Some(key_path(&[
                key("grid"),
                Segment::Index(1),
                Segment::Index(2)
            ]))
        );
        assert_eq!(KeyPath::parse("a..b", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("a.", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("[0]", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("a[x]", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("a[0", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("sops.mac", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("a\"b", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("a]b", SecretsFormat::Yaml), None);
        assert_eq!(KeyPath::parse("A[0]", SecretsFormat::Dotenv), None);
    }

    #[test]
    fn test_leaves_document_order() {
        let value = json!({
            "zeta": "1",
            "db": { "password": "p", "empty": {}, "hosts": ["a", {"name": "b"}], "none": [] },
            "alpha": 3,
        });
        let paths: Vec<String> = leaves(&value)
            .into_iter()
            .map(|(path, _)| path.to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "zeta",
                "db.password",
                "db.hosts[0]",
                "db.hosts[1].name",
                "alpha"
            ]
        );
    }

    #[test]
    fn test_encrypted_keys_dotenv() {
        let text = "FIRST=ENC[AES256_GCM,data:abc,tag:def,type:str]\n#ENC[AES256_GCM,data:xyz,tag:uvw,type:comment]\n\nSECOND=ENC[AES256_GCM,data:1,tag:2,type:str]\nsops_age__list_0__map_enc=-----BEGIN\nsops_mac=ENC[x]\n";
        let paths: Vec<String> = encrypted_keys(text, SecretsFormat::Dotenv)
            .unwrap()
            .into_iter()
            .map(|path| path.to_string())
            .collect();
        assert_eq!(paths, ["FIRST", "SECOND"]);
    }

    #[test]
    fn test_encrypted_keys_yaml() {
        let text = "db:\n    password: ENC[AES256_GCM,data:a,tag:b,type:str]\nlist:\n    - ENC[AES256_GCM,data:c,tag:d,type:str]\nsops:\n    mac: ENC[x]\n    version: 3.13.3\n";
        let paths: Vec<String> = encrypted_keys(text, SecretsFormat::Yaml)
            .unwrap()
            .into_iter()
            .map(|path| path.to_string())
            .collect();
        assert_eq!(paths, ["db.password", "list[0]"]);
        assert!(encrypted_keys("- a", SecretsFormat::Yaml).is_err());
    }

    #[test]
    fn test_age_recipients() {
        let dotenv = "A=ENC[x]\nsops_age__list_0__map_recipient=age1one\nsops_age__list_1__map_recipient=age1two\nsops_mac=ENC[y]\n";
        assert_eq!(
            age_recipients(dotenv, SecretsFormat::Dotenv),
            ["age1one", "age1two"]
        );
        let yaml =
            "a: ENC[x]\nsops:\n  age:\n    - recipient: age1one\n      enc: x\n  mac: ENC[y]\n";
        assert_eq!(age_recipients(yaml, SecretsFormat::Yaml), ["age1one"]);
        assert!(age_recipients("a: 1", SecretsFormat::Yaml).is_empty());
    }

    #[test]
    fn test_display_value() {
        assert_eq!(display_value(&json!("text")), "text");
        assert_eq!(display_value(&json!(12)), "12");
        assert_eq!(display_value(&json!(true)), "true");
        assert_eq!(display_value(&Value::Null), "null");
    }

    #[test]
    fn test_edited_value() {
        assert_eq!(edited_value(Some(&json!(1)), "42"), json!(42));
        assert_eq!(edited_value(Some(&json!(1)), "abc"), json!("abc"));
        assert_eq!(edited_value(Some(&json!(true)), "false"), json!(false));
        assert_eq!(edited_value(Some(&Value::Null), "null"), Value::Null);
        assert_eq!(edited_value(Some(&json!("text")), "42"), json!("42"));
        assert_eq!(edited_value(None, "42"), json!("42"));
        assert_eq!(edited_value(Some(&json!(1)), "[1]"), json!("[1]"));
    }
}
