//! Config discovery, parsing, and layering for `hpds.toml`.
//!
//! Layering: **built-in defaults ← user config ← project config ← CLI
//! flags**. Each file parses into a [`Layer`] (only the keys it actually
//! sets); layers are applied to [`Config::default`] in order, so later
//! layers win key-by-key.
//!
//! This module returns data only; it never prints. Warnings about unknown
//! keys and malformed GitHub logins are returned on [`Loaded::warnings`]
//! for the caller to report through `ui/`.

mod discover;
pub(crate) mod raw;

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::ui::HintExt;

/// Fully resolved configuration; `Default` is the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub project: ProjectConfig,
    pub audit: AuditConfig,
}

/// Valid `[project] status` values: the machine-readable lifecycle.
pub const PROJECT_STATUSES: &[&str] = &["active", "submitted", "published", "retired"];

/// `[project]`: lifecycle metadata used by `hpds audit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    /// active | submitted | published | retired
    pub status: String,
    /// GitHub username; audit checks they watch the repo.
    pub primary_author: String,
}

/// `[audit]`: knobs for `hpds audit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditConfig {
    /// Branches with no commits in more than this many days count as stale.
    pub stale_days: u32,
    /// GitHub logins that must watch every lab repo (the project's
    /// primary author is required in addition to these). User config
    /// replaces the built-in list; project config only adds to it, so an
    /// audited repo can require extra watchers but never drop one. Entries
    /// are deduplicated case-insensitively, keeping the first spelling.
    pub required_watchers: Vec<String>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            project: ProjectConfig {
                status: "active".to_string(),
                primary_author: String::new(),
            },
            audit: AuditConfig {
                stale_days: 90,
                required_watchers: strings(&["malcolmbarrett", "sherrirose"]),
            },
        }
    }
}

/// One configuration layer: only the keys this source actually set.
///
/// A parsed config file becomes a `Layer`, and CLI flags that override
/// config keys are expressed as a `Layer` too, so all four layers merge
/// through the same [`Config::apply`] path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub project_status: Option<String>,
    pub project_primary_author: Option<String>,
    pub audit_stale_days: Option<u32>,
    pub audit_required_watchers: Option<Vec<String>>,
}

impl Config {
    /// Apply a layer on top of `self`; every key the layer sets wins.
    pub fn apply(&mut self, layer: Layer) {
        if let Some(v) = layer.project_status {
            self.project.status = v;
        }
        if let Some(v) = layer.project_primary_author {
            self.project.primary_author = v;
        }
        if let Some(v) = layer.audit_stale_days {
            self.audit.stale_days = v;
        }
        if let Some(v) = layer.audit_required_watchers {
            self.audit.required_watchers = v;
        }
    }

    /// Apply a project-config layer on top of `self`.
    ///
    /// Every key behaves as in [`Config::apply`] except
    /// `required_watchers`, which is merged into the current list instead of
    /// replacing it: the result is the current entries followed by the
    /// project's, deduplicated case-insensitively with the first spelling
    /// and first position kept. The audited repo may require more watchers,
    /// never fewer.
    pub fn apply_project(&mut self, mut layer: Layer) {
        if let Some(extra) = layer.audit_required_watchers.take() {
            self.audit.required_watchers.extend(extra);
        }
        dedupe_logins(&mut self.audit.required_watchers);
        self.apply(layer);
    }
}

/// Drop later entries that match an earlier one case-insensitively, keeping
/// the first spelling and the original order.
fn dedupe_logins(logins: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    logins.retain(|login| seen.insert(fold_login(login)));
}

/// GitHub logins are case-insensitive; compare them that way.
pub fn same_login(a: &str, b: &str) -> bool {
    fold_login(a) == fold_login(b)
}

/// The case-folded form of a GitHub login, for comparisons and lookups.
pub fn fold_login(login: &str) -> String {
    login.to_lowercase()
}

/// Whether `text` is a well-formed GitHub login: 1 to 39 ASCII letters,
/// digits, or hyphens, not starting with a hyphen.
pub fn is_github_login(text: &str) -> bool {
    (1..=39).contains(&text.len())
        && !text.starts_with('-')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `value` as a GitHub login, with one leading `@` removed, or `None` when
/// what remains is not a well-formed login.
fn normalize_login(value: &str) -> Option<&str> {
    let login = value.strip_prefix('@').unwrap_or(value);
    is_github_login(login).then_some(login)
}

/// Remove malformed GitHub logins from a file's layer, warning about each
/// one with the file's path. A blank `primary-author` means unset and is
/// kept as an empty string without a warning; an invalid one becomes empty
/// too, so it overrides lower layers the same way the file intended to.
fn validate_logins(layer: &mut Layer, path: &Path, warnings: &mut Vec<String>) {
    if let Some(author) = layer.project_primary_author.as_mut() {
        if author.trim().is_empty() {
            author.clear();
        } else if let Some(login) = normalize_login(author) {
            *author = login.to_string();
        } else {
            warnings.push(format!(
                "ignoring invalid GitHub login `{author}` in `project.primary-author` of {}: \
                 set it to a single login (letters, digits, and hyphens only), or leave it empty",
                path.display()
            ));
            author.clear();
        }
    }
    if let Some(watchers) = layer.audit_required_watchers.as_mut() {
        let mut kept = Vec::with_capacity(watchers.len());
        for value in watchers.drain(..) {
            match normalize_login(&value) {
                Some(login) => kept.push(login.to_string()),
                None => warnings.push(format!(
                    "ignoring invalid GitHub login `{value}` in `audit.required-watchers` of {}: \
                     list each login as its own string (letters, digits, and hyphens only)",
                    path.display()
                )),
            }
        }
        *watchers = kept;
    }
}

/// Typed error for `--config` pointing at a file that does not exist: a
/// bad flag value, so `main` renders it as a usage error and exits 2.
#[derive(Debug, thiserror::Error)]
#[error("config file `{}` does not exist", path.display())]
pub struct MissingConfigFile {
    pub path: PathBuf,
}

impl MissingConfigFile {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        "check the path passed to --config, or drop the flag to discover \
         hpds.toml automatically"
            .to_string()
    }
}

/// The result of [`load`]: the resolved config, which files contributed,
/// and any unknown-key warnings for the caller to print via `ui::warn`.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// User config file, when it existed and was layered in.
    pub user_path: Option<PathBuf>,
    /// Project config file (`--config` or discovered `hpds.toml`).
    pub project_path: Option<PathBuf>,
    /// Human-readable warnings (unknown keys, malformed GitHub logins);
    /// print through `ui::warn`.
    pub warnings: Vec<String>,
}

/// Discover, parse, and layer configuration.
///
/// `explicit` is the global `--config <path>` flag: it replaces project-file
/// discovery, so the file is layered as project config, and it is an error
/// for it not to exist. `flags` carries any CLI-flag overrides (the final
/// layer).
pub fn load(cwd: &Path, explicit: Option<&Path>, flags: Layer) -> anyhow::Result<Loaded> {
    let user_path = discover::user_config_path().filter(|path| path.is_file());

    let project_path = match explicit {
        Some(path) => {
            if !path.is_file() {
                // Typed so `main` can exit 2: a bad flag value is a usage
                // error, not a runtime failure.
                return Err(anyhow::Error::new(MissingConfigFile {
                    path: path.to_path_buf(),
                }));
            }
            Some(path.to_path_buf())
        }
        None => discover::find_project_config(cwd),
    };
    let (config, warnings) = layer_files(user_path.as_deref(), project_path.as_deref(), flags)?;

    Ok(Loaded {
        config,
        user_path,
        project_path,
        warnings,
    })
}

/// Layer the defaults, the user file, the project file, and the flags, in
/// that order, returning the resolved config and any warnings.
fn layer_files(
    user_path: Option<&Path>,
    project_path: Option<&Path>,
    flags: Layer,
) -> anyhow::Result<(Config, Vec<String>)> {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    if let Some(path) = user_path {
        config.apply(load_file(path, &mut warnings)?);
    }
    // `--config` may name the user file itself; layering it a second time
    // would change nothing but repeat its warnings.
    let project_path =
        project_path.filter(|project| !user_path.is_some_and(|user| same_file(user, project)));
    if let Some(path) = project_path {
        config.apply_project(load_file(path, &mut warnings)?);
    }
    // `apply` replaces `required_watchers`; a future CLI flag for that key
    // would need additive handling like `apply_project`.
    config.apply(flags);
    dedupe_logins(&mut config.audit.required_watchers);
    Ok((config, warnings))
}

/// Whether two paths name the same file: compared canonicalized when both
/// resolve, and as given otherwise.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Read and parse one config file into a layer, converting its unknown keys
/// and malformed GitHub logins into warnings that name the file.
fn load_file(path: &Path, warnings: &mut Vec<String>) -> anyhow::Result<Layer> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read config file `{}`", path.display()))
        .hint("check the file's permissions, or remove it if it should not exist")?;
    let parsed = raw::parse(&text)
        .with_context(|| format!("could not parse `{}`", path.display()))
        .hint("fix the TOML shown above; run `hpds config --help` and see hpds.toml docs for the supported keys")?;
    for key in parsed.unknown_keys {
        warnings.push(format!(
            "ignoring unknown key `{key}` in {}",
            path.display()
        ));
    }
    let mut layer = parsed.layer;
    validate_logins(&mut layer, path, warnings);
    Ok(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_project_and_audit() {
        let config = Config::default();
        assert_eq!(config.project.status, "active");
        assert_eq!(config.project.primary_author, "");
        assert_eq!(config.audit.stale_days, 90);
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose"])
        );
    }

    #[test]
    fn audit_stale_days_layers_like_any_other_key() {
        let user = Layer {
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        assert_eq!(config.audit.stale_days, 30);
        config.apply(project);
        assert_eq!(config.audit.stale_days, 45);
        // a layer that does not set the key leaves it alone
        config.apply(Layer::default());
        assert_eq!(config.audit.stale_days, 45);
    }

    #[test]
    fn layering_defaults_then_user_then_project_then_flags() {
        // default < user < project < flag. Each layer overrides only the
        // keys it sets; everything else shines through.
        let user = Layer {
            audit_stale_days: Some(30),
            project_status: Some("submitted".to_string()),
            project_primary_author: Some("malcolm".to_string()),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            project_status: Some("published".to_string()),
            ..Layer::default()
        };
        let flags = Layer {
            audit_stale_days: Some(7),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        config.apply(flags);

        // flag beat project beat user for audit.stale-days
        assert_eq!(config.audit.stale_days, 7);
        // project beat user for status
        assert_eq!(config.project.status, "published");
        // user's value survives where nothing above set the key
        assert_eq!(config.project.primary_author, "malcolm");
        // untouched keys keep built-in defaults
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose"])
        );
    }

    #[test]
    fn audit_keys_layer_like_everything_else() {
        // User config overrides the built-in lab leads; a later layer wins.
        let user = Layer {
            audit_required_watchers: Some(strings(&["lead1", "lead2"])),
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            ..Layer::default()
        };
        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        assert_eq!(config.audit.stale_days, 45);
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
    }

    fn watchers_layer(names: &[&str]) -> Layer {
        Layer {
            audit_required_watchers: Some(strings(names)),
            ..Layer::default()
        }
    }

    #[test]
    fn project_required_watchers_add_to_the_user_list() {
        let mut config = Config::default();
        config.apply(watchers_layer(&["lead1", "lead2"]));
        config.apply_project(watchers_layer(&["collab1", "collab2"]));
        assert_eq!(
            config.audit.required_watchers,
            strings(&["lead1", "lead2", "collab1", "collab2"])
        );
    }

    #[test]
    fn project_required_watchers_add_to_the_defaults_without_user_config() {
        let mut config = Config::default();
        config.apply_project(watchers_layer(&["collab1"]));
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose", "collab1"])
        );
    }

    #[test]
    fn required_watchers_dedupe_case_insensitively_keeping_the_first_spelling() {
        let mut config = Config::default();
        config.apply(watchers_layer(&["Lead1", "lead2"]));
        config.apply_project(watchers_layer(&["LEAD2", "Collab1", "collab1", "lead1"]));
        assert_eq!(
            config.audit.required_watchers,
            strings(&["Lead1", "lead2", "Collab1"])
        );
    }

    #[test]
    fn duplicates_inside_the_base_list_collapse_on_project_merge() {
        let mut config = Config::default();
        config.apply(watchers_layer(&["lead1", "LEAD1", "lead2"]));
        config.apply_project(watchers_layer(&["Lead1", "collab1", "COLLAB1"]));
        assert_eq!(
            config.audit.required_watchers,
            strings(&["lead1", "lead2", "collab1"])
        );
    }

    #[test]
    fn duplicates_inside_the_user_file_collapse_without_project_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join("config.toml");
        std::fs::write(
            &user,
            "[audit]\nrequired-watchers = [\"lead1\", \"LEAD1\", \"lead2\"]\n",
        )
        .expect("write user");
        let (config, warnings) = layer_files(Some(&user), None, Layer::default()).expect("loads");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
    }

    #[test]
    fn project_required_watchers_cannot_drop_base_names() {
        let mut config = Config::default();
        config.apply(watchers_layer(&["lead1", "lead2"]));
        config.apply_project(watchers_layer(&[]));
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
        config.apply_project(watchers_layer(&["lead2"]));
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
    }

    #[test]
    fn project_required_watchers_load_from_files_without_a_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join("config.toml");
        std::fs::write(&user, "[audit]\nrequired-watchers = [\"lead1\"]\n").expect("write user");
        let project = dir.path().join("hpds.toml");
        std::fs::write(
            &project,
            "[audit]\nstale-days = 30\nrequired-watchers = [\"collab1\"]\n",
        )
        .expect("write project");

        let (config, warnings) =
            layer_files(Some(&user), Some(&project), Layer::default()).expect("loads");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.audit.stale_days, 30);
        assert_eq!(
            config.audit.required_watchers,
            strings(&["lead1", "collab1"])
        );
    }

    /// Load `contents` as the user file through the normal file path.
    fn load_user(contents: &str) -> (tempfile::TempDir, PathBuf, Config, Vec<String>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join("config.toml");
        std::fs::write(&user, contents).expect("write user");
        let (config, warnings) = layer_files(Some(&user), None, Layer::default()).expect("loads");
        (dir, user, config, warnings)
    }

    #[test]
    fn invalid_required_watcher_is_dropped_with_a_warning_naming_the_file() {
        let (_dir, user, config, warnings) =
            load_user("[audit]\nrequired-watchers = [\"alice, bob\"]\n");
        assert!(config.audit.required_watchers.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(warning.contains("`alice, bob`"), "{warning}");
        assert!(warning.contains("audit.required-watchers"), "{warning}");
        assert!(
            warning.contains(&user.display().to_string()),
            "names the file: {warning}"
        );
        assert!(
            warning.contains("list each login as its own string"),
            "says what to do: {warning}"
        );
    }

    #[test]
    fn valid_required_watchers_are_kept_in_order_around_invalid_ones() {
        let (_dir, _user, config, warnings) = load_user(
            "[audit]\nrequired-watchers = [\"lead1\", \"x @bob\", \"Lead-2\", \"-dash\", \"\", \"collab3\"]\n",
        );
        assert_eq!(
            config.audit.required_watchers,
            strings(&["lead1", "Lead-2", "collab3"])
        );
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("`x @bob`"), "{warnings:?}");
        assert!(warnings[1].contains("`-dash`"), "{warnings:?}");
        assert!(warnings[2].contains("``"), "{warnings:?}");
    }

    #[test]
    fn invalid_project_required_watchers_are_dropped_with_a_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("hpds.toml");
        std::fs::write(
            &project,
            "[audit]\nrequired-watchers = [\"collab1\", \"a b\"]\n",
        )
        .expect("write project");
        let (config, warnings) =
            layer_files(None, Some(&project), Layer::default()).expect("loads");
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose", "collab1"])
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("`a b`"), "{warnings:?}");
        assert!(
            warnings[0].contains(&project.display().to_string()),
            "{warnings:?}"
        );
    }

    #[test]
    fn invalid_primary_author_is_dropped_with_a_warning() {
        let (_dir, user, config, warnings) =
            load_user("[project]\nprimary-author = \"alice smith\"\n");
        assert_eq!(config.project.primary_author, "");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(warning.contains("`alice smith`"), "{warning}");
        assert!(warning.contains("project.primary-author"), "{warning}");
        assert!(
            warning.contains(&user.display().to_string()),
            "names the file: {warning}"
        );
    }

    #[test]
    fn invalid_primary_author_overrides_a_lower_layer_with_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join("config.toml");
        std::fs::write(&user, "[project]\nprimary-author = \"malcolm\"\n").expect("write user");
        let project = dir.path().join("hpds.toml");
        std::fs::write(&project, "[project]\nprimary-author = \"a/b\"\n").expect("write project");
        let (config, warnings) =
            layer_files(Some(&user), Some(&project), Layer::default()).expect("loads");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(config.project.primary_author, "");
    }

    #[test]
    fn empty_primary_author_is_unset_without_a_warning() {
        let (_dir, _user, config, warnings) = load_user("[project]\nprimary-author = \"\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.project.primary_author, "");
    }

    #[test]
    fn blank_primary_author_is_unset_without_a_warning() {
        let (_dir, _user, config, warnings) = load_user("[project]\nprimary-author = \"  \"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.project.primary_author, "");
    }

    #[test]
    fn one_leading_at_sign_is_stripped_without_a_warning() {
        let (_dir, _user, config, warnings) = load_user(
            "[project]\nprimary-author = \"@malcolm\"\n[audit]\nrequired-watchers = [\"@lead1\", \"lead2\", \"@LEAD2\"]\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.project.primary_author, "malcolm");
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
    }

    #[test]
    fn a_doubled_or_bare_at_sign_is_invalid() {
        let (_dir, _user, config, warnings) =
            load_user("[audit]\nrequired-watchers = [\"@@lead1\", \"@\", \"lead2\"]\n");
        assert_eq!(config.audit.required_watchers, strings(&["lead2"]));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("`@@lead1`"), "{warnings:?}");
        assert!(warnings[1].contains("`@`"), "{warnings:?}");
    }

    #[test]
    fn github_login_rule() {
        assert!(is_github_login("a"));
        assert!(is_github_login("Mixed-Case-9"));
        assert!(is_github_login(&"a".repeat(39)));
        assert!(!is_github_login(&"a".repeat(40)));
        assert!(!is_github_login(""));
        assert!(!is_github_login("-dash"));
        assert!(!is_github_login("alice, bob"));
        assert!(!is_github_login("@alice"));
        assert!(!is_github_login("dependabot[bot]"));
        assert!(!is_github_login("j\u{f6}rg"));
    }

    #[test]
    fn a_project_path_naming_the_user_file_is_layered_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join("config.toml");
        std::fs::write(
            &user,
            "bogus = 1\n[audit]\nrequired-watchers = [\"lead1\", \"a b\"]\n",
        )
        .expect("write user");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let indirect = dir.path().join("sub").join("..").join("config.toml");
        assert_ne!(user, indirect, "the paths differ until canonicalized");
        let (config, warnings) =
            layer_files(Some(&user), Some(&indirect), Layer::default()).expect("loads");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(config.audit.required_watchers, strings(&["lead1"]));
    }

    #[test]
    fn same_file_falls_back_to_a_plain_comparison_for_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.toml");
        assert!(same_file(&missing, &missing));
        assert!(!same_file(&missing, &dir.path().join("other.toml")));
    }

    #[test]
    fn load_reports_missing_explicit_config_as_a_typed_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let err = load(dir.path(), Some(&missing), Layer::default())
            .expect_err("missing --config file must be an error");
        assert!(
            err.to_string().contains("nope.toml"),
            "names the file: {err}"
        );
        let typed = err
            .downcast_ref::<MissingConfigFile>()
            .expect("typed so main can exit 2 (usage error)");
        assert!(typed.hint().contains("--config"), "hint: {}", typed.hint());
    }
}
