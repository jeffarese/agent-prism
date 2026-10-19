//! Env vars of agent-prism (`AGP…`). An empty value counts as unset.

use std::ffi::OsString;

/// The value of `name`, or `None` when it is unset or empty.
pub fn var_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// [`var_os`] as a UTF-8 string.
pub fn var(name: &str) -> Option<String> {
    var_os(name).and_then(|v| v.into_string().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_counts_as_unset() {
        // SAFETY: only this test uses this name.
        unsafe { std::env::set_var("AGP_ENV_TEST_X", "") };
        assert_eq!(var("AGP_ENV_TEST_X"), None);
        unsafe { std::env::set_var("AGP_ENV_TEST_X", "new") };
        assert_eq!(var("AGP_ENV_TEST_X").as_deref(), Some("new"));
        unsafe { std::env::remove_var("AGP_ENV_TEST_X") };
        assert_eq!(var("AGP_NOT_SET_ANYWHERE"), None);
    }
}
