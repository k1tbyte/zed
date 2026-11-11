mod dotnet;
mod npm;
mod project_files;

use zed_extension_api::{self as zed, Result, serde_json::Value};

struct LaunchTasks;

impl zed::Extension for LaunchTasks {
    fn new() -> Self {
        Self
    }

    fn provide_tasks(
        &mut self,
        provider_name: String,
        worktree: &zed::Worktree,
    ) -> Result<Vec<Value>> {
        let list_directory = |directory: &str| worktree.list_directory(directory, None);
        let read_file = |path: &str| worktree.read_text_file(path);
        match provider_name.as_str() {
            "dotnet" => Ok(dotnet::generate_tasks(&list_directory, &read_file)),
            "npm" => Ok(npm::generate_tasks(&list_directory, &read_file)),
            _ => Err(format!("unknown task provider {provider_name}")),
        }
    }
}

zed::register_extension!(LaunchTasks);
