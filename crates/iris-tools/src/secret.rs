//! Camera passwords taken from an environment variable, never from the
//! command line, where process listings and shell history would expose them.

use std::fmt;

/// A password read from the environment variable named on the command line.
/// Its `Debug` form never shows the value.
#[derive(Clone, PartialEq, Eq)]
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
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Password(value)),
        Ok(_) => Err(format!("password environment variable {name} is empty")),
        Err(std::env::VarError::NotPresent) => Err(format!(
            "password environment variable {name} is not set; export it first, for \
             example with `read -rs {name} && export {name}`"
        )),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "password environment variable {name} is not valid UTF-8"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_comes_from_the_named_variable_and_never_prints() {
        // The test process inherits PATH; setting a variable in-process needs `unsafe`.
        let expected = std::env::var("PATH").expect("tests run with PATH set");
        let password = password_from_env("PATH").unwrap();
        assert_eq!(password.expose(), expected);
        assert!(!format!("{password:?}").contains(&expected));
        let missing = password_from_env("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A").unwrap_err();
        assert!(
            missing.contains("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A"),
            "{missing}"
        );
        assert!(missing.contains("not set"), "{missing}");
    }
}
