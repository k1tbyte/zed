use anyhow::{Result, bail};
use async_trait::async_trait;
use dap::{DapLocator, DebugRequest, adapters::DebugAdapterName};
use gpui::{BackgroundExecutor, SharedString};
use serde_json::{Map, Value};
use task::{BuildTaskDefinition, DebugScenario, SpawnInTerminal, TaskTemplate};

/// The worker only accepts `coreclr`, `msclr`, `mono` and `dotnet` as the DAP `adapterID`, and
/// Zed sends the adapter's name there. The `languages` crate registers the adapter under it.
pub const ADAPTER_NAME: &str = "coreclr";

/// Turns `dotnet run` tasks into scenarios that build first. The adapter finds the built
/// program and the launch profile itself, so the scenario only names the project.
pub(crate) struct DotnetLocator;

#[async_trait]
impl DapLocator for DotnetLocator {
    fn name(&self) -> SharedString {
        SharedString::new_static("dotnet")
    }

    async fn create_scenario(
        &self,
        build_config: &TaskTemplate,
        resolved_label: &str,
        adapter: &DebugAdapterName,
    ) -> Option<DebugScenario> {
        if adapter.0.as_ref() != ADAPTER_NAME || build_config.command != "dotnet" {
            return None;
        }
        let (verb, arguments) = build_config.args.split_first()?;
        if verb != "run" {
            return None;
        }
        let (options, program_args) = match arguments.iter().position(|argument| argument == "--") {
            Some(separator) => (&arguments[..separator], &arguments[separator + 1..]),
            None => (arguments, &[][..]),
        };

        // `--project=X` is the same as `--project X`.
        let mut options = options
            .iter()
            .flat_map(|option| match option.split_once('=') {
                Some((name, value)) if name.starts_with('-') => vec![name, value],
                _ => vec![option.as_str()],
            });
        let (mut project, mut profile, mut no_profile) = (None, None, false);
        while let Some(option) = options.next() {
            match option {
                "--project" => project = options.next(),
                "--launch-profile" | "-lp" => profile = options.next(),
                "--no-launch-profile" => no_profile = true,
                _ => {}
            }
        }

        let mut config = Map::new();
        config.insert("request".into(), "launch".into());
        if let Some(project) = project {
            config.insert("project".into(), project.into());
        }
        if no_profile {
            config.insert("noLaunchProfile".into(), true.into());
        } else if let Some(profile) = profile {
            config.insert("launchProfile".into(), profile.into());
        }
        if let Some(cwd) = &build_config.cwd {
            config.insert("cwd".into(), cwd.clone().into());
        }
        if !build_config.env.is_empty() {
            config.insert("env".into(), serde_json::to_value(&build_config.env).ok()?);
        }
        if !program_args.is_empty() {
            config.insert("args".into(), program_args.to_vec().into());
        }

        let mut build = build_config.clone();
        build.label = format!("{} (build)", build_config.label);
        build.args = std::iter::once("build".to_owned())
            .chain(project.map(str::to_owned))
            .collect();
        build.use_new_terminal = false;

        Some(DebugScenario {
            adapter: adapter.0.clone(),
            label: resolved_label.to_string().into(),
            build: Some(BuildTaskDefinition::Template {
                task_template: build,
                locator_name: None,
            }),
            config: Value::Object(config),
            tcp_connection: None,
        })
    }

    async fn run(&self, _: SpawnInTerminal, _: BackgroundExecutor) -> Result<DebugRequest> {
        bail!("the dotnet locator does not run builds itself");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template(command: &str, args: &[&str]) -> TaskTemplate {
        TaskTemplate {
            label: "dotnet run App".into(),
            command: command.into(),
            args: args.iter().map(|argument| argument.to_string()).collect(),
            use_new_terminal: true,
            ..Default::default()
        }
    }

    fn scenario(template: &TaskTemplate, adapter: &str) -> Option<DebugScenario> {
        futures::executor::block_on(DotnetLocator.create_scenario(
            template,
            "dotnet run App",
            &DebugAdapterName(adapter.to_owned().into()),
        ))
    }

    #[test]
    fn a_run_task_becomes_a_scenario_that_builds_the_project_first() {
        let task = template(
            "dotnet",
            &[
                "run",
                "--project",
                "src/App.csproj",
                "--launch-profile",
                "https",
                "--",
                "--flag",
            ],
        );
        let scenario = scenario(&task, ADAPTER_NAME).unwrap();
        assert_eq!(
            scenario.config,
            json!({
                "request": "launch",
                "project": "src/App.csproj",
                "launchProfile": "https",
                "args": ["--flag"]
            })
        );
        let Some(BuildTaskDefinition::Template { task_template, .. }) = scenario.build else {
            panic!("expected a build task");
        };
        assert_eq!(task_template.args, ["build", "src/App.csproj"]);
        assert!(!task_template.use_new_terminal);
    }

    #[test]
    fn options_with_an_equals_sign_and_no_launch_profile_are_understood() {
        let task = template(
            "dotnet",
            &[
                "run",
                "--project=src/App.csproj",
                "--launch-profile=https",
                "--no-launch-profile",
            ],
        );
        let scenario = scenario(&task, ADAPTER_NAME).unwrap();
        assert_eq!(
            scenario.config,
            json!({
                "request": "launch",
                "project": "src/App.csproj",
                "noLaunchProfile": true
            })
        );
    }

    #[test]
    fn a_run_without_a_project_builds_the_one_in_the_directory() {
        let scenario = scenario(&template("dotnet", &["run"]), ADAPTER_NAME).unwrap();
        assert_eq!(scenario.config, json!({ "request": "launch" }));
        let Some(BuildTaskDefinition::Template { task_template, .. }) = scenario.build else {
            panic!("expected a build task");
        };
        assert_eq!(task_template.args, ["build"]);
    }

    #[test]
    fn other_tasks_and_adapters_are_left_alone() {
        assert!(scenario(&template("dotnet", &["build", "App.csproj"]), ADAPTER_NAME).is_none());
        assert!(scenario(&template("dotnet", &["test"]), ADAPTER_NAME).is_none());
        assert!(scenario(&template("cargo", &["run"]), ADAPTER_NAME).is_none());
        assert!(scenario(&template("dotnet", &["run"]), "JavaScript").is_none());
    }
}
