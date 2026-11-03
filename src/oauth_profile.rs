//! Secure, named OAuth credentials for mcp-repl.
//!
//! The TOML config contains only routing and registration metadata. Access
//! tokens, refresh tokens, and DCR client secrets are serialized into one
//! profile record in the operating-system credential store.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use toml_edit::{Array, DocumentMut, Item, Table, value};
use tower_mcp::client::{
    OAuthAuthorizationAction, OAuthAuthorizationFlow, OAuthAuthorizationHandler,
    OAuthAuthorizationRequest, OAuthClientError, OAuthClientRegistration,
    OAuthClientRegistrationOptions, OAuthClientRegistrationStore, OAuthDynamicClientRegistration,
    OAuthRedirectPolicy, OAuthStoredToken, OAuthTokenBinding, OAuthTokenStore,
};

use crate::config::OAuthProfile;

const KEYRING_SERVICE: &str = "mcp-repl/oauth";
// Credential Manager stores passwords as UTF-16 in a 2,560-byte blob. Base64
// of 768 raw bytes is 1,024 ASCII characters / 2,048 UTF-16 bytes, leaving
// room below that platform limit. The other supported stores allow more.
const KEYRING_CHUNK_BYTES: usize = 768;
const MAX_KEYRING_CHUNKS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyringManifest {
    generation: String,
    chunks: usize,
}

impl KeyringManifest {
    fn parse(encoded: &str) -> Result<Self, String> {
        let mut parts = encoded.split(':');
        let (Some("v1"), Some(generation), Some(chunks), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("stored OAuth credential manifest is invalid".to_string());
        };
        if generation.is_empty()
            || !generation
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err("stored OAuth credential manifest has an invalid generation".to_string());
        }
        let chunks = chunks
            .parse::<usize>()
            .map_err(|_| "stored OAuth credential manifest has an invalid chunk count")?;
        if chunks == 0 || chunks > MAX_KEYRING_CHUNKS {
            return Err("stored OAuth credential manifest has an invalid chunk count".to_string());
        }
        Ok(Self {
            generation: generation.to_string(),
            chunks,
        })
    }

    fn encode(&self) -> String {
        format!("v1:{}:{}", self.generation, self.chunks)
    }
}

fn new_generation() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("cannot generate credential-store record id: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn chunk_account(profile: &str, manifest: &KeyringManifest, index: usize) -> String {
    format!("{profile}:{}:{index}", manifest.generation)
}

fn encode_chunks(secret: &str) -> Vec<String> {
    secret
        .as_bytes()
        .chunks(KEYRING_CHUNK_BYTES)
        .map(|chunk| base64::engine::general_purpose::STANDARD.encode(chunk))
        .collect()
}

fn decode_chunks(chunks: impl IntoIterator<Item = String>) -> Result<String, String> {
    let mut decoded = Vec::new();
    for chunk in chunks {
        decoded.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .map_err(|_| "stored OAuth credential chunk is invalid")?,
        );
    }
    String::from_utf8(decoded)
        .map_err(|_| "stored OAuth credential record is not UTF-8".to_string())
}

// Use a private store rather than keyring-core's process-wide default. This
// keeps library callers and isolated tests from replacing each other's store.
fn platform_store() -> keyring_core::Result<Arc<keyring_core::CredentialStore>> {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new()?;
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new()?;
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    let store = dbus_secret_service_keyring_store::Store::new()?;
    #[cfg(all(any(unix, windows), not(any(target_os = "ios", target_os = "android"))))]
    {
        Ok(store)
    }
    #[cfg(not(all(any(unix, windows), not(any(target_os = "ios", target_os = "android")))))]
    Err(keyring_core::Error::Invalid(
        "platform".to_string(),
        "must be macOS, Windows, or a non-iOS, non-Android *nix variant".to_string(),
    ))
}

fn keyring_entry(
    store: &keyring_core::CredentialStore,
    account: &str,
) -> Result<keyring_core::Entry, String> {
    // No modifiers: preserve the v1 backend's service/account identity and
    // default collection, including discovery of existing Secret Service items.
    store
        .build(KEYRING_SERVICE, account, None)
        .map_err(|error| error.to_string())
}

fn delete_keyring_entry(
    store: &keyring_core::CredentialStore,
    account: &str,
) -> Result<(), String> {
    match keyring_entry(store, account)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn read_keyring_manifest(
    store: &keyring_core::CredentialStore,
    profile: &str,
) -> Result<Option<KeyringManifest>, String> {
    match keyring_entry(store, profile)?.get_password() {
        Ok(encoded) => KeyringManifest::parse(&encoded).map(Some),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn delete_keyring_generation(
    store: &keyring_core::CredentialStore,
    profile: &str,
    manifest: &KeyringManifest,
) -> Result<(), String> {
    let mut first_error = None;
    for index in 0..manifest.chunks {
        if let Err(error) = delete_keyring_entry(store, &chunk_account(profile, manifest, index))
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[async_trait]
trait SecretBackend: Send + Sync {
    async fn load(&self, profile: &str) -> Result<Option<String>, String>;
    async fn save(&self, profile: &str, secret: &str) -> Result<(), String>;
    async fn remove(&self, profile: &str) -> Result<(), String>;
}

#[derive(Debug)]
struct KeyringSecretBackend {
    store: Arc<keyring_core::CredentialStore>,
}

impl KeyringSecretBackend {
    fn from_store(
        store: keyring_core::Result<Arc<keyring_core::CredentialStore>>,
    ) -> Result<Self, String> {
        store.map(|store| Self { store }).map_err(|error| {
            format!(
                "the operating-system credential store is unavailable: {error}. \
                 mcp-repl will not fall back to plaintext; use MCP_BEARER for a headless \
                 environment or configure the platform credential service"
            )
        })
    }
}

#[async_trait]
impl SecretBackend for KeyringSecretBackend {
    async fn load(&self, profile: &str) -> Result<Option<String>, String> {
        let profile = profile.to_string();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let Some(manifest) = read_keyring_manifest(store.as_ref(), &profile)? else {
                return Ok(None);
            };
            let mut chunks = Vec::with_capacity(manifest.chunks);
            for index in 0..manifest.chunks {
                let account = chunk_account(&profile, &manifest, index);
                let chunk = keyring_entry(store.as_ref(), &account)?
                    .get_password()
                    .map_err(|error| {
                        if matches!(error, keyring_core::Error::NoEntry) {
                            "stored OAuth credential record is incomplete".to_string()
                        } else {
                            error.to_string()
                        }
                    })?;
                chunks.push(chunk);
            }
            decode_chunks(chunks).map(Some)
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }

    async fn save(&self, profile: &str, secret: &str) -> Result<(), String> {
        let profile = profile.to_string();
        let secret = secret.to_string();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let previous = read_keyring_manifest(store.as_ref(), &profile)?;
            let encoded_chunks = encode_chunks(&secret);
            if encoded_chunks.is_empty() || encoded_chunks.len() > MAX_KEYRING_CHUNKS {
                return Err("OAuth credential record is too large for the secure store".to_string());
            }
            let manifest = KeyringManifest {
                generation: new_generation()?,
                chunks: encoded_chunks.len(),
            };
            for (index, chunk) in encoded_chunks.iter().enumerate() {
                let account = chunk_account(&profile, &manifest, index);
                if let Err(error) = keyring_entry(store.as_ref(), &account)
                    .and_then(|entry| entry.set_password(chunk).map_err(|error| error.to_string()))
                {
                    for cleanup in 0..index {
                        let _ = delete_keyring_entry(
                            store.as_ref(),
                            &chunk_account(&profile, &manifest, cleanup),
                        );
                    }
                    return Err(error);
                }
            }
            if let Err(error) = keyring_entry(store.as_ref(), &profile).and_then(|entry| {
                entry
                    .set_password(&manifest.encode())
                    .map_err(|error| error.to_string())
            }) {
                let _ = delete_keyring_generation(store.as_ref(), &profile, &manifest);
                return Err(error);
            }
            if let Some(previous) = previous {
                delete_keyring_generation(store.as_ref(), &profile, &previous)?;
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }

    async fn remove(&self, profile: &str) -> Result<(), String> {
        let profile = profile.to_string();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(manifest) = read_keyring_manifest(store.as_ref(), &profile)? {
                delete_keyring_generation(store.as_ref(), &profile, &manifest)?;
            }
            delete_keyring_entry(store.as_ref(), &profile)
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredToken {
    binding: OAuthTokenBinding,
    token: OAuthStoredToken,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredRegistration {
    issuer: String,
    registration: OAuthClientRegistration,
}

#[derive(Default, Serialize, Deserialize)]
struct Secrets {
    #[serde(default)]
    tokens: Vec<StoredToken>,
    #[serde(default)]
    registrations: Vec<StoredRegistration>,
}

/// Both persistence traits share one encrypted/keychain record and one lock,
/// so token refresh and DCR updates cannot overwrite one another in-process.
#[derive(Clone)]
pub struct CredentialStore {
    profile: Arc<str>,
    backend: Arc<dyn SecretBackend>,
    lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    pub fn keyring(profile: &str) -> Result<Self, String> {
        validate_name(profile)?;
        let backend = KeyringSecretBackend::from_store(platform_store())?;
        Ok(Self::with_backend(profile, Arc::new(backend)))
    }

    fn with_backend(profile: &str, backend: Arc<dyn SecretBackend>) -> Self {
        Self {
            profile: Arc::from(profile),
            backend,
            lock: Arc::new(Mutex::new(())),
        }
    }

    async fn load_secrets(&self) -> Result<Secrets, String> {
        let Some(encoded) = self.backend.load(&self.profile).await? else {
            return Ok(Secrets::default());
        };
        serde_json::from_str(&encoded)
            .map_err(|error| format!("stored OAuth profile is invalid: {error}"))
    }

    async fn save_secrets(&self, secrets: &Secrets) -> Result<(), String> {
        if secrets.tokens.is_empty() && secrets.registrations.is_empty() {
            return self.backend.remove(&self.profile).await;
        }
        let encoded = serde_json::to_string(secrets)
            .map_err(|error| format!("cannot encode OAuth credentials: {error}"))?;
        self.backend.save(&self.profile, &encoded).await
    }

    pub async fn clear(&self) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        self.backend.remove(&self.profile).await
    }

    pub async fn clear_tokens(&self) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let mut secrets = self.load_secrets().await?;
        secrets.tokens.clear();
        self.save_secrets(&secrets).await
    }

    pub async fn has_tokens(&self) -> Result<bool, String> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| !secrets.tokens.is_empty())
    }
}

#[async_trait]
impl OAuthTokenStore for CredentialStore {
    async fn load(
        &self,
        binding: &OAuthTokenBinding,
    ) -> Result<Option<OAuthStoredToken>, OAuthClientError> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| {
                secrets
                    .tokens
                    .into_iter()
                    .find(|stored| stored.binding == *binding)
                    .map(|stored| stored.token)
            })
            .map_err(OAuthClientError::TokenStore)
    }

    async fn save(
        &self,
        binding: &OAuthTokenBinding,
        token: &OAuthStoredToken,
    ) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::TokenStore)?;
        secrets.tokens.retain(|stored| stored.binding != *binding);
        secrets.tokens.push(StoredToken {
            binding: binding.clone(),
            token: token.clone(),
        });
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::TokenStore)
    }

    async fn remove(&self, binding: &OAuthTokenBinding) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::TokenStore)?;
        secrets.tokens.retain(|stored| stored.binding != *binding);
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::TokenStore)
    }
}

#[async_trait]
impl OAuthClientRegistrationStore for CredentialStore {
    async fn load(
        &self,
        issuer: &str,
    ) -> Result<Option<OAuthClientRegistration>, OAuthClientError> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| {
                secrets
                    .registrations
                    .into_iter()
                    .find(|stored| stored.issuer == issuer)
                    .map(|stored| stored.registration)
            })
            .map_err(OAuthClientError::CredentialStore)
    }

    async fn save(
        &self,
        issuer: &str,
        registration: &OAuthClientRegistration,
    ) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::CredentialStore)?;
        secrets
            .registrations
            .retain(|stored| stored.issuer != issuer);
        secrets.registrations.push(StoredRegistration {
            issuer: issuer.to_string(),
            registration: registration.clone(),
        });
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::CredentialStore)
    }

    async fn remove(&self, issuer: &str) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::CredentialStore)?;
        secrets
            .registrations
            .retain(|stored| stored.issuer != issuer);
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::CredentialStore)
    }
}

type BrowserOpener = dyn Fn(&str) -> Result<(), String> + Send + Sync;

#[derive(Clone)]
struct BrowserAuthorizationHandler {
    profile: Arc<str>,
    interactive: bool,
    open_browser: bool,
    opener: Arc<BrowserOpener>,
}

impl BrowserAuthorizationHandler {
    fn new(profile: &str, interactive: bool, open_browser: bool) -> Self {
        Self {
            profile: Arc::from(profile),
            interactive,
            open_browser,
            opener: Arc::new(|url| webbrowser::open(url).map_err(|error| error.to_string())),
        }
    }
}

#[async_trait]
impl OAuthAuthorizationHandler for BrowserAuthorizationHandler {
    async fn authorize(
        &self,
        request: OAuthAuthorizationRequest,
    ) -> Result<OAuthAuthorizationAction, OAuthClientError> {
        if !self.interactive {
            return Err(OAuthClientError::Redirect(format!(
                "interactive authorization is required; run `mcp-repl --login {} --http {}` first",
                self.profile, request.resource
            )));
        }
        if self.open_browser {
            match (self.opener)(&request.authorization_url) {
                Ok(()) => eprintln!("OAuth authorization opened in your browser."),
                Err(error) => eprintln!("Could not open a browser ({error})."),
            }
        }
        eprintln!(
            "Authorize this client, then return here (waiting up to 5 minutes):\n{}",
            request.authorization_url
        );
        Ok(OAuthAuthorizationAction::AwaitLoopback)
    }
}

pub fn build_flow(
    name: &str,
    resource_url: &str,
    metadata: &OAuthProfile,
    interactive: bool,
    open_browser: bool,
) -> Result<(OAuthAuthorizationFlow, CredentialStore), String> {
    let store = CredentialStore::keyring(name)?;
    let mut options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
        OAuthDynamicClientRegistration::native("mcp-repl", std::iter::empty::<String>()),
    );
    if let Some(client_id) = &metadata.client_id_metadata_document {
        options = options.with_client_id_metadata_document(client_id.clone());
    }
    let mut builder = OAuthAuthorizationFlow::builder(resource_url)
        .redirect_policy(OAuthRedirectPolicy::loopback())
        .registration_options(options)
        .registration_store(store.clone())
        .token_store(store.clone())
        .authorization_handler(BrowserAuthorizationHandler::new(
            name,
            interactive,
            open_browser,
        ));
    if let Some(issuer) = &metadata.authorization_server {
        builder = builder.preferred_authorization_server(issuer.clone());
    }
    let flow = builder.build().map_err(|error| error.to_string())?;
    Ok((flow, store))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(
            "OAuth profile names must be 1-64 characters containing only ASCII letters, digits, \
             `.`, `_`, or `-`"
                .to_string(),
        );
    }
    Ok(())
}

pub fn save_metadata(path: &Path, name: &str, profile: &OAuthProfile) -> Result<(), String> {
    validate_name(name)?;
    edit_config(path, |document| {
        if document.get("oauth").is_none() {
            document["oauth"] = Item::Table(Table::new());
        }
        let oauth = document["oauth"]
            .as_table_mut()
            .ok_or("top-level `oauth` must be a table")?;
        let mut table = Table::new();
        table["url"] = value(&profile.url);
        if !profile.scopes.is_empty() {
            let mut scopes = Array::new();
            for scope in &profile.scopes {
                scopes.push(scope.as_str());
            }
            table["scopes"] = value(scopes);
        }
        if let Some(client_id) = &profile.client_id_metadata_document {
            table["client_id_metadata_document"] = value(client_id);
        }
        if let Some(issuer) = &profile.authorization_server {
            table["authorization_server"] = value(issuer);
        }
        oauth[name] = Item::Table(table);
        Ok(())
    })
}

pub fn remove_metadata(path: &Path, name: &str) -> Result<bool, String> {
    validate_name(name)?;
    let mut removed = false;
    edit_config(path, |document| {
        let Some(oauth) = document.get_mut("oauth") else {
            return Ok(());
        };
        let table = oauth
            .as_table_mut()
            .ok_or("top-level `oauth` must be a table")?;
        removed = table.remove(name).is_some();
        if table.is_empty() {
            document.remove("oauth");
        }
        Ok(())
    })?;
    Ok(removed)
}

fn edit_config(
    path: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), String>,
) -> Result<(), String> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    edit(&mut document)?;
    write_atomic(path, &document.to_string())
        .map_err(|error| format!("{}: {error}", path.display()))
}

use crate::secure_file::write_atomic;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct MemoryBackend(RwLock<HashMap<String, String>>);

    #[async_trait]
    impl SecretBackend for MemoryBackend {
        async fn load(&self, profile: &str) -> Result<Option<String>, String> {
            Ok(self.0.read().unwrap().get(profile).cloned())
        }

        async fn save(&self, profile: &str, secret: &str) -> Result<(), String> {
            self.0
                .write()
                .unwrap()
                .insert(profile.to_string(), secret.to_string());
            Ok(())
        }

        async fn remove(&self, profile: &str) -> Result<(), String> {
            self.0.write().unwrap().remove(profile);
            Ok(())
        }
    }

    fn binding(resource: &str) -> OAuthTokenBinding {
        OAuthTokenBinding {
            resource: resource.to_string(),
            issuer: "https://auth.example".to_string(),
            client_id: "client".to_string(),
        }
    }

    #[test]
    fn secure_store_chunks_large_unicode_records_below_platform_limit() {
        let secret = format!("{}{}", "jwt.".repeat(1200), "🔐".repeat(700));
        let chunks = encode_chunks(&secret);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 1024));
        assert_eq!(decode_chunks(chunks).unwrap(), secret);

        let manifest = KeyringManifest {
            generation: "01abcdef".to_string(),
            chunks: 7,
        };
        assert_eq!(
            KeyringManifest::parse(&manifest.encode()).unwrap(),
            manifest
        );
        assert!(KeyringManifest::parse("v1:bad:not-a-number").is_err());
    }

    #[test]
    fn profile_names_are_bounded_for_platform_stores() {
        assert!(validate_name("work-prod_1.example").is_ok());
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert!(validate_name("contains/slash").is_err());
    }

    fn mock_keyring() -> (Arc<keyring_core::mock::Store>, KeyringSecretBackend) {
        let store = keyring_core::mock::Store::new().unwrap();
        let backend = KeyringSecretBackend::from_store(Ok(store.clone())).unwrap();
        (store, backend)
    }

    #[test]
    fn store_initialization_failure_keeps_headless_guidance() {
        let error =
            KeyringSecretBackend::from_store(Err(keyring_core::Error::NoDefaultStore)).unwrap_err();
        assert!(error.contains("credential store is unavailable"));
        assert!(error.contains("will not fall back to plaintext"));
        assert!(error.contains("MCP_BEARER"));
    }

    fn seed_legacy_record(store: &keyring_core::CredentialStore) {
        // Literal identifiers/format from the old v1 API, not the new helpers.
        store
            .build("mcp-repl/oauth", "work", None)
            .unwrap()
            .set_password("v1:01abcdef:2")
            .unwrap();
        store
            .build("mcp-repl/oauth", "work:01abcdef:0", None)
            .unwrap()
            .set_password("bGVnYWN5IA==")
            .unwrap();
        store
            .build("mcp-repl/oauth", "work:01abcdef:1", None)
            .unwrap()
            .set_password("c2VjcmV0")
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_store_reads_replaces_and_removes_legacy_chunks() {
        let (store, backend) = mock_keyring();
        seed_legacy_record(store.as_ref());
        assert_eq!(
            backend.load("work").await.unwrap().as_deref(),
            Some("legacy secret")
        );

        let secret = "🔐".repeat(700);
        backend.save("work", &secret).await.unwrap();
        assert_eq!(backend.load("work").await.unwrap(), Some(secret));
        let manifest = read_keyring_manifest(store.as_ref(), "work")
            .unwrap()
            .unwrap();
        assert_ne!(manifest.generation, "01abcdef");
        assert!(manifest.chunks > 1);
        for index in 0..manifest.chunks {
            let chunk = keyring_entry(store.as_ref(), &chunk_account("work", &manifest, index))
                .unwrap()
                .get_password()
                .unwrap();
            assert!(chunk.len() <= 1024);
        }
        for account in ["work:01abcdef:0", "work:01abcdef:1"] {
            assert!(matches!(
                keyring_entry(store.as_ref(), account)
                    .unwrap()
                    .get_password(),
                Err(keyring_core::Error::NoEntry)
            ));
        }

        backend.remove("work").await.unwrap();
        backend.remove("work").await.unwrap();
        assert_eq!(backend.load("work").await.unwrap(), None);
        for index in 0..manifest.chunks {
            assert!(matches!(
                keyring_entry(store.as_ref(), &chunk_account("work", &manifest, index))
                    .unwrap()
                    .get_password(),
                Err(keyring_core::Error::NoEntry)
            ));
        }
    }

    #[tokio::test]
    async fn missing_corrupt_and_inaccessible_records_remain_distinct() {
        let (store, backend) = mock_keyring();
        assert_eq!(backend.load("work").await.unwrap(), None);
        let entry = keyring_entry(store.as_ref(), "work").unwrap();
        entry.set_password("not a manifest").unwrap();
        assert!(
            backend
                .load("work")
                .await
                .unwrap_err()
                .contains("manifest is invalid")
        );
        entry.set_password("v1:01abcdef:1").unwrap();
        assert!(
            backend
                .load("work")
                .await
                .unwrap_err()
                .contains("record is incomplete")
        );
        entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap()
            .set_error(keyring_core::Error::NoStorageAccess(Box::new(
                std::io::Error::other("test store locked"),
            )));
        assert!(
            backend
                .load("work")
                .await
                .unwrap_err()
                .contains("test store locked")
        );
    }

    // Fail one entry construction after a chosen number of successful calls.
    // The underlying store is entirely in memory, including cleanup checks.
    struct FailingStore {
        inner: Arc<keyring_core::mock::Store>,
        calls: AtomicUsize,
        fail_on: usize,
    }

    impl keyring_core::api::CredentialStoreApi for FailingStore {
        fn vendor(&self) -> String {
            "mcp-repl test".to_string()
        }
        fn id(&self) -> String {
            "isolated".to_string()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn build(
            &self,
            service: &str,
            account: &str,
            modifiers: Option<&HashMap<&str, &str>>,
        ) -> keyring_core::Result<keyring_core::Entry> {
            assert_eq!(service, "mcp-repl/oauth");
            assert!(modifiers.is_none());
            if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_on {
                return Err(keyring_core::Error::NoStorageAccess(Box::new(
                    std::io::Error::other("injected build failure"),
                )));
            }
            self.inner.build(service, account, modifiers)
        }
    }

    #[tokio::test]
    async fn failed_chunk_or_manifest_write_preserves_previous_generation() {
        use keyring_core::api::CredentialStoreApi;
        // Read old manifest, write two chunks, publish manifest. Fail the
        // second chunk or the publication, exercising both rollback paths.
        for fail_on in [3, 4] {
            let inner = keyring_core::mock::Store::new().unwrap();
            seed_legacy_record(inner.as_ref());
            let store = Arc::new(FailingStore {
                inner: inner.clone(),
                calls: AtomicUsize::new(0),
                fail_on,
            });
            let backend = KeyringSecretBackend::from_store(Ok(store)).unwrap();
            assert!(
                backend
                    .save("work", &"x".repeat(KEYRING_CHUNK_BYTES + 1))
                    .await
                    .unwrap_err()
                    .contains("injected build failure")
            );
            assert_eq!(
                backend.load("work").await.unwrap().as_deref(),
                Some("legacy secret")
            );
            for entry in inner
                .search(&HashMap::from([("service", "mcp-repl/oauth")]))
                .unwrap()
            {
                let (_, account) = entry.get_specifiers().unwrap();
                if account != "work" && !account.starts_with("work:01abcdef:") {
                    assert!(matches!(
                        entry.get_password(),
                        Err(keyring_core::Error::NoEntry)
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn failed_chunk_deletion_can_be_retried_without_losing_manifest() {
        let (store, backend) = mock_keyring();
        seed_legacy_record(store.as_ref());
        let chunk = keyring_entry(store.as_ref(), "work:01abcdef:0").unwrap();
        chunk
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap()
            .set_error(keyring_core::Error::NoStorageAccess(Box::new(
                std::io::Error::other("test store locked"),
            )));
        assert!(
            backend
                .remove("work")
                .await
                .unwrap_err()
                .contains("test store locked")
        );
        assert!(
            read_keyring_manifest(store.as_ref(), "work")
                .unwrap()
                .is_some()
        );
        backend.remove("work").await.unwrap();
        assert_eq!(backend.load("work").await.unwrap(), None);
    }

    fn token(access_token: &str) -> OAuthStoredToken {
        OAuthStoredToken {
            access_token: access_token.to_string(),
            refresh_token: Some("refresh-secret".to_string()),
            expires_at: u64::MAX,
            scopes: vec!["openid".to_string()],
        }
    }

    #[tokio::test]
    async fn exact_bindings_and_issuers_share_one_secret_record() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend.clone());
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        OAuthTokenStore::save(&store, &binding("https://two/mcp"), &token("two"))
            .await
            .unwrap();
        let registration = OAuthClientRegistration::dynamically_registered(
            "https://auth.example",
            "client",
            Some("client-secret".to_string()),
        );
        OAuthClientRegistrationStore::save(&store, "https://auth.example", &registration)
            .await
            .unwrap();

        assert_eq!(
            OAuthTokenStore::load(&store, &binding("https://two/mcp"))
                .await
                .unwrap()
                .unwrap()
                .access_token,
            "two"
        );
        assert!(
            OAuthTokenStore::load(&store, &binding("https://other/mcp"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            OAuthClientRegistrationStore::load(&store, "https://auth.example")
                .await
                .unwrap()
                .unwrap(),
            registration
        );

        let encoded = backend.0.read().unwrap()["work"].clone();
        assert!(encoded.contains("refresh-secret"));
        assert!(encoded.contains("client-secret"));
        assert!(!format!("{store:?}").contains("secret"));
    }

    #[tokio::test]
    async fn clear_removes_every_secret_for_profile() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend.clone());
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        store.clear().await.unwrap();
        assert!(backend.0.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clearing_tokens_preserves_dynamic_registration() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend);
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        let registration = OAuthClientRegistration::dynamically_registered(
            "https://auth.example",
            "client",
            Some("client-secret".to_string()),
        );
        OAuthClientRegistrationStore::save(&store, "https://auth.example", &registration)
            .await
            .unwrap();

        assert!(store.has_tokens().await.unwrap());
        store.clear_tokens().await.unwrap();
        assert!(!store.has_tokens().await.unwrap());
        assert!(
            OAuthTokenStore::load(&store, &binding("https://one/mcp"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            OAuthClientRegistrationStore::load(&store, "https://auth.example")
                .await
                .unwrap(),
            Some(registration)
        );
    }

    fn request() -> OAuthAuthorizationRequest {
        OAuthAuthorizationRequest {
            authorization_url: "https://auth.example/authorize?state=test".to_string(),
            redirect_uri: "http://127.0.0.1:12345/callback".to_string(),
            resource: "https://mcp.example/mcp".to_string(),
            issuer: "https://auth.example".to_string(),
            scopes: vec!["openid".to_string()],
        }
    }

    #[tokio::test]
    async fn browser_handler_has_an_automation_safe_seam() {
        let opens = Arc::new(AtomicUsize::new(0));
        let opener = {
            let opens = opens.clone();
            Arc::new(move |_url: &str| {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }) as Arc<BrowserOpener>
        };
        let headless = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: false,
            open_browser: true,
            opener: opener.clone(),
        };
        let error = headless.authorize(request()).await.unwrap_err();
        assert!(error.to_string().contains("--login work"));
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let manual = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: true,
            open_browser: false,
            opener: opener.clone(),
        };
        assert!(matches!(
            manual.authorize(request()).await.unwrap(),
            OAuthAuthorizationAction::AwaitLoopback
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let browser = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: true,
            open_browser: true,
            opener,
        };
        assert!(matches!(
            browser.authorize(request()).await.unwrap(),
            OAuthAuthorizationAction::AwaitLoopback
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn metadata_round_trip_never_contains_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(&path, "[aliases]\nt = \"tools\"\n").unwrap();
        let profile = OAuthProfile {
            url: "https://mcp.example/mcp".to_string(),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            client_id_metadata_document: Some("https://client.example/metadata.json".to_string()),
            authorization_server: Some("https://auth.example".to_string()),
        };

        save_metadata(&path, "work", &profile).unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(source.contains("[oauth.work]"));
        assert!(source.contains("[aliases]"));
        assert!(!source.contains("token"));
        assert!(!source.contains("secret"));
        assert_eq!(
            crate::config::Config::parse(&source).unwrap().oauth["work"],
            profile
        );

        assert!(remove_metadata(&path, "work").unwrap());
        let source = std::fs::read_to_string(path).unwrap();
        assert!(!source.contains("[oauth"));
        assert!(source.contains("[aliases]"));
    }
}
