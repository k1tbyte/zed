use project::WorktreeId;
use serde_json::json;
use util::rel_path::RelPath;

use super::*;

fn key(text: &str) -> KeyPath {
    KeyPath::parse(text, SecretsFormat::Yaml).unwrap()
}

fn file(name: &str, state: EnvironmentState) -> EnvironmentFile {
    EnvironmentFile {
        name: name.to_string().into(),
        abs_path: PathBuf::from(format!("/{name}.sops.yaml")),
        format: SecretsFormat::Yaml,
        state,
        recipients: Vec::new(),
    }
}

fn unlocked(name: &str, values: &[(&str, Value)]) -> EnvironmentFile {
    let values = values
        .iter()
        .map(|(text, value)| (key(text), value.clone()))
        .collect();
    file(name, EnvironmentState::Unlocked { values })
}

fn group(environments: Vec<EnvironmentFile>) -> SecretsGroup {
    let opened = ProjectPath {
        worktree_id: WorktreeId::from_usize(0),
        path: RelPath::empty_arc(),
    };
    SecretsGroup::new(
        "{env}.sops.yaml".to_string().into(),
        opened,
        None,
        environments,
    )
}

fn two_environments() -> SecretsGroup {
    group(vec![
        unlocked("stg", &[("A", json!("1")), ("B", json!(2))]),
        unlocked("prod", &[("B", json!(3)), ("C", json!("x"))]),
    ])
}

fn rows(group: &SecretsGroup) -> Vec<String> {
    group.rows().iter().map(ToString::to_string).collect()
}

fn value(text: &str, edited: bool) -> Cell {
    Cell::Value {
        text: text.to_string(),
        edited,
    }
}

#[test]
fn test_rows_and_cells() {
    let group = two_environments();
    assert_eq!(rows(&group), ["A", "B", "C"]);
    assert_eq!(group.cell(0, &key("A")), value("1", false));
    assert_eq!(group.cell(0, &key("B")), value("2", false));
    assert_eq!(group.cell(0, &key("C")), Cell::Missing);
    assert_eq!(group.cell(1, &key("A")), Cell::Missing);
    assert!(!group.is_dirty());
}

#[test]
fn test_locked_cells_use_known_keys() {
    let locked = file(
        "prod",
        EnvironmentState::Locked {
            keys: vec![key("Z")],
            error: None,
        },
    );
    let group = group(vec![unlocked("stg", &[("A", json!("1"))]), locked]);
    assert_eq!(rows(&group), ["A", "Z"]);
    assert_eq!(group.cell(1, &key("Z")), Cell::Locked);
    assert_eq!(group.cell(1, &key("A")), Cell::Missing);
    assert!(group.any_locked());
    let missing = group.missing();
    assert_eq!(missing.len(), 2);
    assert_eq!(missing[0].0.as_ref(), "stg");
    assert_eq!(missing[1].0.as_ref(), "prod");
}

#[test]
fn test_set_value_keeps_typing_and_drops_noop() {
    let mut group = two_environments();
    group.set_value(0, key("A"), "changed");
    assert_eq!(group.cell(0, &key("A")), value("changed", true));
    assert!(group.is_dirty());

    group.set_value(0, key("A"), "1");
    assert_eq!(group.cell(0, &key("A")), value("1", false));
    assert!(!group.is_dirty());

    group.set_value(0, key("B"), "5");
    assert_eq!(group.edits[&(0, key("B"))], PendingEdit::Set(json!(5)));
    group.set_value(0, key("B"), "2");
    assert!(!group.is_dirty());

    group.set_value(0, key("C"), "new");
    assert_eq!(group.cell(0, &key("C")), value("new", true));
}

#[test]
fn test_unset() {
    let mut group = two_environments();
    group.unset(0, key("A"));
    assert_eq!(group.cell(0, &key("A")), Cell::Removed);
    group.set_value(0, key("C"), "new");
    group.unset(0, key("C"));
    assert_eq!(group.cell(0, &key("C")), Cell::Missing);
    assert_eq!(group.edits.len(), 1);
}

#[test]
fn test_remove_key() {
    let mut group = two_environments();
    group.set_value(0, key("B"), "9");
    group.remove_key(&key("B"));
    assert_eq!(group.cell(0, &key("B")), Cell::Removed);
    assert_eq!(group.cell(1, &key("B")), Cell::Removed);

    let added = group.add_key("D").unwrap();
    group.set_value(0, added.clone(), "v");
    group.remove_key(&added);
    assert_eq!(rows(&group), ["A", "B", "C"]);
    assert_eq!(group.cell(0, &added), Cell::Missing);
}

#[test]
fn test_add_key() {
    let mut group = two_environments();
    assert_eq!(group.add_key(" db.host ").unwrap().to_string(), "db.host");
    assert_eq!(rows(&group), ["A", "B", "C", "db.host"]);
    assert!(group.add_key("A").is_err());
    assert!(group.add_key("db.host").is_err());
    assert!(group.add_key("").is_err());
    assert!(group.add_key("sops.mac").is_err());
    assert!(group.add_key("db").is_err());
    assert!(group.add_key("A.nested").is_err());
    assert!(!group.is_dirty());
}

#[test]
fn test_missing() {
    let mut group = two_environments();
    let missing = group.missing();
    assert_eq!(missing.len(), 2);
    assert_eq!(missing[0].0.as_ref(), "stg");
    assert_eq!(missing[0].1, [key("C")]);
    assert_eq!(missing[1].0.as_ref(), "prod");
    assert_eq!(missing[1].1, [key("A")]);

    group.set_value(0, key("C"), "x");
    group.set_value(1, key("A"), "y");
    assert!(group.missing().is_empty());

    group.unset(1, key("B"));
    let missing = group.missing();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].1, [key("B")]);
    group.remove_key(&key("B"));
    assert!(group.missing().is_empty());

    let single = group_with_single_environment();
    assert!(single.missing().is_empty());
}

fn group_with_single_environment() -> SecretsGroup {
    group(vec![unlocked("only", &[("A", json!("1"))])])
}

#[test]
fn test_changes() {
    let mut group = two_environments();
    group.set_value(0, key("A"), "changed");
    group.unset(1, key("B"));
    group.add_key("D").unwrap();
    group.set_value(1, key("D"), "d");

    let changes = group.changes();
    assert_eq!(changes.len(), 2);
    assert!(
        matches!(changes[0].as_slice(), [Change::Set(path, value)] if *path == key("A") && *value == json!("changed"))
    );
    assert!(
        matches!(changes[1].as_slice(), [Change::Unset(removed), Change::Set(added, value)]
            if *removed == key("B") && *added == key("D") && *value == json!("d"))
    );
}

#[test]
fn test_decrypt_drops_edits_that_reached_disk() {
    let mut group = two_environments();
    group.set_value(0, key("A"), "saved");
    group.set_value(0, key("B"), "pending");
    group.unset(1, key("B"));
    let added = group.add_key("D").unwrap();
    group.set_value(0, added, "d");

    group.finish_decrypt(0, Ok(json!({"A": "saved", "B": 2, "D": "d"})));
    assert!(group.added_keys.is_empty());
    assert_eq!(group.edits.len(), 2);

    group.finish_decrypt(1, Ok(json!({"C": "x"})));
    assert_eq!(group.edits.len(), 1);
    assert_eq!(group.cell(0, &key("B")), value("pending", true));
}

#[test]
fn test_array_elements_are_not_removable() {
    let mut group = group(vec![unlocked("stg", &[("list[0]", json!("a"))])]);
    group.unset(0, key("list[0]"));
    group.remove_key(&key("list[0]"));
    assert!(!group.is_dirty());
}

#[test]
fn test_edits_during_save_survive_reconcile() {
    let mut group = two_environments();
    group.saving = true;
    group.set_value(0, key("A"), "1");
    group.unset(0, key("C"));
    assert_eq!(group.edits.len(), 2);

    group.finish_decrypt(0, Ok(json!({"A": "saved", "B": 2, "C": "added"})));
    assert_eq!(group.edits.len(), 2);
}
