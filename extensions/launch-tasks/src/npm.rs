use std::collections::{BTreeMap, BTreeSet};

use zed_extension_api::serde_json::{Value, json};

use crate::project_files::{
    ListDirectory, ReadFile, directory_of, file_name, find_files, join, parse_json,
};

/// In priority order: the first lock file found in a directory decides the package manager.
const LOCK_FILES: [(&str, &str); 7] = [
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("pnpm-lock.yaml", "pnpm"),
    ("pnpm-workspace.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
    ("npm-shrinkwrap.json", "npm"),
];

/// Scripts named like these (before a `:`) are servers or watchers, so they get their own terminal.
const LONG_RUNNING_SCRIPTS: [&str; 5] = ["start", "dev", "serve", "watch", "preview"];

pub fn generate_tasks(list_directory: ListDirectory, read_file: ReadFile) -> Vec<Value> {
    let files = find_files(list_directory);
    let packages = files
        .iter()
        .filter(|path| file_name(path) == "package.json")
        .filter_map(|path| {
            let package = parse_json(&read_file(path).ok()?)?;
            Some((directory_of(path), package))
        })
        .collect::<BTreeMap<_, _>>();

    let mut tasks = Vec::new();
    for (&directory, package) in &packages {
        let manager = package_manager(directory, &packages, &files);
        let scripts = package["scripts"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, command)| command.is_string())
            .map(|(name, _)| name.as_str());
        tasks.push(task(manager, directory, &["install"], false));
        for script in scripts {
            let long_running = script
                .split(':')
                .next()
                .is_some_and(|prefix| LONG_RUNNING_SCRIPTS.contains(&prefix));
            tasks.push(task(manager, directory, &["run", script], long_running));
        }
    }
    tasks
}

fn task(manager: &str, directory: &str, args: &[&str], long_running: bool) -> Value {
    let location = if directory.is_empty() {
        String::new()
    } else {
        format!(" ({directory})")
    };
    let mut task = json!({
        "label": format!("{manager} {}{location}", args.join(" ")),
        "command": manager,
        "args": args,
        "use_new_terminal": long_running,
    });
    if !directory.is_empty() {
        task["cwd"] = json!(format!("$ZED_WORKTREE_ROOT/{directory}"));
    }
    task
}

/// Looks at the directory itself, then at its parents, so a monorepo root decides for its packages.
fn package_manager(
    directory: &str,
    packages: &BTreeMap<&str, Value>,
    files: &BTreeSet<String>,
) -> &'static str {
    let mut current = directory;
    loop {
        let declared = packages.get(current).and_then(declared_manager);
        let locked = || {
            LOCK_FILES
                .iter()
                .find(|(lock_file, _)| files.contains(&join(current, lock_file)))
                .map(|(_, manager)| *manager)
        };
        if let Some(manager) = declared.or_else(locked) {
            return manager;
        }
        if current.is_empty() {
            return "npm";
        }
        current = directory_of(current);
    }
}

/// The `packageManager` field, then `devEngines.packageManager` (an object or a list of them).
fn declared_manager(package: &Value) -> Option<&'static str> {
    let dev_engines = match &package["devEngines"]["packageManager"] {
        Value::Array(items) => items.iter().map(|item| item["name"].as_str()).collect(),
        other => vec![other["name"].as_str()],
    };
    std::iter::once(package["packageManager"].as_str())
        .chain(dev_engines)
        .flatten()
        .find_map(manager_name)
}

fn manager_name(value: &str) -> Option<&'static str> {
    match value.split('@').next()? {
        "pnpm" => Some("pnpm"),
        "yarn" => Some("yarn"),
        "npm" => Some("npm"),
        "bun" => Some("bun"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_files::test_support::{FakeWorktree, labels, summarize};

    const SCRIPTS: &str = r#"{ "scripts": { "build": "tsc", "dev:web": "vite", "broken": 1 } }"#;

    fn tasks_for(files: &[(&str, &str)]) -> Vec<(String, Vec<String>)> {
        let worktree = FakeWorktree::new(files);
        summarize(generate_tasks(
            &|directory| worktree.list_directory(directory),
            &|path| worktree.read_text_file(path),
        ))
    }

    #[test]
    fn scripts_use_the_package_manager_of_the_lock_file() {
        let tasks = tasks_for(&[("package.json", SCRIPTS), ("pnpm-lock.yaml", "")]);
        assert_eq!(
            labels(&tasks),
            ["pnpm install", "pnpm run build", "pnpm run dev:web"]
        );
        assert_eq!(tasks[1].1, ["run", "build"]);
    }

    #[test]
    fn package_manager_field_beats_lock_file() {
        let package = r#"{ "packageManager": "yarn@4.1.0", "scripts": { "build": "x" } }"#;
        let tasks = tasks_for(&[("package.json", package), ("package-lock.json", "")]);
        assert_eq!(labels(&tasks), ["yarn install", "yarn run build"]);
    }

    #[test]
    fn dev_engines_list_picks_the_first_known_manager() {
        let package =
            r#"{ "devEngines": { "packageManager": [{ "name": "foo" }, { "name": "bun" }] } }"#;
        assert_eq!(
            labels(&tasks_for(&[("package.json", package)])),
            ["bun install"]
        );
    }

    #[test]
    fn no_hints_falls_back_to_npm() {
        let tasks = tasks_for(&[("package.json", SCRIPTS)]);
        assert_eq!(tasks[1].0, "npm run build");
    }

    #[test]
    fn workspace_packages_use_the_root_lock_file_and_their_own_directory() {
        let tasks = tasks_for(&[("bun.lock", ""), ("packages/web/package.json", SCRIPTS)]);
        assert_eq!(tasks[1].0, "bun run build (packages/web)");
    }

    #[test]
    fn servers_get_their_own_terminal_and_packages_their_directory() {
        let worktree = FakeWorktree::new(&[("web/package.json", SCRIPTS)]);
        let tasks = generate_tasks(&|directory| worktree.list_directory(directory), &|path| {
            worktree.read_text_file(path)
        });
        let build = &tasks[1];
        let dev = &tasks[2];
        assert_eq!(build["use_new_terminal"], false);
        assert_eq!(dev["use_new_terminal"], true);
        assert_eq!(dev["cwd"], "$ZED_WORKTREE_ROOT/web");
    }

    #[test]
    fn invalid_package_json_is_skipped() {
        assert!(tasks_for(&[("package.json", "{")]).is_empty());
    }

    #[test]
    fn package_json_with_a_bom_is_read() {
        let package = format!("\u{feff}{SCRIPTS}");
        assert_eq!(
            tasks_for(&[("package.json", &package)])[1].0,
            "npm run build"
        );
    }
}
