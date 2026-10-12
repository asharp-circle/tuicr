//! Personal-fork icon source: reads `~/.config/crcl/vim.yaml` and `repos.yaml` directly.
//! Missing, malformed, or non-string entries are skipped so labels fall back to text/names.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_yaml::Value;

use super::AppConfig;

const VIM_YAML: &str = ".config/crcl/vim.yaml";
const REPOS_YAML: &str = ".config/crcl/repos.yaml";

/// Fill icon tables from the crcl YAML files under `home`; entries already in `config` win.
pub(super) fn merge_crcl_icons(home: &Path, config: &mut AppConfig) {
    merge(
        &mut config.mode_icons,
        read_map(&home.join(VIM_YAML), "modes"),
    );
    merge(
        &mut config.repo_icons,
        read_map(&home.join(REPOS_YAML), "icons"),
    );
    merge(
        &mut config.organization_icons,
        read_map(&home.join(REPOS_YAML), "organization_icons"),
    );
}

fn merge(target: &mut BTreeMap<String, String>, icons: BTreeMap<String, String>) {
    for (name, icon) in icons {
        target.entry(name).or_insert(icon);
    }
}

fn read_map(path: &Path, key: &str) -> BTreeMap<String, String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(Value::Mapping(root)) = serde_yaml::from_str::<Value>(&contents) else {
        return BTreeMap::new();
    };
    let Some(Value::Mapping(entries)) = root.get(key) else {
        return BTreeMap::new();
    };
    entries
        .iter()
        .filter_map(|(name, icon)| match (name.as_str(), icon.as_str()) {
            (Some(name), Some(icon)) if !icon.is_empty() => {
                Some((name.to_string(), icon.to_string()))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(vim: Option<&str>, repos: Option<&str>) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/crcl")).unwrap();
        if let Some(v) = vim {
            fs::write(home.path().join(VIM_YAML), v).unwrap();
        }
        if let Some(r) = repos {
            fs::write(home.path().join(REPOS_YAML), r).unwrap();
        }
        home
    }

    #[test]
    fn should_read_modes_repos_and_organizations() {
        let home = home_with(
            Some("modes:\n  normal: N\n  bad: 1\n"),
            Some("icons:\n  a-b: R\norganization_icons:\n  crcl-main: O\n"),
        );
        let mut config = AppConfig::default();
        merge_crcl_icons(home.path(), &mut config);
        assert_eq!(config.mode_icons["normal"], "N");
        assert!(!config.mode_icons.contains_key("bad"));
        assert_eq!(config.repo_icons["a-b"], "R");
        assert_eq!(config.organization_icons["crcl-main"], "O");
    }

    #[test]
    fn should_not_override_explicit_config() {
        let home = home_with(Some("modes:\n  normal: Y\n  insert: I\n"), None);
        let mut config = AppConfig::default();
        config.mode_icons.insert("normal".into(), "C".into());
        merge_crcl_icons(home.path(), &mut config);
        assert_eq!(config.mode_icons["normal"], "C");
        assert_eq!(config.mode_icons["insert"], "I");
    }

    #[test]
    fn should_ignore_missing_and_malformed_files() {
        let home = home_with(Some("modes: [unterminated"), Some("- just\n- a list\n"));
        let mut config = AppConfig::default();
        merge_crcl_icons(home.path(), &mut config);
        merge_crcl_icons(Path::new("/nonexistent"), &mut config);
        assert!(config.mode_icons.is_empty() && config.repo_icons.is_empty());
        assert!(config.organization_icons.is_empty());
    }
}
