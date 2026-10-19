//! Find Claude and Codex config dirs under the home dir (DESIGN §1, §4.1).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;
use crate::identity::{has_oauth_account, read_identity};
use crate::keychain::{SecretStore, read_credentials};
use crate::model::{AccountIdentity, Tool};
use crate::paths::Paths;
use crate::profile::Profile;

/// What a config dir is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirKind {
    /// Has an `oauthAccount` and readable credentials.
    LoggedIn,
    NotLoggedIn,
    /// A local proxy / API-key setup. Never offer it as an account.
    Proxy,
}

/// One discovered dir with suggestions for its profile entry.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredDir {
    pub dir: PathBuf,
    pub tool: Tool,
    pub is_default_install: bool,
    pub kind: DirKind,
    pub identity: AccountIdentity,
    pub suggested_id: String,
    pub suggested_label: String,
}

/// Scan `<home>/.claude`, `<home>/.claude-*`, `<home>/.codex` and
/// `<home>/.codex-*` dirs (and the common homes of the config), Claude
/// first, each sorted by path.
pub fn discover(paths: &Paths, config: &Config, store: &dyn SecretStore) -> Vec<DiscoveredDir> {
    let names: Vec<(String, PathBuf)> = std::fs::read_dir(&paths.home)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for tool in Tool::ALL {
        let base = tool.dir_name();
        let common = config.common_home_for(paths, tool);
        let mut dirs: Vec<PathBuf> = names
            .iter()
            .filter(|(n, _)| {
                (n == base || n.starts_with(&format!("{base}-"))) && !n.ends_with(".json")
            })
            .map(|(_, p)| p.clone())
            .collect();
        if common.is_dir() && !dirs.contains(&common) {
            dirs.push(common.clone());
        }
        dirs.sort();
        out.extend(
            dirs.into_iter()
                .map(|dir| inspect(paths, tool, &common, store, dir)),
        );
    }
    out
}

fn inspect(
    paths: &Paths,
    tool: Tool,
    common_home: &Path,
    store: &dyn SecretStore,
    dir: PathBuf,
) -> DiscoveredDir {
    let is_default_install = dir == common_home;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffix = name.strip_prefix(&format!("{}-", tool.dir_name()));
    let suffix = match (tool, suffix) {
        (Tool::Claude, Some(s)) => s.to_string(),
        (Tool::Claude, None) => "default".to_string(),
        (Tool::Codex, Some(s)) => format!("codex-{s}"),
        (Tool::Codex, None) => "codex".to_string(),
    };
    let profile = Profile::bare(&suffix, tool, dir.clone(), common_home.to_path_buf());
    let mut identity = read_identity(&profile, paths);
    let oauth = has_oauth_account(&profile, paths);
    let creds = read_credentials(store, &profile).ok().flatten();
    let kind = match tool {
        Tool::Claude => {
            let env = settings_env(&dir);
            let base_url = env.iter().any(|k| k == "ANTHROPIC_BASE_URL");
            let anthropic_env = env.iter().any(|k| k.starts_with("ANTHROPIC_"));
            if base_url || (!oauth && creds.is_none() && anthropic_env) {
                DirKind::Proxy
            } else if oauth && creds.is_some() {
                DirKind::LoggedIn
            } else {
                DirKind::NotLoggedIn
            }
        }
        Tool::Codex => match crate::codex::read_login(&dir) {
            Ok(crate::codex::CodexLogin::ChatGpt(..)) => DirKind::LoggedIn,
            Ok(crate::codex::CodexLogin::ApiKey) => DirKind::Proxy,
            _ => DirKind::NotLoggedIn,
        },
    };
    if let Some(c) = &creds {
        identity.subscription = c.subscription_type.clone();
        identity.rate_limit_tier = c.rate_limit_tier.clone();
    }
    let suggested_label = suggest_label(&identity, &suffix);
    DiscoveredDir {
        dir,
        tool,
        is_default_install,
        kind,
        identity,
        suggested_id: suffix,
        suggested_label,
    }
}

/// Keys of `settings.json` `.env`.
fn settings_env(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("env").and_then(Value::as_object).cloned())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The account's display name, or `fallback` when it has none.
fn suggest_label(id: &AccountIdentity, fallback: &str) -> String {
    id.display_name
        .clone()
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::tests::{FakeStore, SAMPLE};
    use crate::profile::keychain_service_for;
    use std::fs;

    #[test]
    fn classifies_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let h = &paths.home;
        for d in [".claude", ".claude-work", ".claude-new", ".claude-proxy"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        fs::write(h.join(".claude-x.json"), "{}").unwrap();
        let acct = r#"{"oauthAccount":{"displayName":"Al","organizationName":"Acme Team"}}"#;
        fs::write(h.join(".claude-work/.claude.json"), acct).unwrap();
        fs::write(
            h.join(".claude-proxy/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://localhost"}}"#,
        )
        .unwrap();
        let mut store = FakeStore::default();
        store.0.insert(
            keychain_service_for(&h.join(".claude-work"), false),
            SAMPLE.into(),
        );
        for d in [".codex", ".codex-two", ".codex-key", ".codexbar"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        fs::write(
            h.join(".codex-two/auth.json"),
            crate::codex::tests::sample_auth("two@x", "plus", "acc2", 9),
        )
        .unwrap();
        fs::write(
            h.join(".codex-key/auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#,
        )
        .unwrap();
        let found = discover(&paths, &Config::default(), &store);
        let kinds: Vec<_> = found
            .iter()
            .map(|d| (d.suggested_id.as_str(), d.tool, d.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("default", Tool::Claude, DirKind::NotLoggedIn),
                ("new", Tool::Claude, DirKind::NotLoggedIn),
                ("proxy", Tool::Claude, DirKind::Proxy),
                ("work", Tool::Claude, DirKind::LoggedIn),
                ("codex", Tool::Codex, DirKind::NotLoggedIn),
                ("codex-key", Tool::Codex, DirKind::Proxy),
                ("codex-two", Tool::Codex, DirKind::LoggedIn),
            ]
        );
        assert!(found[4].is_default_install);
        let two = &found[6];
        assert_eq!(two.suggested_label, "Neo");
        assert_eq!(two.identity.subscription.as_deref(), Some("plus"));
        assert_eq!(two.identity.email.as_deref(), Some("two@x"));
        assert!(found[0].is_default_install);
        let work = &found[3];
        assert_eq!(work.suggested_label, "Al");
        assert_eq!(work.identity.subscription.as_deref(), Some("team"));
    }
}
