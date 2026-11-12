//! .NET debug adapter on top of the debugger worker that ships in the ReSharper backend.
//! Nothing is downloaded or copied here: the worker is the one the language server installed.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use collections::HashMap;
use dap::{
    DapRegistry, StartDebuggingRequestArguments,
    adapters::{
        DapDelegate, DebugAdapter, DebugAdapterBinary, DebugAdapterName, DebugTaskDefinition,
        TcpArguments,
    },
    configure_tcp_connection,
};
use gpui::{App, AsyncApp, BorrowAppContext, SharedString};
use language::LanguageName;
use project::{
    debugger::locators::dotnet::ADAPTER_NAME, lsp_store::resharper_ext::RESHARPER_SERVER_NAME,
    project_settings::ProjectSettings,
};
use serde_json::{Map, Value, json};
use settings::{Settings as _, SettingsLocation};
use task::{DebugRequest, DebugScenario, ZedDebugConfig};
use util::{rel_path::RelPath, shell::ShellKind};

use crate::resharper::{
    DEBUGGER_WORKER, backend_container_dir, executable_name, installed_backend_dir,
};

pub fn register(cx: &mut App) {
    cx.update_default_global(|registry: &mut DapRegistry, _| {
        registry.add_adapter(Arc::new(CoreClrDebugAdapter));
    });
}

struct CoreClrDebugAdapter;

#[async_trait(?Send)]
impl DebugAdapter for CoreClrDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(SharedString::new_static(ADAPTER_NAME))
    }

    fn adapter_language_name(&self) -> Option<LanguageName> {
        Some(SharedString::new_static("CSharp").into())
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let mut config = match &zed_scenario.request {
            DebugRequest::Attach(attach) => {
                json!({ "request": "attach", "processId": attach.process_id })
            }
            DebugRequest::Launch(launch) => {
                let mut config = json!({ "request": "launch", "program": launch.program });
                if !launch.args.is_empty() {
                    config["args"] = launch.args.clone().into();
                }
                if !launch.env.is_empty() {
                    config["env"] = launch.env_json();
                }
                if let Some(cwd) = &launch.cwd {
                    config["cwd"] = cwd.to_string_lossy().into_owned().into();
                }
                config
            }
        };
        if let Some(stop_at_entry) = zed_scenario.stop_on_entry {
            config["stopAtEntry"] = stop_at_entry.into();
        }
        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config,
            tcp_connection: None,
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        task_definition: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        cx: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        let worker = match user_installed_path {
            Some(path) => path,
            None => backend_worker(delegate, cx).await?,
        };
        // Over stdio the debuggee's output would mix into the DAP stream.
        let (host, port, timeout) =
            configure_tcp_connection(task_definition.tcp_connection.clone().unwrap_or_default())
                .await?;
        let configuration = resolve_configuration(delegate, task_definition.config.clone()).await?;
        Ok(DebugAdapterBinary {
            command: Some(worker.to_string_lossy().into_owned()),
            arguments: user_args.unwrap_or_else(|| vec![format!("--port={port}")]),
            envs: user_env.unwrap_or_default(),
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            connection: Some(TcpArguments {
                host,
                port,
                timeout,
            }),
            request_args: StartDebuggingRequestArguments {
                configuration,
                request: self.request_kind(&task_definition.config).await?,
            },
        })
    }

    fn dap_schema(&self) -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "required": ["request"],
                    "properties": {
                        "request": { "type": "string", "enum": ["launch"] },
                        "program": {
                            "type": "string",
                            "description": "Path to the built .dll. Leave it out to use `project`."
                        },
                        "project": {
                            "type": "string",
                            "description": "Path to a .csproj, relative to `cwd` or the worktree. Defaults to the project in `cwd`."
                        },
                        "noLaunchProfile": {
                            "type": "boolean",
                            "description": "Ignore Properties/launchSettings.json, like `dotnet run --no-launch-profile`."
                        },
                        "launchProfile": {
                            "type": "string",
                            "description": "Profile of Properties/launchSettings.json for args, environment and URLs. Defaults to the first `Project` profile."
                        },
                        "cwd": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "env": { "type": "object", "additionalProperties": { "type": "string" } },
                        "stopAtEntry": { "type": "boolean" }
                    }
                },
                {
                    "type": "object",
                    "required": ["request"],
                    "properties": {
                        "request": { "type": "string", "enum": ["attach"] },
                        "processId": { "type": ["integer", "string"] }
                    }
                }
            ]
        })
    }
}

/// The worker sits next to the backend, so a hand-set `lsp.resharper.binary.path` leads to it too.
async fn backend_worker(delegate: &Arc<dyn DapDelegate>, cx: &AsyncApp) -> Result<PathBuf> {
    let user_backend = cx.update(|cx| {
        let location = SettingsLocation {
            worktree_id: delegate.worktree_id(),
            path: RelPath::empty(),
        };
        let lsp = &ProjectSettings::get(Some(location), cx).lsp;
        lsp.get(&RESHARPER_SERVER_NAME)?
            .binary
            .as_ref()?
            .path
            .clone()
    });
    let backend_dir = match user_backend {
        Some(path) => delegate
            .worktree_root_path()
            .join(path)
            .parent()
            .map(Path::to_path_buf)
            .context("`lsp.resharper.binary.path` has no directory")?,
        None => installed_backend_dir(&backend_container_dir())
            .await
            .context(
                "the ReSharper backend is not installed: enable `resharper` for C# and open a C# file so it downloads",
            )?,
    };
    let worker = backend_dir.join(executable_name(DEBUGGER_WORKER));
    smol::fs::metadata(&worker)
        .await
        .with_context(|| format!("no debugger worker at {}", worker.display()))?;
    Ok(worker)
}

/// Turns `project` and `launchProfile` into what the worker understands: `program`, `args`,
/// `env` and `cwd`. Values set in the scenario win over the profile.
async fn resolve_configuration(
    delegate: &Arc<dyn DapDelegate>,
    mut config: Value,
) -> Result<Value> {
    let object = config
        .as_object_mut()
        .context("the debug configuration must be an object")?;
    let project = take_string(object, "project");
    let profile_name = take_string(object, "launchProfile");
    let no_profile = object.remove("noLaunchProfile") == Some(Value::Bool(true));
    object.remove("request");
    if object.contains_key("program") {
        return Ok(config);
    }

    let root = delegate.worktree_root_path();
    let base = match object.get("cwd").and_then(Value::as_str) {
        Some(cwd) => root.join(cwd),
        None => root.to_path_buf(),
    };
    let project_file = project.map(|project| base.join(project));
    let project_dir = project_file
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(&base)
        .to_path_buf();

    let program = target_path(delegate, project_file.as_deref(), &base).await?;
    object.insert("program".into(), program.into());
    let settings = if no_profile {
        None
    } else {
        match smol::fs::read_to_string(project_dir.join("Properties/launchSettings.json")).await {
            Ok(text) => Some(parse_launch_settings(&text)?),
            Err(_) => None,
        }
    };
    apply_launch_settings(
        object,
        settings.as_ref(),
        profile_name.as_deref(),
        &project_dir,
    )?;
    Ok(config)
}

fn take_string(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    match object.remove(key)? {
        Value::String(text) => Some(text),
        _ => None,
    }
}

async fn target_path(
    delegate: &Arc<dyn DapDelegate>,
    project: Option<&Path>,
    base: &Path,
) -> Result<String> {
    let dotnet = delegate
        .which(OsStr::new("dotnet"))
        .await
        .context("`dotnet` was not found, install the .NET SDK")?;
    let mut command = util::command::new_command(dotnet);
    command.arg("msbuild");
    if let Some(project) = project {
        command.arg(project);
    }
    let output = command
        .arg("-getProperty:TargetPath")
        .current_dir(base)
        .envs(delegate.shell_env().await)
        .output()
        .await
        .context("failed to run `dotnet msbuild`")?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || path.is_empty() {
        bail!(
            "`dotnet msbuild` found no output path (projects with several target frameworks need `program`): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(path)
}

/// Visual Studio writes launchSettings.json with a BOM, and people add comments.
fn parse_launch_settings(text: &str) -> Result<Value> {
    serde_json_lenient::from_str(text.trim_start_matches('\u{feff}'))
        .context("invalid launchSettings.json")
}

fn launch_profile<'a>(settings: &'a Value, name: Option<&str>) -> Option<&'a Value> {
    let profiles = settings["profiles"].as_object()?;
    match name {
        Some(name) => profiles.get(name),
        None => profiles
            .values()
            .find(|profile| profile["commandName"] == "Project"),
    }
}

fn apply_launch_settings(
    config: &mut Map<String, Value>,
    settings: Option<&Value>,
    profile_name: Option<&str>,
    project_dir: &Path,
) -> Result<()> {
    let profile = settings.and_then(|settings| launch_profile(settings, profile_name));
    if let (Some(name), None) = (profile_name, profile) {
        bail!("no launch profile `{name}` in Properties/launchSettings.json");
    }

    if let Some(profile) = profile {
        if !config.contains_key("args")
            && let Some(args) = profile["commandLineArgs"]
                .as_str()
                .and_then(|args| ShellKind::Posix.split(args))
        {
            config.insert("args".into(), args.into());
        }

        let mut env = Map::new();
        if let Some(urls) = profile["applicationUrl"].as_str() {
            env.insert("ASPNETCORE_URLS".into(), urls.into());
        }
        for (name, value) in profile["environmentVariables"]
            .as_object()
            .into_iter()
            .flatten()
        {
            if value.is_string() {
                env.insert(name.clone(), value.clone());
            }
        }
        env.extend(
            config
                .get("env")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        );
        if !env.is_empty() {
            config.insert("env".into(), env.into());
        }

        if !config.contains_key("cwd")
            && let Some(directory) = profile["workingDirectory"].as_str()
        {
            config.insert(
                "cwd".into(),
                project_dir
                    .join(directory)
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
        }
    }

    // `dotnet run` starts in the project directory, which is where appsettings.json is found.
    config
        .entry("cwd")
        .or_insert_with(|| project_dir.to_string_lossy().into_owned().into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resharper::{BACKEND, platform};

    const SETTINGS: &str = r#"{
        "profiles": {
            "iis": { "commandName": "IISExpress" },
            "http": {
                "commandName": "Project",
                "commandLineArgs": "--verbose \"two words\"",
                "applicationUrl": "http://localhost:5000",
                "environmentVariables": { "ASPNETCORE_ENVIRONMENT": "Development", "NUMBER": 1 }
            },
            "https": { "commandName": "Project", "workingDirectory": "bin" }
        }
    }"#;

    fn settings() -> Value {
        serde_json::from_str(SETTINGS).unwrap()
    }

    fn apply(config: Value, profile: Option<&str>) -> Result<Value> {
        let mut config = config;
        apply_launch_settings(
            config.as_object_mut().unwrap(),
            Some(&settings()),
            profile,
            Path::new("/app/src"),
        )?;
        Ok(config)
    }

    #[test]
    fn launch_settings_with_a_bom_comments_and_trailing_commas_are_read() {
        let text = "\u{feff}{ // local\n \"profiles\": { \"http\": { \"commandName\": \"Project\", }, }, }";
        let settings = parse_launch_settings(text).unwrap();
        assert!(launch_profile(&settings, None).is_some());
    }

    #[test]
    fn the_first_project_profile_is_the_default() {
        let config = apply(json!({}), None).unwrap();
        assert_eq!(config["args"], json!(["--verbose", "two words"]));
        assert_eq!(
            config["env"],
            json!({
                "ASPNETCORE_URLS": "http://localhost:5000",
                "ASPNETCORE_ENVIRONMENT": "Development"
            })
        );
        assert_eq!(config["cwd"], "/app/src");
    }

    #[test]
    fn a_named_profile_can_set_the_working_directory() {
        let config = apply(json!({}), Some("https")).unwrap();
        assert_eq!(config["cwd"], "/app/src/bin");
        assert!(config.get("env").is_none());
    }

    #[test]
    fn values_of_the_scenario_win_over_the_profile() {
        let config = apply(
            json!({ "args": ["x"], "cwd": "/elsewhere", "env": { "ASPNETCORE_ENVIRONMENT": "Production" } }),
            Some("http"),
        )
        .unwrap();
        assert_eq!(config["args"], json!(["x"]));
        assert_eq!(config["cwd"], "/elsewhere");
        assert_eq!(config["env"]["ASPNETCORE_ENVIRONMENT"], "Production");
        assert_eq!(config["env"]["ASPNETCORE_URLS"], "http://localhost:5000");
    }

    #[test]
    fn an_unknown_profile_is_an_error() {
        let error = apply(json!({}), Some("nope")).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }

    #[test]
    fn without_launch_settings_only_the_directory_is_set() {
        let mut config = Map::new();
        apply_launch_settings(&mut config, None, None, Path::new("/app")).unwrap();
        assert_eq!(Value::Object(config), json!({ "cwd": "/app" }));
    }

    #[test]
    fn the_backend_is_found_in_the_platform_directory_of_an_archive() {
        let container = tempfile::tempdir().unwrap();
        smol::block_on(async {
            assert_eq!(installed_backend_dir(container.path()).await, None);

            let platform_dir = container
                .path()
                .join("JetBrains.VsCode.Backend.1")
                .join(platform().unwrap());
            std::fs::create_dir_all(&platform_dir).unwrap();
            std::fs::write(platform_dir.join(executable_name(BACKEND)), "").unwrap();
            assert_eq!(
                installed_backend_dir(container.path()).await,
                Some(platform_dir)
            );
        });
    }
}
