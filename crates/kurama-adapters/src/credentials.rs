use std::{collections::BTreeMap, fmt};

use kurama_protocol::{KuramaError, config::AuthRef};
use zeroize::Zeroizing;

#[cfg(all(feature = "native-credentials", target_os = "linux"))]
#[path = "credentials/linux.rs"]
mod linux_native;

pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_zeroizing(self) -> Zeroizing<String> {
        self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

#[derive(Default)]
pub struct SessionSecrets {
    values: BTreeMap<String, SecretValue>,
}

impl SessionSecrets {
    pub fn insert(&mut self, profile_name: impl Into<String>, secret: SecretValue) {
        self.values.insert(profile_name.into(), secret);
    }

    pub fn remove(&mut self, profile_name: &str) -> Option<SecretValue> {
        self.values.remove(profile_name)
    }

    pub fn contains(&self, profile_name: &str) -> bool {
        self.values.contains_key(profile_name)
    }

    fn resolve(&self, profile_name: &str) -> Result<SecretValue, KuramaError> {
        let secret = self.values.get(profile_name).ok_or_else(|| {
            KuramaError::Configuration(format!(
                "session credential is not available for profile {profile_name}"
            ))
        })?;
        non_empty_secret(secret.expose().to_owned(), "session credential")
    }
}

impl fmt::Debug for SessionSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSecrets")
            .field("profiles", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CredentialResolver;

impl CredentialResolver {
    pub fn resolve(
        &self,
        profile_name: &str,
        auth_ref: &AuthRef,
        session_secrets: &SessionSecrets,
    ) -> Result<SecretValue, KuramaError> {
        match auth_ref {
            AuthRef::Environment { name } => {
                validate_environment_name(name)?;
                let value = std::env::var(name).map_err(|_| {
                    KuramaError::Configuration(format!(
                        "environment credential {name} is unavailable"
                    ))
                })?;
                non_empty_secret(value, "environment credential")
            }
            AuthRef::Keychain { service, account } => self.read_native(service, account),
            AuthRef::Session => session_secrets.resolve(profile_name),
        }
    }

    pub fn resolve_optional(
        &self,
        profile_name: &str,
        auth_ref: Option<&AuthRef>,
        session_secrets: &SessionSecrets,
    ) -> Result<Option<SecretValue>, KuramaError> {
        auth_ref
            .map(|auth_ref| self.resolve(profile_name, auth_ref, session_secrets))
            .transpose()
    }

    /// Resolves a native credential synchronously. Linux subprocess work has a
    /// ten-second deadline; macOS Security.framework may block for OS keychain
    /// authorization and does not expose a cancellable operation or deadline.
    pub fn read_native(&self, service: &str, account: &str) -> Result<SecretValue, KuramaError> {
        validate_keychain_parts(service, account)?;
        #[cfg(all(feature = "native-credentials", target_os = "macos"))]
        {
            let bytes = Zeroizing::new(
                security_framework::passwords::generic_password(
                    security_framework::passwords::PasswordOptions::new_generic_password(
                        service, account,
                    ),
                )
                .map_err(|_| KuramaError::Configuration("native keychain lookup failed".into()))?,
            );
            native_secret(&bytes, false)
        }
        #[cfg(all(feature = "native-credentials", target_os = "linux"))]
        {
            linux_native::read(service, account)
        }
        #[cfg(not(all(
            feature = "native-credentials",
            any(target_os = "macos", target_os = "linux")
        )))]
        Err(KuramaError::Configuration(
            "native keychain unavailable".into(),
        ))
    }

    /// Stores secret bytes without placing them in command arguments. The same
    /// synchronous OS authorization limitations as `read_native` apply.
    pub fn write_native(
        &self,
        service: &str,
        account: &str,
        secret: &SecretValue,
    ) -> Result<(), KuramaError> {
        validate_keychain_parts(service, account)?;
        if secret.expose().is_empty() {
            return Err(KuramaError::Configuration(
                "native keychain credential is empty".into(),
            ));
        }

        #[cfg(all(feature = "native-credentials", target_os = "macos"))]
        {
            security_framework::passwords::set_generic_password(
                service,
                account,
                secret.expose().as_bytes(),
            )
            .map_err(|_| KuramaError::Configuration("native keychain write failed".into()))
        }
        #[cfg(all(feature = "native-credentials", target_os = "linux"))]
        {
            linux_native::write(service, account, secret)
        }
        #[cfg(not(all(
            feature = "native-credentials",
            any(target_os = "macos", target_os = "linux")
        )))]
        Err(KuramaError::Configuration(
            "native keychain unavailable".into(),
        ))
    }
}

pub fn parse_auth_ref(value: &str) -> Result<AuthRef, KuramaError> {
    if value == "session" {
        return Ok(AuthRef::Session);
    }
    if let Some(name) = value.strip_prefix("env:") {
        validate_environment_name(name)?;
        return Ok(AuthRef::Environment {
            name: name.to_owned(),
        });
    }
    if let Some(value) = value.strip_prefix("keychain:") {
        let (service, account) = value.split_once('/').ok_or_else(invalid_auth_ref)?;
        validate_keychain_parts(service, account).map_err(|_| invalid_auth_ref())?;
        return Ok(AuthRef::Keychain {
            service: service.to_owned(),
            account: account.to_owned(),
        });
    }
    Err(invalid_auth_ref())
}

pub fn format_auth_ref(auth_ref: &AuthRef) -> String {
    match auth_ref {
        AuthRef::Environment { name } => format!("env:{name}"),
        AuthRef::Keychain { service, account } => format!("keychain:{service}/{account}"),
        AuthRef::Session => "session".into(),
    }
}

fn non_empty_secret(value: String, source: &str) -> Result<SecretValue, KuramaError> {
    if value.is_empty() {
        Err(KuramaError::Configuration(format!("{source} is empty")))
    } else {
        Ok(SecretValue::new(value))
    }
}

fn validate_environment_name(name: &str) -> Result<(), KuramaError> {
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
    if !valid_first || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        return Err(invalid_auth_ref());
    }
    Ok(())
}

fn validate_keychain_parts(service: &str, account: &str) -> Result<(), KuramaError> {
    if !safe_keychain_part(service) || !safe_keychain_part(account) {
        return Err(KuramaError::Configuration(
            "invalid keychain service or account".into(),
        ));
    }
    Ok(())
}

fn safe_keychain_part(value: &str) -> bool {
    !value.is_empty() && !value.contains(['/', '\0']) && !value.chars().any(char::is_control)
}

fn invalid_auth_ref() -> KuramaError {
    KuramaError::Configuration("auth must be env:NAME, keychain:SERVICE/ACCOUNT, or session".into())
}

#[cfg(any(
    test,
    all(
        feature = "native-credentials",
        any(target_os = "macos", target_os = "linux")
    )
))]
fn native_secret(bytes: &[u8], command_newline: bool) -> Result<SecretValue, KuramaError> {
    let value = std::str::from_utf8(bytes).map_err(|_| {
        KuramaError::Configuration("native keychain returned non-UTF-8 secret".into())
    })?;
    let value = if command_newline {
        value.strip_suffix('\n').unwrap_or(value)
    } else {
        value
    };
    let value = if command_newline && bytes.ends_with(b"\r\n") {
        value.strip_suffix('\r').unwrap_or(value)
    } else {
        value
    };
    non_empty_secret(value.to_owned(), "native keychain credential")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_secret_decoding_preserves_api_bytes_and_trims_only_cli_terminator() {
        assert_eq!(
            native_secret(b"secret-value\r\n", false).unwrap().expose(),
            "secret-value\r\n"
        );
        assert_eq!(
            native_secret(b"secret-value\r\n", true).unwrap().expose(),
            "secret-value"
        );
        assert_eq!(
            native_secret(b"secret-value\n\n", true).unwrap().expose(),
            "secret-value\n"
        );
        assert_eq!(
            native_secret(b"secret-value\r", true).unwrap().expose(),
            "secret-value\r"
        );
        assert!(native_secret(b"\r\n", true).is_err());
        let error = native_secret(b"secret-value\xff", false).unwrap_err();
        assert!(!error.to_string().contains("secret-value"));
        assert!(!format!("{error:?}").contains("secret-value"));
    }

    #[test]
    fn invalid_native_requests_are_rejected_without_accessing_credentials() {
        let resolver = CredentialResolver;
        let secret = SecretValue::new("test-owned-secret".into());
        assert!(resolver.read_native("invalid/service", "account").is_err());
        assert!(
            resolver
                .write_native("service", "invalid/account", &secret)
                .is_err()
        );
        assert!(
            resolver
                .write_native("service", "account", &SecretValue::new(String::new()))
                .is_err()
        );
        assert!(!format!("{secret:?}").contains("test-owned-secret"));
    }

    #[cfg(not(feature = "native-credentials"))]
    #[test]
    fn native_requests_require_the_native_credentials_feature() {
        let resolver = CredentialResolver;
        assert!(
            resolver
                .read_native("test-service", "test-account")
                .is_err()
        );
        assert!(
            resolver
                .write_native(
                    "test-service",
                    "test-account",
                    &SecretValue::new("test-owned-secret".into())
                )
                .is_err()
        );
    }
}
