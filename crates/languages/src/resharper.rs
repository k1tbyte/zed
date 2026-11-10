use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use collections::HashMap;
use gpui::{App, AsyncApp};
use html_to_markdown::{
    HandleTag, HtmlElement, MarkdownWriter, StartTagOutcome, TagHandler, convert_html_to_markdown,
    markdown,
};
use http_client::{github::AssetKind, github_download::download_server_binary};
use language::{
    CodeLabel, CodeLabelExt as _, Language, LanguageName, LanguageRegistry, LspAdapter,
    LspAdapterDelegate, LspInstaller,
};
use lsp::{LanguageServerBinary, LanguageServerId, LanguageServerName};
use project::lsp_store::resharper_ext::RESHARPER_SERVER_NAME;
use settings::{
    AllLanguageSettingsContent, ConfiguredLanguageServer, RegisterSetting, Settings, SettingsStore,
};
use smol::{fs, stream::StreamExt as _};
use std::{
    cell::RefCell,
    env::consts,
    future::Future,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use util::{ResultExt as _, fs::remove_matching};

const UNRESOLVED_SYMBOL_PREFIX: &str = "Cannot resolve symbol";

#[derive(Clone, Debug, RegisterSetting)]
struct ResharperSettings {
    enabled: bool,
    download_url: Option<String>,
    sha256: Option<String>,
}

impl Settings for ResharperSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let resharper = content.extended.clone().unwrap().resharper.unwrap();
        Self {
            enabled: resharper.enabled.unwrap(),
            download_url: resharper.download_url,
            sha256: resharper.sha256,
        }
    }
}

/// Registers the opt-in adapter and re-applies the C# server choice when `enabled` flips.
pub fn init(languages: &Arc<LanguageRegistry>, cx: &mut App) {
    languages.register_available_lsp_adapter(RESHARPER_SERVER_NAME, Arc::new(ResharperLspAdapter));
    let languages = languages.clone();
    let mut enabled = false;
    let mut sync = move |cx: &mut App| {
        let now = ResharperSettings::get_global(cx).enabled;
        if std::mem::replace(&mut enabled, now) != now {
            crate::set_extension_settings(languages.language_settings(), cx);
        }
    };
    sync(cx);
    cx.observe_global::<SettingsStore>(sync).detach();
}

/// Goes into the layer below user settings, so an explicit `language_servers` still wins.
pub fn choose_csharp_server(all_languages: &mut AllLanguageSettingsContent, cx: &App) {
    if ResharperSettings::get_global(cx).enabled {
        all_languages
            .languages
            .0
            .entry("CSharp".into())
            .or_default()
            .language_servers = Some(vec![ConfiguredLanguageServer::new(
            RESHARPER_SERVER_NAME.0,
        )]);
    }
}

pub struct ResharperLspAdapter;

pub struct BackendArchive {
    url: String,
    sha256: Option<String>,
    /// Named after the archive, so a new URL installs next to nothing stale.
    directory: String,
}

fn platform() -> Result<&'static str> {
    Ok(match (consts::OS, consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("windows", "aarch64") => "windows-arm64",
        ("windows", "x86_64") => "windows-x64",
        (os, arch) => bail!("ReSharper backend is not available for {os}-{arch}"),
    })
}

fn server_binary(version_dir: &Path, platform: &str) -> LanguageServerBinary {
    LanguageServerBinary {
        path: version_dir
            .join(platform)
            .join(format!("JetBrains.VsCode.Backend{}", consts::EXE_SUFFIX)),
        arguments: vec![
            format!("--ParentPid={}", std::process::id()).into(),
            "--ClientName=zed".into(),
        ],
        env: None,
    }
}

/// Quick doc puts the declaring type on its own line and params/exceptions into header-less
/// tables, which markdown cannot express, so rows become list items.
struct QuickDocLayoutHandler;

impl HandleTag for QuickDocLayoutHandler {
    fn should_handle(&self, tag: &str) -> bool {
        matches!(tag, "div" | "tr")
    }

    fn handle_tag_start(
        &mut self,
        tag: &HtmlElement,
        writer: &mut MarkdownWriter,
    ) -> StartTagOutcome {
        if tag.tag() == "tr" {
            writer.push_str("\n- ");
        } else if tag.has_class("container") {
            writer.push_str("\n");
        }
        StartTagOutcome::Continue
    }
}

fn html_to_markdown(html: &str) -> Result<String> {
    let mut handlers: Vec<TagHandler> = vec![
        Rc::new(RefCell::new(QuickDocLayoutHandler)),
        Rc::new(RefCell::new(markdown::ParagraphHandler)),
        Rc::new(RefCell::new(markdown::HeadingHandler)),
        Rc::new(RefCell::new(markdown::ListHandler)),
        Rc::new(RefCell::new(markdown::StyledTextHandler)),
        Rc::new(RefCell::new(markdown::CodeHandler)),
    ];
    convert_html_to_markdown(html.as_bytes(), &mut handlers)
}

async fn installed_binary(version_dir: &Path) -> Option<LanguageServerBinary> {
    let binary = server_binary(version_dir, platform().ok()?);
    fs::metadata(&binary.path).await.ok()?;
    Some(binary)
}

impl LspInstaller for ResharperLspAdapter {
    type BinaryVersion = BackendArchive;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        cx: &mut AsyncApp,
    ) -> Result<BackendArchive> {
        let settings = cx.update(|cx| ResharperSettings::get_global(cx).clone());
        let url = settings
            .download_url
            .context("set `extended.resharper.download_url` to download the ReSharper backend")?
            .replace("{platform}", platform()?);
        let directory = Path::new(&url)
            .file_stem()
            .with_context(|| format!("no archive name in {url}"))?
            .to_string_lossy()
            .into_owned();
        Ok(BackendArchive {
            url,
            sha256: settings.sha256,
            directory,
        })
    }

    fn check_if_version_installed(
        &self,
        archive: &BackendArchive,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let version_dir = container_dir.join(&archive.directory);
        async move { installed_binary(&version_dir).await }
    }

    fn fetch_server_binary(
        &self,
        archive: BackendArchive,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();
        async move {
            let platform = platform()?;
            let version_dir = container_dir.join(&archive.directory);
            download_server_binary(
                &*delegate.http_client(),
                &archive.url,
                archive.sha256.as_deref(),
                &version_dir,
                AssetKind::Zip,
            )
            .await?;
            remove_matching(&container_dir, |entry| entry != version_dir).await;
            Ok(server_binary(&version_dir, platform))
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let mut entries = fs::read_dir(&container_dir).await.ok()?;
        while let Some(entry) = entries.next().await {
            if let Some(binary) = installed_binary(&entry.ok()?.path()).await {
                return Some(binary);
            }
        }
        None
    }
}

#[async_trait(?Send)]
impl LspAdapter for ResharperLspAdapter {
    fn name(&self) -> LanguageServerName {
        RESHARPER_SERVER_NAME
    }

    fn uses_socket_transport(&self) -> bool {
        true
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([(LanguageName::new_static("CSharp"), "csharp".to_string())])
    }

    /// The backend also reports errors that only follow from an unresolved reference on the same
    /// range (e.g. "Ambiguous invocation" for an unresolved argument), which Rider hides.
    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams, _: LanguageServerId) {
        let unresolved_ranges: Vec<lsp::Range> = params
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .message
                    .as_str()
                    .starts_with(UNRESOLVED_SYMBOL_PREFIX)
            })
            .map(|diagnostic| diagnostic.range)
            .collect();
        params.diagnostics.retain(|diagnostic| {
            diagnostic
                .message
                .as_str()
                .starts_with(UNRESOLVED_SYMBOL_PREFIX)
                || diagnostic.severity != Some(lsp::DiagnosticSeverity::ERROR)
                || !unresolved_ranges.contains(&diagnostic.range)
        });
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        use lsp::CompletionItemKind as Kind;
        let mut label = CodeLabel::fallback_for_completion(item, Some(language));
        // ReSharper filters on the server and sends an empty `filterText`, which would render
        // the whole label as a muted suffix.
        if item.filter_text.as_deref() == Some("") {
            label.filter_range = 0..item.label.len();
        }
        // The C# grammar captures properties only as `property.definition`.
        if label.runs.is_empty()
            && matches!(item.kind, Some(Kind::PROPERTY | Kind::FIELD))
            && let Some(highlight_id) = language
                .grammar()
                .and_then(|grammar| grammar.highlight_id_for_name("property.definition"))
        {
            label.runs.push((0..item.label.len(), highlight_id));
        }
        Some(label)
    }

    /// ReSharper sends quick docs as HTML inside markdown, which Zed would show verbatim.
    fn process_hover(&self, hover: &mut lsp::Hover) {
        if let lsp::HoverContents::Markup(content) = &mut hover.contents
            && let Some(markdown) = html_to_markdown(&content.value).log_err()
        {
            content.value = markdown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResharperLspAdapter, html_to_markdown};
    use gpui::{BorrowAppContext as _, TestAppContext};
    use language::{
        Language, LanguageConfig, LanguageName, LanguageRegistry, LspAdapter as _,
        language_settings::LanguageSettings,
    };
    use lsp::{
        CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity, LanguageServerId,
        Position, PublishDiagnosticsParams, Range, Uri,
    };
    use settings::{ConfiguredLanguageServer, SettingsStore};
    use std::sync::Arc;

    fn diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 18), Position::new(line, 25)),
            severity: Some(severity),
            message: message.to_string().into(),
            ..Diagnostic::default()
        }
    }

    #[test]
    fn test_unresolved_symbol_hides_errors_on_same_range() {
        let mut params = PublishDiagnosticsParams {
            uri: Uri::from_file_path("/root/Program.cs").expect("valid path"),
            diagnostics: vec![
                diagnostic(1, DiagnosticSeverity::ERROR, "Ambiguous invocation."),
                diagnostic(
                    1,
                    DiagnosticSeverity::ERROR,
                    "Cannot resolve symbol 'greting'",
                ),
                diagnostic(1, DiagnosticSeverity::WARNING, "Some warning"),
                diagnostic(2, DiagnosticSeverity::ERROR, "Ambiguous invocation."),
            ],
            version: None,
        };
        ResharperLspAdapter.process_diagnostics(&mut params, LanguageServerId(0));
        let messages: Vec<_> = params
            .diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.range.start.line, diagnostic.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            [
                (1, "Cannot resolve symbol 'greting'"),
                (1, "Some warning"),
                (2, "Ambiguous invocation."),
            ]
        );
    }

    #[gpui::test]
    async fn test_empty_filter_text_keeps_label_primary() {
        let language = Arc::new(Language::new(LanguageConfig::default(), None));
        let item = CompletionItem {
            label: "Length".to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            filter_text: Some(String::new()),
            label_details: Some(lsp::CompletionItemLabelDetails {
                detail: Some(" ".to_string()),
                description: Some("int".to_string()),
            }),
            ..CompletionItem::default()
        };
        let label = ResharperLspAdapter
            .label_for_completion(&item, &language)
            .await
            .expect("label");
        assert_eq!(label.text, "Length int");
        assert_eq!(label.filter_range, 0.."Length".len());
    }

    #[test]
    fn test_quick_doc_keeps_generic_arguments() {
        let html = "<div class='definition'><pre><div class='signature'>local variable <a title=\"System.Collections.Generic.List\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.Collections.Generic.List%25601%22%5D\">List</a>&lt;<a title=\"System.String\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.String%22%5D\">string</a>&gt; <b>names</b></div></pre></div>";
        assert_eq!(
            html_to_markdown(html).expect("valid html").trim(),
            "```\nlocal variable List<string> names\n```"
        );
    }

    #[test]
    fn test_quick_doc_html_to_markdown() {
        let html = "<div class='definition'><pre><div class='signature'>public static <a title=\"System.Void\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.Void%22%5D\">void</a> <b>WriteLine</b>(<a title=\"System.Boolean\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.Boolean%22%5D\">bool</a>&nbsp;value)</div><div class='container'> in class <a title=\"System.Console\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.Console%22%5D\">System.Console</a></div></pre></div><h5>Summary</h5><div>Writes the text representation of the specified Boolean value, followed by the current line terminator, to the standard output stream.</div><table><h5>Params</h5><div><table>\n<tr><td class=\"pdef\">value: </td><td>The value to write.</td></tr>\n</table>\n</div><h5>Exceptions</h5><div><table>\n<tr><td class=\"pdef\"><a title=\"IOException\" href=\"command:resharper.goToSymbol?%5B%22T%3ASystem.IO.IOException%22%5D\">IOException</a>: </td><td>An I/O error occurred.</td></tr>\n</table>\n</div></table>";
        let markdown = html_to_markdown(html).expect("valid html");
        assert_eq!(
            markdown.trim(),
            "```\npublic static void WriteLine(bool\u{a0}value)\n in class System.Console\n```\n\n\
             ##### Summary\n\nWrites the text representation of the specified Boolean value, followed by the current line terminator, to the standard output stream.\n\n\
             ##### Params\n\n- value: The value to write.\n\n\
             ##### Exceptions\n\n- IOException: An I/O error occurred."
        );
    }

    #[gpui::test]
    fn test_enabled_makes_resharper_the_only_csharp_server(cx: &mut TestAppContext) {
        let csharp = LanguageName::new_static("CSharp");
        let servers = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                LanguageSettings::resolve(None, Some(&csharp), cx)
                    .language_servers
                    .clone()
            })
        };
        let update = |cx: &mut TestAppContext, f: fn(&mut settings::SettingsContent)| {
            cx.update(|cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| store.update_user_settings(cx, f))
            });
            cx.run_until_parked();
        };
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            super::init(&languages, cx);
        });
        let default_servers = servers(cx);

        update(cx, |settings| {
            settings
                .extended
                .get_or_insert_default()
                .resharper
                .get_or_insert_default()
                .enabled = Some(true)
        });
        assert_eq!(servers(cx), [ConfiguredLanguageServer::new("resharper")]);

        update(cx, |settings| {
            settings
                .project
                .all_languages
                .languages
                .0
                .entry("CSharp".into())
                .or_default()
                .language_servers = Some(vec![ConfiguredLanguageServer::new("roslyn")])
        });
        assert_eq!(servers(cx), [ConfiguredLanguageServer::new("roslyn")]);

        update(cx, |settings| {
            settings.project.all_languages.languages.0.clear();
            settings
                .extended
                .get_or_insert_default()
                .resharper
                .get_or_insert_default()
                .enabled = Some(false)
        });
        assert_eq!(servers(cx), default_servers);
    }
}
