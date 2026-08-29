use std::{
    collections::BTreeMap,
    fmt,
    io::Write,
    process::{Command, Stdio},
};

use kurama_protocol::{KuramaError, config::AuthRef};
use zeroize::Zeroizing;

pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
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

    pub fn read_native(&self, service: &str, account: &str) -> Result<SecretValue, KuramaError> {
        validate_keychain_parts(service, account)?;
        let output = match std::env::consts::OS {
            "macos" => Command::new("security")
                .args(["find-generic-password", "-s", service, "-a", account, "-w"])
                .output(),
            "linux" => Command::new("secret-tool")
                .args(["lookup", "service", service, "account", account])
                .output(),
            _ => {
                return Err(KuramaError::Configuration(
                    "native keychain unavailable".into(),
                ));
            }
        }
        .map_err(|error| {
            KuramaError::Configuration(format!("native keychain command failed: {error}"))
        })?;

        if !output.status.success() {
            return Err(KuramaError::Configuration(format!(
                "native keychain lookup failed with status {}",
                output.status
            )));
        }
        let value = String::from_utf8(output.stdout).map_err(|_| {
            KuramaError::Configuration("native keychain returned non-UTF-8 secret".into())
        })?;
        non_empty_secret(trim_command_newline(value), "native keychain credential")
    }

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

        let status = match std::env::consts::OS {
            "macos" => Command::new("security")
                .args([
                    "add-generic-password",
                    "-U",
                    "-s",
                    service,
                    "-a",
                    account,
                    "-w",
                    secret.expose(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
            "linux" => {
                let mut child = Command::new("secret-tool")
                    .args([
                        "store",
                        "--label=Kurama",
                        "service",
                        service,
                        "account",
                        account,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|error| {
                        KuramaError::Configuration(format!(
                            "native keychain command failed: {error}"
                        ))
                    })?;
                let write_result = child
                    .stdin
                    .take()
                    .ok_or_else(|| {
                        KuramaError::Configuration(
                            "native keychain command stdin unavailable".into(),
                        )
                    })?
                    .write_all(secret.expose().as_bytes());
                let status = child.wait();
                write_result?;
                status
            }
            _ => {
                return Err(KuramaError::Configuration(
                    "native keychain unavailable".into(),
                ));
            }
        }
        .map_err(|error| {
            KuramaError::Configuration(format!("native keychain command failed: {error}"))
        })?;

        if status.success() {
            Ok(())
        } else {
            Err(KuramaError::Configuration(format!(
                "native keychain write failed with status {status}"
            )))
        }
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

fn trim_command_newline(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}
