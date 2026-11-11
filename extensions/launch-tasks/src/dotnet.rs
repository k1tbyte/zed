use std::collections::BTreeSet;

use zed_extension_api::serde_json::{Value, json};

use crate::project_files::{
    ListDirectory, ReadFile, directory_of, file_name, find_files, join, parse_json,
};

#[derive(PartialEq)]
enum ProjectKind {
    Library,
    Executable,
    Test,
}

struct Project {
    path: String,
    kind: ProjectKind,
}

pub fn generate_tasks(list_directory: ListDirectory, read_file: ReadFile) -> Vec<Value> {
    let files = find_files(list_directory);
    let solutions = files_with_extension(&files, &["sln", "slnx"]);
    let projects = files_with_extension(&files, &["csproj", "fsproj", "vbproj"])
        .into_iter()
        .map(|path| {
            let kind = read_file(&path)
                .map(|text| project_kind(&text))
                .unwrap_or(ProjectKind::Library);
            Project { path, kind }
        })
        .collect::<Vec<_>>();
    let has_tests = projects
        .iter()
        .any(|project| project.kind == ProjectKind::Test);

    let mut tasks = Vec::new();
    let solution_names = display_names(&solutions);
    for (solution, name) in solutions.iter().zip(&solution_names) {
        tasks.push(task(name, &["build", solution], false));
        tasks.push(task(name, &["restore", solution], false));
        tasks.push(task(name, &["clean", solution], false));
        if has_tests {
            tasks.push(task(name, &["test", solution], false));
        }
    }

    let project_paths = projects
        .iter()
        .map(|project| project.path.clone())
        .collect::<Vec<_>>();
    let project_names = display_names(&project_paths);
    for (project, name) in projects.iter().zip(&project_names) {
        let path = &project.path;
        if solutions.is_empty() {
            tasks.push(task(name, &["build", path], false));
            tasks.push(task(name, &["restore", path], false));
            tasks.push(task(name, &["clean", path], false));
        }
        match project.kind {
            ProjectKind::Library => {}
            ProjectKind::Test => tasks.push(task(name, &["test", path], false)),
            ProjectKind::Executable => {
                tasks.push(task(name, &["run", "--project", path], true));
                tasks.push(task(name, &["watch", "--project", path, "run"], true));
                for profile in launch_profiles(path, &files, read_file) {
                    tasks.push(task(
                        &format!("{name} [{profile}]"),
                        &["run", "--project", path, "--launch-profile", &profile],
                        true,
                    ));
                }
            }
        }
    }
    tasks
}

fn files_with_extension(files: &BTreeSet<String>, extensions: &[&str]) -> Vec<String> {
    files
        .iter()
        .filter(|path| {
            file_name(path)
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extensions.contains(&extension))
        })
        .cloned()
        .collect()
}

/// File names, or the full paths when the file names alone would be ambiguous.
fn display_names(paths: &[String]) -> Vec<String> {
    let names = paths.iter().map(|path| file_name(path)).collect::<Vec<_>>();
    let unique = names.iter().collect::<BTreeSet<_>>().len() == names.len();
    if unique {
        names.into_iter().map(str::to_string).collect()
    } else {
        paths.to_vec()
    }
}

fn project_kind(text: &str) -> ProjectKind {
    // MSBuild accepts single-quoted attributes too.
    let text = text.to_lowercase().replace('\'', "\"");
    let has_any = |markers: &[&str]| markers.iter().any(|marker| text.contains(marker));
    if has_any(&[
        "<istestproject>true<",
        "microsoft.net.test.sdk",
        "sdk=\"mstest.sdk",
    ]) {
        ProjectKind::Test
    } else if has_any(&[
        "<outputtype>exe<",
        "<outputtype>winexe<",
        "sdk=\"microsoft.net.sdk.web",
        "sdk=\"microsoft.net.sdk.worker",
        "sdk=\"microsoft.net.sdk.blazorwebassembly",
    ]) {
        ProjectKind::Executable
    } else {
        ProjectKind::Library
    }
}

fn launch_profiles(
    project_path: &str,
    files: &BTreeSet<String>,
    read_file: ReadFile,
) -> Vec<String> {
    let settings_path = join(directory_of(project_path), "Properties/launchSettings.json");
    if !files.contains(&settings_path) {
        return Vec::new();
    }
    let Some(settings) = read_file(&settings_path)
        .ok()
        .and_then(|text| parse_json(&text))
    else {
        return Vec::new();
    };
    settings["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, profile)| profile["commandName"] == "Project")
        .map(|(name, _)| name.clone())
        .collect()
}

/// The label names the first argument, which is the dotnet verb.
fn task(name: &str, args: &[&str], long_running: bool) -> Value {
    let verb = args.first().copied().unwrap_or_default();
    json!({
        "label": format!("dotnet {verb} {name}"),
        "command": "dotnet",
        "args": args,
        "use_new_terminal": long_running,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_files::test_support::{FakeWorktree, labels, summarize};

    fn tasks_for(files: &[(&str, &str)]) -> Vec<(String, Vec<String>)> {
        let worktree = FakeWorktree::new(files);
        summarize(generate_tasks(
            &|directory| worktree.list_directory(directory),
            &|path| worktree.read_text_file(path),
        ))
    }

    const EXE: &str = "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><OutputType>Exe</OutputType></PropertyGroup></Project>";
    const LIBRARY: &str = "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>";
    const TESTS: &str = "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>";
    const LAUNCH_SETTINGS: &str = r#"{ "profiles": {
        "Demo.App": { "commandName": "Project" },
        "IIS": { "commandName": "IISExpress" }
    } }"#;

    #[test]
    fn solution_with_executable_library_and_tests() {
        let tasks = tasks_for(&[
            ("Demo.sln", ""),
            ("src/Demo.App/Demo.App.csproj", EXE),
            (
                "src/Demo.App/Properties/launchSettings.json",
                LAUNCH_SETTINGS,
            ),
            ("src/Demo.Core/Demo.Core.csproj", LIBRARY),
            ("tests/Demo.Tests/Demo.Tests.csproj", TESTS),
        ]);
        assert_eq!(
            labels(&tasks),
            [
                "dotnet build Demo.sln",
                "dotnet restore Demo.sln",
                "dotnet clean Demo.sln",
                "dotnet test Demo.sln",
                "dotnet run Demo.App.csproj",
                "dotnet watch Demo.App.csproj",
                "dotnet run Demo.App.csproj [Demo.App]",
                "dotnet test Demo.Tests.csproj",
            ]
        );
        assert_eq!(
            tasks[6].1,
            [
                "run",
                "--project",
                "src/Demo.App/Demo.App.csproj",
                "--launch-profile",
                "Demo.App"
            ]
        );
    }

    #[test]
    fn projects_without_a_solution_get_their_own_build_tasks() {
        let tasks = tasks_for(&[("Tool.csproj", EXE)]);
        assert_eq!(
            labels(&tasks),
            [
                "dotnet build Tool.csproj",
                "dotnet restore Tool.csproj",
                "dotnet clean Tool.csproj",
                "dotnet run Tool.csproj",
                "dotnet watch Tool.csproj",
            ]
        );
    }

    #[test]
    fn ambiguous_names_use_full_paths() {
        let tasks = tasks_for(&[("a/App.sln", ""), ("b/App.sln", "")]);
        assert_eq!(tasks[0].0, "dotnet build a/App.sln");
        assert_eq!(tasks[3].0, "dotnet build b/App.sln");
    }

    #[test]
    fn nothing_without_dotnet_files() {
        assert!(tasks_for(&[("README.md", "")]).is_empty());
    }

    #[test]
    fn project_kind_covers_sdk_variants() {
        assert!(project_kind("<Project Sdk='Microsoft.NET.Sdk.Web'/>") == ProjectKind::Executable);
        assert!(
            project_kind("<Project Sdk=\"Microsoft.NET.Sdk.BlazorWebAssembly\"/>")
                == ProjectKind::Executable
        );
        assert!(project_kind("<Project Sdk=\"MSTest.Sdk/3.6.2\"/>") == ProjectKind::Test);
        assert!(project_kind("<Project Sdk=\"Microsoft.NET.Sdk\"/>") == ProjectKind::Library);
    }

    #[test]
    fn launch_settings_with_a_bom_still_yield_profiles() {
        let settings = format!("\u{feff}{LAUNCH_SETTINGS}");
        let tasks = tasks_for(&[
            ("App.csproj", EXE),
            ("Properties/launchSettings.json", &settings),
        ]);
        assert!(labels(&tasks).contains(&"dotnet run App.csproj [Demo.App]"));
    }
}
