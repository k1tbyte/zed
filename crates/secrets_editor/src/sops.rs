use std::path::Path;

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use futures::AsyncWriteExt as _;
use serde_json::Value;
use util::command::{Stdio, new_command};

use crate::{environments::SecretsFormat, secret_tree::KeyPath};

pub(crate) enum Change {
    Set(KeyPath, Value),
    Unset(KeyPath),
}

pub(crate) async fn decrypt(
    environment: &HashMap<String, String>,
    path: &Path,
    format: SecretsFormat,
) -> Result<Value> {
    let path = path.to_string_lossy();
    let stdout = run(
        environment,
        &[
            "decrypt",
            "--input-type",
            format.sops_type(),
            "--output-type",
            "json",
            &path,
        ],
        None,
    )
    .await?;
    serde_json::from_slice(&stdout).context("parsing decrypted sops output")
}

pub(crate) async fn apply(
    environment: &HashMap<String, String>,
    path: &Path,
    format: SecretsFormat,
    changes: &[Change],
) -> Result<()> {
    let path = path.to_string_lossy();
    let sops_type = format.sops_type();
    for change in changes {
        let (Change::Set(key_path, _) | Change::Unset(key_path)) = change;
        let Some(sops_path) = key_path.sops_path() else {
            bail!("{key_path}: keys containing \", [ or ] cannot be edited with sops");
        };
        let (command, value) = match change {
            Change::Set(_, value) => ("set", Some(serde_json::to_vec(value)?)),
            Change::Unset(_) => ("unset", None),
        };
        let mut arguments = vec![
            command,
            "--input-type",
            sops_type,
            "--output-type",
            sops_type,
        ];
        if value.is_some() {
            arguments.push("--value-stdin");
        }
        arguments.extend([path.as_ref(), sops_path.as_str()]);
        run(environment, &arguments, value.as_deref()).await?;
    }
    Ok(())
}

async fn run(
    environment: &HashMap<String, String>,
    arguments: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut child = new_command("sops")
        .args(arguments)
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run sops, is it installed and on PATH?")?;

    if let Some(mut pipe) = child.stdin.take() {
        if let Some(input) = stdin {
            pipe.write_all(input).await.context("writing to sops")?;
            pipe.flush().await.context("writing to sops")?;
        }
    }

    let output = child.output().await.context("waiting for sops")?;
    if !output.status.success() {
        bail!(describe_failure(&output.stderr, output.status.code()));
    }
    Ok(output.stdout)
}

// sops may echo decrypted values in stderr (e.g. type errors), so it is never shown or logged.
fn describe_failure(stderr: &[u8], code: Option<i32>) -> String {
    const KNOWN: &[(&str, &str)] = &[
        (
            "Failed to get the data key",
            "None of your keys can decrypt this file",
        ),
        ("sops metadata not found", "Not a sops-encrypted file"),
        (
            "MAC mismatch",
            "MAC mismatch: the file was modified outside sops",
        ),
        ("Key not found", "The key no longer exists in the file"),
    ];
    let stderr = String::from_utf8_lossy(stderr);
    KNOWN
        .iter()
        .find(|(needle, _)| stderr.contains(needle))
        .map(|(_, message)| message.to_string())
        .unwrap_or_else(|| match code {
            Some(code) => format!("sops failed with exit code {code}"),
            None => "sops was terminated".to_string(),
        })
}
