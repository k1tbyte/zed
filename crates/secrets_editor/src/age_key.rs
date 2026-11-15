use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use age::secrecy::SecretString;
use anyhow::{Context as _, Result};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Global, SharedString, WeakEntity, Window,
    actions,
};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};
use zeroize::{Zeroize, Zeroizing};

use crate::SopsSettings;
use crate::passphrase_modal;

// One keychain entry holding a JSON map of key file path -> identity, so forgetting needs no paths.
const KEYCHAIN_URL: &str = "zed://sops-age-keys";
const KEYCHAIN_USERNAME: &str = "age";

actions!(
    secrets_editor,
    [
        /// Forgets the unlocked age keys and removes them from the keychain.
        ForgetAgeKey
    ]
);

#[derive(Default)]
struct AgeKeySession {
    /// Unlocked identities of passphrase-protected key files.
    identities: HashMap<PathBuf, Zeroizing<String>>,
    /// Serializes prompts: a second modal would dismiss the first one.
    prompt_lock: Arc<futures::lock::Mutex<()>>,
}

impl Global for AgeKeySession {}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(forget_age_key);
    })
    .detach();
}

fn forget_age_key(
    _workspace: &mut Workspace,
    _: &ForgetAgeKey,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.default_global::<AgeKeySession>().identities.clear();
    let provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |workspace, cx| {
        let message = match provider.delete_credentials(KEYCHAIN_URL, cx).await {
            Ok(()) => "Forgot the age keys",
            Err(error) => {
                log::error!("failed to delete the age keys from the keychain: {error:#}");
                "Forgot the age keys, but could not remove them from the keychain"
            }
        };
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<ForgetAgeKey>(), message).autohide(),
                    cx,
                );
            })
            .log_err();
    })
    .detach();
}

pub(crate) struct KeySource {
    pub recipient: String,
    pub origin: SharedString,
}

pub(crate) struct SopsKeys {
    pub environment: HashMap<String, String>,
    pub sources: Vec<KeySource>,
}

/// Loads every age identity Zed knows about and hands them to sops in `SOPS_AGE_KEY`.
pub(crate) async fn add_keys(
    workspace: WeakEntity<Workspace>,
    mut environment: HashMap<String, String>,
    extra_files: Vec<PathBuf>,
    cx: &mut AsyncWindowContext,
) -> Result<SopsKeys> {
    let default_file = default_key_file(&environment);
    let mut key_files = cx.update(|_, cx| SopsSettings::get_global(cx).age_key_files.clone())?;
    key_files.extend(extra_files);
    key_files.push(default_file.clone());
    let mut seen = collections::HashSet::default();
    key_files.retain(|path| seen.insert(path.clone()));

    // Setting SOPS_AGE_KEY replaces the user's own value, so keep it.
    let mut loaded: Vec<(SharedString, Zeroizing<String>)> = environment
        .get("SOPS_AGE_KEY")
        .map(|key| ("SOPS_AGE_KEY".into(), Zeroizing::new(key.clone())))
        .into_iter()
        .collect();
    let mut loaded_default_file = false;
    for key_file in key_files {
        if let Some(identity) = unlock_key_file(&workspace, &key_file, cx).await? {
            loaded_default_file |= key_file == default_file;
            loaded.push((key_file.display().to_string().into(), identity));
        }
    }
    // Its identities are passed below; sops would otherwise prompt for an encrypted original.
    if loaded_default_file {
        let empty_file = if cfg!(windows) { "NUL" } else { "/dev/null" };
        environment.insert("SOPS_AGE_KEY_FILE".to_string(), empty_file.to_string());
    }
    let sources = loaded
        .iter()
        .flat_map(|(origin, identity)| {
            public_keys(identity)
                .into_iter()
                .map(|recipient| KeySource {
                    recipient,
                    origin: origin.clone(),
                })
        })
        .collect();
    if !loaded.is_empty() {
        let joined: Vec<&str> = loaded
            .iter()
            .map(|(_, identity)| identity.as_str())
            .collect();
        environment.insert("SOPS_AGE_KEY".to_string(), joined.join("\n"));
    }
    Ok(SopsKeys {
        environment,
        sources,
    })
}

fn public_keys(identity_file: &str) -> Vec<String> {
    identity_file
        .lines()
        .filter_map(|line| line.trim().parse::<age::x25519::Identity>().ok())
        .map(|identity| identity.to_public().to_string())
        .collect()
}

fn cached_identity(
    key_file: &Path,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Zeroizing<String>>> {
    cx.update(|_, cx| {
        cx.try_global::<AgeKeySession>()
            .and_then(|session| session.identities.get(key_file).cloned())
    })
}

async fn unlock_key_file(
    workspace: &WeakEntity<Workspace>,
    key_file: &Path,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Zeroizing<String>>> {
    if let Some(identity) = cached_identity(key_file, cx)? {
        return Ok(Some(identity));
    }

    let content = cx
        .background_spawn({
            let key_file = key_file.to_path_buf();
            async move { read_key_file(&key_file) }
        })
        .await?;
    let Some(content) = content else {
        return Ok(None);
    };
    if !is_age_encrypted(&content) {
        let text = std::str::from_utf8(&content)
            .with_context(|| format!("{} is not valid UTF-8", key_file.display()))?;
        return Ok(Some(Zeroizing::new(text.to_owned())));
    }

    let prompt_lock =
        cx.update(|_, cx| cx.default_global::<AgeKeySession>().prompt_lock.clone())?;
    let _prompt_guard = prompt_lock.lock().await;
    if let Some(identity) = cached_identity(key_file, cx)? {
        return Ok(Some(identity));
    }

    let provider = cx.update(|_, cx| zed_credentials_provider::global(cx))?;
    let path_key = key_file.to_string_lossy().into_owned();
    let mut remembered = read_remembered(provider.as_ref(), cx).await;
    if let Some(identity) = remembered.remove(&path_key) {
        let identity = Zeroizing::new(identity);
        cache_identity(key_file, &identity, cx)?;
        return Ok(Some(identity));
    }

    let mut error: Option<String> = None;
    loop {
        let receiver = workspace.update_in(cx, |workspace, window, cx| {
            passphrase_modal::prompt(
                workspace,
                key_file,
                error.take().map(Into::into),
                window,
                cx,
            )
        })?;
        let Some(response) = receiver.await.ok().flatten() else {
            return Ok(None);
        };

        let decrypted = cx
            .background_spawn({
                let encrypted = content.clone();
                async move { decrypt_identity(&encrypted, &response.passphrase) }
            })
            .await;
        match decrypted {
            Ok(identity) => {
                cache_identity(key_file, &identity, cx)?;
                if response.remember {
                    let mut remembered = read_remembered(provider.as_ref(), cx).await;
                    remembered.insert(path_key, identity.to_string());
                    let json = Zeroizing::new(serde_json::to_vec(&remembered)?);
                    provider
                        .write_credentials(KEYCHAIN_URL, KEYCHAIN_USERNAME, &json, cx)
                        .await
                        .log_err();
                    remembered.values_mut().for_each(Zeroize::zeroize);
                }
                return Ok(Some(identity));
            }
            Err(decrypt_error) if is_wrong_passphrase(&decrypt_error) => {
                error = Some("Wrong passphrase".to_string());
            }
            Err(decrypt_error) => error = Some(format!("{decrypt_error:#}")),
        }
    }
}

async fn read_remembered(
    provider: &dyn CredentialsProvider,
    cx: &AsyncWindowContext,
) -> HashMap<String, String> {
    match provider.read_credentials(KEYCHAIN_URL, cx).await {
        Ok(Some((_, bytes))) => {
            let bytes = Zeroizing::new(bytes);
            serde_json::from_slice(&bytes)
                .context("parsing remembered age keys")
                .log_err()
                .unwrap_or_default()
        }
        Ok(None) => HashMap::default(),
        Err(error) => {
            log::error!("failed to read the age keys from the keychain: {error:#}");
            HashMap::default()
        }
    }
}

fn cache_identity(
    key_file: &Path,
    identity: &Zeroizing<String>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    cx.update(|_, cx| {
        cx.default_global::<AgeKeySession>()
            .identities
            .insert(key_file.to_path_buf(), identity.clone());
    })
}

fn default_key_file(environment: &HashMap<String, String>) -> PathBuf {
    if let Some(path) = environment.get("SOPS_AGE_KEY_FILE") {
        return PathBuf::from(path);
    }
    if let Some(config_home) = environment.get("XDG_CONFIG_HOME") {
        return Path::new(config_home).join("sops/age/keys.txt");
    }
    let home = paths::home_dir();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/sops/age/keys.txt")
    } else if cfg!(target_os = "windows") {
        environment
            .get("AppData")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("sops/age/keys.txt")
    } else {
        home.join(".config/sops/age/keys.txt")
    }
}

fn read_key_file(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
    match std::fs::read(path) {
        Ok(content) => Ok(Some(Zeroizing::new(content))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn is_age_encrypted(content: &[u8]) -> bool {
    content.starts_with(b"age-encryption.org/v1")
        || content.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----")
}

fn decrypt_identity(ciphertext: &[u8], passphrase: &str) -> Result<Zeroizing<String>> {
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
    let decryptor = age::Decryptor::new_buffered(age::armor::ArmoredReader::new(ciphertext))?;
    anyhow::ensure!(
        decryptor.is_scrypt(),
        "The identity file is not passphrase encrypted"
    );
    let mut reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
    let mut plaintext = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plaintext)?;
    let identity_text = Zeroizing::new(std::str::from_utf8(&plaintext)?.to_owned());

    let identities = age::IdentityFile::from_buffer(identity_text.as_bytes())?.into_identities()?;
    anyhow::ensure!(
        !identities.is_empty(),
        "The decrypted file contains no age identity"
    );
    Ok(identity_text)
}

fn is_wrong_passphrase(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<age::DecryptError>(),
        Some(
            age::DecryptError::DecryptionFailed
                | age::DecryptError::KeyDecryptionFailed
                | age::DecryptError::NoMatchingKeys
        )
    )
}

#[cfg(test)]
mod tests {
    use age::secrecy::ExposeSecret as _;

    use super::*;

    fn identity_text() -> String {
        let identity = age::x25519::Identity::generate();
        format!(
            "# created: test\n{}\n",
            identity.to_string().expose_secret()
        )
    }

    fn scrypt_recipient(passphrase: &str) -> age::scrypt::Recipient {
        let mut recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_owned()));
        recipient.set_work_factor(2);
        recipient
    }

    #[test]
    fn decrypts_binary_and_armored_identity_files() -> Result<()> {
        let text = identity_text();
        let recipient = scrypt_recipient("correct horse");

        let binary = age::encrypt(&recipient, text.as_bytes())?;
        assert!(is_age_encrypted(&binary));
        assert_eq!(*decrypt_identity(&binary, "correct horse")?, text);

        let armored = age::encrypt_and_armor(&recipient, text.as_bytes())?;
        assert!(is_age_encrypted(armored.as_bytes()));
        assert_eq!(
            *decrypt_identity(armored.as_bytes(), "correct horse")?,
            text
        );
        Ok(())
    }

    #[test]
    fn public_keys_skip_comments() {
        let identity = age::x25519::Identity::generate();
        let text = format!(
            "# created: test\n# public key: x\n{}\n",
            identity.to_string().expose_secret()
        );
        assert_eq!(public_keys(&text), [identity.to_public().to_string()]);
    }

    #[test]
    fn wrong_passphrase_is_reported() -> Result<()> {
        let armored =
            age::encrypt_and_armor(&scrypt_recipient("right"), identity_text().as_bytes())?;
        let error = decrypt_identity(armored.as_bytes(), "wrong")
            .err()
            .context("decryption should fail")?;
        assert!(is_wrong_passphrase(&error), "{error:#}");
        Ok(())
    }
}
