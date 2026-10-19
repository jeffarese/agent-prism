//! Who a profile is logged in as (DESIGN §4.1).

use serde_json::Value;

use crate::model::{AccountIdentity, Tool};
use crate::paths::Paths;
use crate::profile::Profile;

/// Read `.oauthAccount` from the profile's `.claude.json`. A missing or
/// unreadable file gives an empty identity. Plan fields are left empty
/// (they come from the credentials). Codex: the claims of `auth.json`,
/// plan included.
pub fn read_identity(profile: &Profile, paths: &Paths) -> AccountIdentity {
    if profile.tool == Tool::Codex {
        return match crate::codex::read_login(&profile.dir) {
            Ok(crate::codex::CodexLogin::ChatGpt(_, id)) => id,
            _ => AccountIdentity {
                tool: Tool::Codex,
                ..Default::default()
            },
        };
    }
    let path = profile.claude_json_path(paths);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| identity_from_json(&v))
        .unwrap_or_default()
}

/// True if the `.claude.json` has an `oauthAccount` object (Codex: if
/// `auth.json` holds a ChatGPT login).
pub(crate) fn has_oauth_account(profile: &Profile, paths: &Paths) -> bool {
    if profile.tool == Tool::Codex {
        return matches!(
            crate::codex::read_login(&profile.dir),
            Ok(crate::codex::CodexLogin::ChatGpt(..))
        );
    }
    std::fs::read_to_string(profile.claude_json_path(paths))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v.get("oauthAccount").is_some_and(Value::is_object))
}

fn identity_from_json(v: &Value) -> AccountIdentity {
    let acct = &v["oauthAccount"];
    let field = |k: &str| acct.get(k).and_then(Value::as_str).map(str::to_string);
    AccountIdentity {
        display_name: field("displayName"),
        email: field("emailAddress"),
        org: field("organizationName"),
        subscription: None,
        rate_limit_tier: None,
        tool: Tool::Claude,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_oauth_account() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let dir = paths.home.join(".claude-x");
        std::fs::create_dir_all(&dir).unwrap();
        let p = Profile::bare("x", Tool::Claude, dir.clone(), paths.home.join(".claude"));
        assert_eq!(read_identity(&p, &paths), AccountIdentity::default());
        std::fs::write(
            dir.join(".claude.json"),
            r#"{"oauthAccount":{"displayName":"Al","emailAddress":"j@x","organizationName":"Org"}}"#,
        )
        .unwrap();
        let id = read_identity(&p, &paths);
        assert_eq!(id.display_name.as_deref(), Some("Al"));
        assert_eq!(id.email.as_deref(), Some("j@x"));
        assert_eq!(id.org.as_deref(), Some("Org"));
        assert!(has_oauth_account(&p, &paths));
    }

    #[test]
    fn reads_codex_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let dir = paths.home.join(".codex-x");
        std::fs::create_dir_all(&dir).unwrap();
        let p = Profile::bare("x", Tool::Codex, dir.clone(), paths.home.join(".codex"));
        assert!(!has_oauth_account(&p, &paths));
        assert_eq!(read_identity(&p, &paths).tool, Tool::Codex);
        std::fs::write(
            dir.join("auth.json"),
            crate::codex::tests::sample_auth("n@x", "plus", "acc", 9),
        )
        .unwrap();
        let id = read_identity(&p, &paths);
        assert_eq!(id.email.as_deref(), Some("n@x"));
        assert_eq!(id.subscription.as_deref(), Some("plus"));
        assert!(has_oauth_account(&p, &paths));
    }
}
