use std::collections::{BTreeSet, VecDeque};

use zed_extension_api::{
    Result,
    serde_json::{self, Value},
};

const MAX_DEPTH: usize = 5;
const MAX_DIRECTORIES: usize = 500;

pub type ListDirectory<'a> = &'a dyn Fn(&str) -> Result<Vec<String>>;
pub type ReadFile<'a> = &'a dyn Fn(&str) -> Result<String>;

/// Files of the worktree up to a bounded depth and directory count, as worktree-relative paths.
pub fn find_files(list_directory: ListDirectory) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    // Breadth first, so shallow directories are listed before a huge subtree uses up the budget.
    let mut pending = VecDeque::from([(String::new(), 0)]);
    let mut listed = 0;
    while let Some((directory, depth)) = pending.pop_front() {
        if listed >= MAX_DIRECTORIES {
            break;
        }
        listed += 1;
        let Ok(entries) = list_directory(&directory) else {
            continue;
        };
        for entry in entries {
            match entry.strip_suffix('/') {
                Some(subdirectory) if depth < MAX_DEPTH => {
                    pending.push_back((subdirectory.to_string(), depth + 1));
                }
                Some(_) => {}
                None => {
                    files.insert(entry);
                }
            }
        }
    }
    files
}

/// Like the .NET and Node tooling, ignores a leading UTF-8 BOM that editors on Windows add.
pub fn parse_json(text: &str) -> Option<Value> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)).ok()
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

pub fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_string()
    } else {
        format!("{directory}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::{find_files, test_support::FakeWorktree};

    #[test]
    fn a_huge_subtree_does_not_hide_shallow_directories() {
        let mut paths = vec!["apps/web/package.json".to_string()];
        paths.extend((0..600).map(|index| format!("zzz/directory{index}/file.txt")));
        let files = paths
            .iter()
            .map(|path| (path.as_str(), ""))
            .collect::<Vec<_>>();
        let worktree = FakeWorktree::new(&files);
        let found = find_files(&|directory| worktree.list_directory(directory));
        assert!(found.contains("apps/web/package.json"));
    }
}

#[cfg(test)]
pub mod test_support {
    use std::collections::{BTreeMap, BTreeSet};

    use zed_extension_api::{Result, serde_json::Value};

    pub struct FakeWorktree(BTreeMap<String, String>);

    impl FakeWorktree {
        pub fn new(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(path, contents)| (path.to_string(), contents.to_string()))
                    .collect(),
            )
        }

        pub fn list_directory(&self, directory: &str) -> Result<Vec<String>> {
            let prefix = if directory.is_empty() {
                String::new()
            } else {
                format!("{directory}/")
            };
            let mut entries = BTreeSet::new();
            for path in self.0.keys() {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((subdirectory, _)) => entries.insert(format!("{prefix}{subdirectory}/")),
                    None => entries.insert(path.clone()),
                };
            }
            Ok(entries.into_iter().collect())
        }

        pub fn read_text_file(&self, path: &str) -> Result<String> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| format!("missing {path}"))
        }
    }

    pub fn summarize(tasks: Vec<Value>) -> Vec<(String, Vec<String>)> {
        tasks
            .into_iter()
            .map(|task| {
                let args = task["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|argument| argument.as_str().unwrap().to_string())
                    .collect();
                (task["label"].as_str().unwrap().to_string(), args)
            })
            .collect()
    }

    pub fn labels(tasks: &[(String, Vec<String>)]) -> Vec<&str> {
        tasks.iter().map(|(label, _)| label.as_str()).collect()
    }
}
