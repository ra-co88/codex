use std::future::Future;
use std::pin::Pin;

use codex_config::ScopedSkillsConfig;

use crate::ExecServerError;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::environment::CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR;
use crate::environment::CODEX_EXEC_SERVER_URL_ENV_VAR;
use crate::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment::REMOTE_ENVIRONMENT_ID;

/// Lists the remote environment transports available to Codex.
///
/// Implementations own a startup snapshot containing both the available
/// environment transport list in configured order and the default environment
/// selection. Providers return transport descriptions before the effective HTTP
/// policy is available; `include_local` controls whether `EnvironmentManager`
/// should add the local environment when the snapshot is built.
pub trait EnvironmentProvider: Send + Sync {
    /// Returns the provider-owned environment startup snapshot.
    fn snapshot(&self) -> EnvironmentProviderFuture<'_>;
}

pub type EnvironmentProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<EnvironmentProviderSnapshot, ExecServerError>> + Send + 'a>>;

#[derive(Clone)]
pub struct EnvironmentProviderSnapshot {
    pub(crate) environments: Vec<EnvironmentProviderEntry>,
    pub default: EnvironmentDefault,
    pub include_local: bool,
}

/// Keeps each configured environment's requirements with its transport.
#[derive(Clone, Debug)]
pub(crate) struct EnvironmentProviderEntry {
    pub(crate) id: String,
    pub(crate) transport: ExecServerTransportParams,
    pub(crate) skills: ScopedSkillsConfig,
}

impl std::fmt::Debug for EnvironmentProviderSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let environment_ids: Vec<_> = self.environments.iter().map(|entry| &entry.id).collect();
        f.debug_struct("EnvironmentProviderSnapshot")
            .field("environments", &environment_ids)
            .field("default", &self.default)
            .field("include_local", &self.include_local)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentDefault {
    Disabled,
    EnvironmentId(String),
}

/// Default provider backed by `CODEX_EXEC_SERVER_URL`.
#[derive(Clone, Debug)]
pub struct DefaultEnvironmentProvider {
    exec_server_url: Option<String>,
    auth_bearer_token: Option<String>,
}

impl DefaultEnvironmentProvider {
    /// Builds a provider from an already-read raw `CODEX_EXEC_SERVER_URL` value.
    pub fn new(exec_server_url: Option<String>) -> Self {
        Self {
            exec_server_url,
            auth_bearer_token: None,
        }
    }

    /// Sends `Authorization: Bearer <token>` when connecting to the remote
    /// exec-server. Empty or whitespace-only tokens are ignored.
    pub fn with_auth_bearer_token(mut self, auth_bearer_token: Option<String>) -> Self {
        self.auth_bearer_token = auth_bearer_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        self
    }

    /// Builds a provider by reading `CODEX_EXEC_SERVER_URL` and the optional
    /// `CODEX_EXEC_SERVER_AUTH_TOKEN`.
    pub fn from_env() -> Self {
        Self::new(std::env::var(CODEX_EXEC_SERVER_URL_ENV_VAR).ok())
            .with_auth_bearer_token(std::env::var(CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR).ok())
    }

    pub(crate) fn snapshot_inner(&self) -> EnvironmentProviderSnapshot {
        let mut environments = Vec::new();
        let (exec_server_url, disabled) = normalize_exec_server_url(self.exec_server_url.clone());

        if let Some(exec_server_url) = exec_server_url {
            environments.push(EnvironmentProviderEntry {
                id: REMOTE_ENVIRONMENT_ID.to_string(),
                transport: websocket_transport(exec_server_url, self.auth_bearer_token.as_deref()),
                skills: ScopedSkillsConfig::default(),
            });
        }

        let has_remote = environments
            .iter()
            .any(|entry| entry.id == REMOTE_ENVIRONMENT_ID);
        let include_local = !disabled && !has_remote;
        let default = if disabled {
            EnvironmentDefault::Disabled
        } else if has_remote {
            EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string())
        } else {
            EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
        };

        EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        }
    }
}

fn websocket_transport(
    exec_server_url: String,
    auth_bearer_token: Option<&str>,
) -> ExecServerTransportParams {
    let mut transport = ExecServerTransportParams::websocket_url(
        exec_server_url,
        DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
    );
    let Some(token) = auth_bearer_token else {
        return transport;
    };
    let ExecServerTransportParams::WebSocketUrl { http_headers, .. } = &mut transport else {
        return transport;
    };
    match http::HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(mut value) => {
            value.set_sensitive(true);
            http_headers.insert(http::header::AUTHORIZATION, value);
        }
        Err(_) => tracing::warn!(
            "ignoring {CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR}: not a valid HTTP header value"
        ),
    }
    transport
}

impl EnvironmentProvider for DefaultEnvironmentProvider {
    fn snapshot(&self) -> EnvironmentProviderFuture<'_> {
        Box::pin(async { Ok(self.snapshot_inner()) })
    }
}

pub(crate) fn normalize_exec_server_url(exec_server_url: Option<String>) -> (Option<String>, bool) {
    match exec_server_url.as_deref().map(str::trim) {
        None | Some("") => (None, false),
        Some(url) if url.eq_ignore_ascii_case("none") => (None, true),
        Some(url) => (Some(url.to_string()), false),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn default_provider_requests_local_environment_when_url_is_missing() {
        let provider = DefaultEnvironmentProvider::new(/*exec_server_url*/ None);
        let snapshot = provider.snapshot().await.expect("environments");
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let environments: HashMap<_, _> = environments
            .into_iter()
            .map(|entry| (entry.id, entry.transport))
            .collect();

        assert!(include_local);
        assert!(!environments.contains_key(LOCAL_ENVIRONMENT_ID));
        assert!(!environments.contains_key(REMOTE_ENVIRONMENT_ID));
        assert_eq!(
            default,
            EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
        );
    }

    #[tokio::test]
    async fn default_provider_requests_local_environment_when_url_is_empty() {
        let provider = DefaultEnvironmentProvider::new(Some(String::new()));
        let snapshot = provider.snapshot().await.expect("environments");
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let environments: HashMap<_, _> = environments
            .into_iter()
            .map(|entry| (entry.id, entry.transport))
            .collect();

        assert!(include_local);
        assert!(!environments.contains_key(LOCAL_ENVIRONMENT_ID));
        assert!(!environments.contains_key(REMOTE_ENVIRONMENT_ID));
        assert_eq!(
            default,
            EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
        );
    }

    #[tokio::test]
    async fn default_provider_omits_local_environment_for_none_value() {
        let provider = DefaultEnvironmentProvider::new(Some("none".to_string()));
        let snapshot = provider.snapshot().await.expect("environments");
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let environments: HashMap<_, _> = environments
            .into_iter()
            .map(|entry| (entry.id, entry.transport))
            .collect();

        assert!(!include_local);
        assert!(!environments.contains_key(LOCAL_ENVIRONMENT_ID));
        assert!(!environments.contains_key(REMOTE_ENVIRONMENT_ID));
        assert_eq!(default, EnvironmentDefault::Disabled);
    }

    #[tokio::test]
    async fn default_provider_adds_remote_environment_for_websocket_url() {
        let provider = DefaultEnvironmentProvider::new(Some("ws://127.0.0.1:8765".to_string()));
        let snapshot = provider.snapshot().await.expect("environments");
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let environments: HashMap<_, _> = environments
            .into_iter()
            .map(|entry| (entry.id, entry.transport))
            .collect();

        assert!(!include_local);
        assert!(!environments.contains_key(LOCAL_ENVIRONMENT_ID));
        assert!(matches!(
            &environments[REMOTE_ENVIRONMENT_ID],
            ExecServerTransportParams::WebSocketUrl { websocket_url, .. }
                if websocket_url == "ws://127.0.0.1:8765"
        ));
        assert_eq!(
            default,
            EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string())
        );
    }

    #[test]
    fn normalizes_exec_server_url() {
        assert_eq!(
            normalize_exec_server_url(Some(" ws://127.0.0.1:8765 ".to_string())),
            (Some("ws://127.0.0.1:8765".to_string()), false)
        );
    }

    #[test]
    fn default_provider_sends_bearer_token_for_websocket_url() {
        let snapshot = DefaultEnvironmentProvider::new(Some("ws://127.0.0.1:8765".to_string()))
            .with_auth_bearer_token(Some(" secret-token \n".to_string()))
            .snapshot_inner();
        let transport = snapshot
            .environments
            .into_iter()
            .find(|entry| entry.id == REMOTE_ENVIRONMENT_ID)
            .expect("remote environment")
            .transport;
        let ExecServerTransportParams::WebSocketUrl { http_headers, .. } = transport else {
            panic!("expected websocket transport");
        };
        let authorization = &http_headers[http::header::AUTHORIZATION];
        assert_eq!(authorization, "Bearer secret-token");
        assert!(authorization.is_sensitive());
    }

    #[test]
    fn default_provider_ignores_blank_bearer_token() {
        let snapshot = DefaultEnvironmentProvider::new(Some("ws://127.0.0.1:8765".to_string()))
            .with_auth_bearer_token(Some("   ".to_string()))
            .snapshot_inner();
        let transport = snapshot
            .environments
            .into_iter()
            .find(|entry| entry.id == REMOTE_ENVIRONMENT_ID)
            .expect("remote environment")
            .transport;
        let ExecServerTransportParams::WebSocketUrl { http_headers, .. } = transport else {
            panic!("expected websocket transport");
        };
        assert!(http_headers.get(http::header::AUTHORIZATION).is_none());
    }
}
