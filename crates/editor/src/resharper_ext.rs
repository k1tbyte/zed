use std::future::Future;

use gpui::{Context, Window, actions};
use project::lsp_store::resharper_ext;
use workspace::{Workspace, notifications::DetachAndPromptErr as _};

actions!(
    resharper,
    [
        /// Signs in to a JetBrains Account to license ReSharper.
        SignIn,
        /// Signs out of the JetBrains Account that licenses ReSharper.
        SignOut,
    ]
);

pub(crate) fn register_actions(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &SignIn, window, cx| {
        let sign_in = resharper_ext::sign_in(&workspace.project().read(cx).lsp_store(), cx);
        prompt_on_error(sign_in, "ReSharper sign-in failed", window, cx);
    });
    workspace.register_action(|workspace, _: &SignOut, window, cx| {
        let sign_out = resharper_ext::sign_out(&workspace.project().read(cx).lsp_store(), cx);
        prompt_on_error(sign_out, "ReSharper sign-out failed", window, cx);
    });
}

fn prompt_on_error(
    task: impl Future<Output = anyhow::Result<()>> + 'static,
    message: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn(async move |_, _| task.await)
        .detach_and_prompt_err(message, window, cx, |_, _, _| None);
}
