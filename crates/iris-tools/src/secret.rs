//! Camera passwords taken from an environment variable, never from the
//! command line, where process listings and shell history would expose them.

use std::env::{self, VarError};
use std::fmt;

/// A password read from the environment variable named on the command line.
/// Its `Debug` form never shows the value.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    /// The password itself.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// The clap value parser for `--password-env VAR`: reads the password from
/// the variable `VAR`, failing with an error that names it.
pub fn password_from_env(name: &str) -> Result<Password, String> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Password(value)),
        Ok(_) => Err(format!("password environment variable {name} is empty")),
        Err(VarError::NotPresent) => Err(format!(
            "password environment variable {name} is not set; export it first, for \
             example with `read -rs {name} && export {name}`"
        )),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "password environment variable {name} is not valid UTF-8"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_only_a_redaction() {
        let password = Password("s3cret".to_owned());
        assert_eq!(format!("{password:?}"), "Password(<redacted>)");
    }

    #[test]
    fn reads_the_named_variable() {
        // The test process inherits PATH; setting a variable in-process needs `unsafe`.
        let expected = env::var("PATH").expect("tests run with PATH set");
        assert_eq!(password_from_env("PATH").unwrap().expose(), expected);
    }

    #[test]
    fn a_missing_variable_is_named_in_the_error() {
        let missing = password_from_env("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A").unwrap_err();
        assert!(
            missing.contains("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A") && missing.contains("not set"),
            "{missing}"
        );
    }
}
