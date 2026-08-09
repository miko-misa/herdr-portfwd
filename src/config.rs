//! Host and behavior configuration.
//!
//! Hosts come from a `hosts.toml` in either `~/.config/herdr-portfwd` or
//! `~/.config/herdr-mirror`, so herdr-mirror users need no extra setup. Both
//! use the same shape; this crate reads only `target` and `prefix` and ignores
//! mirror's own keys. When both files exist this crate's own file wins and the
//! other is reported as shadowed rather than silently dropped, which is the
//! courtesy herdr-mirror extends to its own candidate dirs.
//!
//! Behavior settings live in `~/.config/herdr-portfwd/config.toml`
//! (`poll_seconds`, `offer_new_listeners`, `require_pane_attribution`, and the
//! opt-in `[[auto]]` allowlist). A missing file means defaults, never an error:
//! the plugin has to work for someone who never wrote one.
//!
//! Parsing is callable as `parse_hosts(&str)` / `parse_settings(&str)` with no
//! I/O so it can be tested against fixtures; only `load_*` touches a disk.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::Host;

/// Seconds between discovery passes. Deliberately shorter than mirror's 60: a
/// dev server that has already printed its URL is worth noticing quickly.
pub const DEFAULT_POLL_SECONDS: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {message}")]
    File { path: String, message: String },
    #[error("{0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

/// Hosts, plus everything worth telling the user about where they came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostsConfig {
    /// In declaration order. Order is user-visible wherever hosts are listed.
    pub hosts: Vec<Host>,
    /// Verbatim from the file; may not name a host we kept.
    pub default_host: Option<String>,
    /// Which file this came from. `None` when parsed from a string, or when no
    /// candidate existed.
    pub source: Option<PathBuf>,
    /// Candidates that also hold a `hosts.toml` and are therefore ignored.
    pub shadowed: Vec<PathBuf>,
    /// Hosts skipped with a reason. Skipping beats aborting: one malformed
    /// entry must not make every other host unusable.
    pub warnings: Vec<String>,
}

impl HostsConfig {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    pub fn by_name(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.name == name)
    }

    /// The configured default, if it names a host that survived parsing.
    ///
    /// No "first host declared" fallback, unlike herdr-mirror: picking a host
    /// out of several without being told to would forward ports on a machine
    /// the user did not mean. Resolution handles the single-host case instead,
    /// where there is nothing to guess.
    pub fn default_host(&self) -> Option<&Host> {
        self.default_host
            .as_deref()
            .and_then(|name| self.by_name(name))
    }
}

#[derive(Debug, Deserialize)]
struct RawHosts {
    default_host: Option<String>,
    /// `toml::Table` with `preserve_order` keeps declaration order.
    #[serde(default)]
    hosts: toml::Table,
}

#[derive(Debug, Deserialize)]
struct RawHost {
    target: Option<String>,
    prefix: Option<String>,
    enabled: Option<bool>,
    /// herdr-mirror's; only `ssh` hosts have something to forward through.
    kind: Option<String>,
}

/// Parse a `hosts.toml`. Unknown keys, global or per-host, are ignored: this
/// file is shared with herdr-mirror and most of it is not ours.
pub fn parse_hosts(text: &str) -> Result<HostsConfig> {
    let raw: RawHosts = toml::from_str(text)?;
    let mut hosts: Vec<Host> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for (name, value) in raw.hosts {
        let host: RawHost = match value.try_into() {
            Ok(host) => host,
            Err(err) => {
                warnings.push(format!("skipping host [hosts.{name}]: {err}"));
                continue;
            }
        };
        if host.enabled == Some(false) {
            continue;
        }
        match host.kind.as_deref() {
            None | Some("ssh") => {}
            Some(other) => {
                warnings.push(format!(
                    "skipping host [hosts.{name}]: kind \"{other}\" has no ssh target to forward through"
                ));
                continue;
            }
        }
        let target = host.target.unwrap_or_default();
        if target.trim().is_empty() {
            warnings.push(format!("skipping host [hosts.{name}]: target is empty"));
            continue;
        }
        hosts.push(Host {
            // An unset prefix still labels mirrored workspaces, because mirror
            // falls back to the host name. Resolving "mini: scholion" back to a
            // host depends on reproducing that fallback exactly.
            prefix: Some(match host.prefix {
                Some(prefix) if !prefix.trim().is_empty() => prefix,
                _ => name.clone(),
            }),
            target: target.trim().to_string(),
            name,
        });
    }

    if let Some(default) = raw.default_host.as_deref() {
        if !hosts.iter().any(|host| host.name == default) {
            // A warning, not an error: this key may belong to herdr-mirror and
            // name a host we deliberately skipped, and one unusable default
            // must not make the usable hosts unreachable.
            warnings.push(format!(
                "default_host \"{default}\" is not a usable [hosts.*] entry"
            ));
        }
    }

    Ok(HostsConfig {
        hosts,
        default_host: raw.default_host,
        source: None,
        shadowed: Vec::new(),
        warnings,
    })
}

/// Config root, honoring `XDG_CONFIG_HOME` the way [`crate::ssh`] honors
/// `XDG_STATE_HOME`.
pub fn config_root() -> PathBuf {
    config_root_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn config_root_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = xdg.filter(|dir| !dir.as_os_str().is_empty()) {
        return dir;
    }
    match home.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(home) => home.join(".config"),
        None => PathBuf::from(".config"),
    }
}

/// `hosts.toml` candidates, most specific first. Our own file wins over
/// mirror's so a user who outgrows the shared config can say so.
pub fn hosts_file_candidates() -> Vec<PathBuf> {
    let root = config_root();
    vec![
        root.join("herdr-portfwd").join("hosts.toml"),
        root.join("herdr-mirror").join("hosts.toml"),
    ]
}

/// Where behavior settings live. Never mirror's: these keys are ours alone.
pub fn settings_file() -> PathBuf {
    config_root().join("herdr-portfwd").join("config.toml")
}

/// Load the first existing candidate. Nothing found is an empty config, not an
/// error: `status` should be able to say "no hosts configured" rather than
/// fail, and host resolution produces the actionable message.
pub fn load_hosts(candidates: &[PathBuf]) -> Result<HostsConfig> {
    let found: Vec<&PathBuf> = candidates.iter().filter(|path| path.is_file()).collect();
    let Some(file) = found.first().copied() else {
        return Ok(HostsConfig::default());
    };
    let text = read_file(file)?;
    let mut config = parse_hosts(&text).map_err(|err| ConfigError::File {
        path: file.display().to_string(),
        message: err.to_string(),
    })?;
    config.source = Some(file.clone());
    config.shadowed = found[1..].iter().map(|path| (*path).clone()).collect();
    Ok(config)
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|err| ConfigError::File {
        path: path.display().to_string(),
        message: err.to_string(),
    })
}

/// One `[[auto]]` entry: the L3 allowlist, which is empty unless the user
/// wrote one. See the automation table in `docs/ARCHITECTURE.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRule {
    /// Matched against a workspace id or its label; the caller knows which it
    /// has. Users write labels, hooks carry ids, and both are checked.
    pub workspace: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Seconds between discovery passes. `0` disables discovery entirely,
    /// which also disables every L2 offer.
    pub poll_seconds: u64,
    pub offer_new_listeners: bool,
    /// When false, listeners whose ancestry reaches no pane are offered too.
    /// Off-by-default noise: a real host's listener list is mostly daemons.
    pub require_pane_attribution: bool,
    pub auto: Vec<AutoRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            poll_seconds: DEFAULT_POLL_SECONDS,
            offer_new_listeners: true,
            require_pane_attribution: true,
            auto: Vec::new(),
        }
    }
}

impl Settings {
    pub fn discovery_enabled(&self) -> bool {
        self.poll_seconds > 0
    }

    /// Whether an L3 rule covers this port for this workspace. Call once per
    /// identifier the caller holds (id, then label).
    pub fn auto_allows(&self, workspace: &str, port: u16) -> bool {
        self.auto
            .iter()
            .any(|rule| rule.workspace == workspace && rule.ports.contains(&port))
    }
}

#[derive(Debug, Deserialize)]
struct RawSettings {
    poll_seconds: Option<u64>,
    offer_new_listeners: Option<bool>,
    require_pane_attribution: Option<bool>,
    #[serde(default)]
    auto: Vec<RawAuto>,
}

#[derive(Debug, Deserialize)]
struct RawAuto {
    workspace: String,
    #[serde(default)]
    ports: Vec<i64>,
}

/// Parse behavior settings. Unlike a host, a malformed `[[auto]]` entry is
/// fatal rather than skipped: silently dropping half of an allowlist rule
/// could forward a port the user never listed, and invariant 7 says nothing is
/// forwarded without an explicit trigger.
pub fn parse_settings(text: &str) -> Result<Settings> {
    let raw: RawSettings = toml::from_str(text)?;
    let mut auto = Vec::with_capacity(raw.auto.len());
    for rule in raw.auto {
        if rule.workspace.trim().is_empty() {
            return Err(invalid("[[auto]]: workspace is empty"));
        }
        let mut ports = Vec::with_capacity(rule.ports.len());
        for port in rule.ports {
            let port = u16::try_from(port)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| {
                    invalid(format!(
                        "[[auto]] for workspace \"{}\": {port} is not a port (1-65535)",
                        rule.workspace
                    ))
                })?;
            ports.push(port);
        }
        auto.push(AutoRule {
            workspace: rule.workspace,
            ports,
        });
    }
    let defaults = Settings::default();
    Ok(Settings {
        poll_seconds: raw.poll_seconds.unwrap_or(defaults.poll_seconds),
        offer_new_listeners: raw
            .offer_new_listeners
            .unwrap_or(defaults.offer_new_listeners),
        require_pane_attribution: raw
            .require_pane_attribution
            .unwrap_or(defaults.require_pane_attribution),
        auto,
    })
}

/// Load behavior settings, or the defaults when the file is absent.
pub fn load_settings(path: &Path) -> Result<Settings> {
    if !path.is_file() {
        return Ok(Settings::default());
    }
    let text = read_file(path)?;
    parse_settings(&text).map_err(|err| ConfigError::File {
        path: path.display().to_string(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hosts.toml shipped in the herdr-mirror README, global keys and all.
    const MIRROR_HOSTS: &str = r#"
autostart = true
poll_seconds = 60
default_host = "mac-mini-honkoma"
close_remote_on_local_close = false
always_control = false

[hosts.mac-mini-honkoma]
target = "mac-mini-honkoma"
prefix = "mini"
api_transport = "auto"
always_control = false
"#;

    #[test]
    fn a_herdr_mirror_hosts_file_loads_unchanged() {
        let config = parse_hosts(MIRROR_HOSTS).expect("mirror's own file must parse");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].name, "mac-mini-honkoma");
        assert_eq!(config.hosts[0].target, "mac-mini-honkoma");
        assert_eq!(config.hosts[0].prefix.as_deref(), Some("mini"));
        assert_eq!(
            config.default_host().map(|host| host.name.as_str()),
            Some("mac-mini-honkoma")
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn keys_this_crate_does_not_use_are_ignored_not_rejected() {
        let config = parse_hosts(
            "autostart = true\nsomething_new = 3\n\
             [hosts.a]\ntarget = \"a\"\nremote_bin = \"/opt/herdr\"\ndocker_bin = \"podman\"\n",
        )
        .expect("unknown keys must not fail the load");
        assert_eq!(config.hosts.len(), 1);
    }

    #[test]
    fn an_unset_prefix_falls_back_to_the_host_name() {
        // herdr-mirror labels workspaces with this same fallback, so anything
        // else breaks resolving "work: scholion" back to the host.
        let config = parse_hosts("[hosts.work]\ntarget = \"w\"\n").expect("minimal host");
        assert_eq!(config.hosts[0].prefix.as_deref(), Some("work"));

        let config = parse_hosts("[hosts.work]\ntarget = \"w\"\nprefix = \"  \"\n")
            .expect("blank prefix host");
        assert_eq!(config.hosts[0].prefix.as_deref(), Some("work"));
    }

    #[test]
    fn declaration_order_survives_parsing() {
        let config = parse_hosts("[hosts.zeta]\ntarget = \"z\"\n[hosts.alpha]\ntarget = \"a\"\n")
            .expect("two hosts");
        let names: Vec<&str> = config.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn disabled_hosts_are_dropped_without_a_warning() {
        let config =
            parse_hosts("[hosts.a]\ntarget = \"a\"\n[hosts.b]\ntarget = \"b\"\nenabled = false\n")
                .expect("one enabled host");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].name, "a");
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn one_unusable_host_is_skipped_with_a_reason_and_the_rest_survive() {
        let config = parse_hosts(
            "[hosts.good]\ntarget = \"g\"\n\
             [hosts.empty]\ntarget = \"   \"\n\
             [hosts.missing]\nprefix = \"m\"\n\
             [hosts.container]\nkind = \"docker\"\ncontainer = \"c\"\n\
             [hosts.wrongtype]\ntarget = 7\n",
        )
        .expect("a bad entry must not abort the load");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].name, "good");
        assert_eq!(config.warnings.len(), 4, "{:?}", config.warnings);
        let joined = config.warnings.join("\n");
        for name in ["empty", "missing", "container", "wrongtype"] {
            assert!(joined.contains(name), "{name} not named in {joined}");
        }
        assert!(
            joined.contains("docker"),
            "the kind must be named: {joined}"
        );
    }

    #[test]
    fn an_empty_or_hostless_file_is_an_empty_config_not_an_error() {
        for text in ["", "poll_seconds = 60\n", "[hosts]\n"] {
            let config = parse_hosts(text).unwrap_or_else(|err| panic!("{text:?}: {err}"));
            assert!(config.is_empty());
            assert!(config.default_host().is_none());
        }
    }

    #[test]
    fn a_default_host_naming_nothing_usable_warns_instead_of_failing() {
        let config = parse_hosts("default_host = \"gone\"\n[hosts.a]\ntarget = \"a\"\n")
            .expect("an unusable default must not hide the usable hosts");
        assert_eq!(config.hosts.len(), 1);
        assert!(config.default_host().is_none());
        assert_eq!(config.default_host.as_deref(), Some("gone"));
        assert!(config.warnings[0].contains("gone"), "{:?}", config.warnings);
    }

    #[test]
    fn truncated_and_duplicated_toml_is_rejected() {
        for text in [
            "[hosts.a\ntarget = \"a\"\n",
            "[hosts.a]\ntarget = \"a\n",
            "[hosts.a]\ntarget = \"a\"\n[hosts.a]\ntarget = \"b\"\n",
            "hosts = 5\n",
        ] {
            assert!(parse_hosts(text).is_err(), "should reject: {text:?}");
        }
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create candidate dir");
        }
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    #[test]
    fn no_candidate_file_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load_hosts(&[dir.path().join("nope/hosts.toml")]).expect("missing is fine");
        assert!(config.is_empty());
        assert_eq!(config.source, None);
        assert!(config.shadowed.is_empty());
    }

    #[test]
    fn our_own_hosts_file_wins_and_the_shadowed_one_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = write(
            dir.path(),
            "herdr-portfwd/hosts.toml",
            "[hosts.winner]\ntarget = \"w\"\n",
        );
        let theirs = write(
            dir.path(),
            "herdr-mirror/hosts.toml",
            "[hosts.loser]\ntarget = \"l\"\n",
        );
        let config = load_hosts(&[ours.clone(), theirs.clone()]).expect("load");
        assert_eq!(config.hosts[0].name, "winner");
        assert_eq!(config.source.as_ref(), Some(&ours));
        assert_eq!(
            config.shadowed,
            vec![theirs],
            "silently dropping it is the trap"
        );
    }

    #[test]
    fn a_mirror_only_setup_needs_no_portfwd_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("herdr-portfwd/hosts.toml");
        let theirs = write(dir.path(), "herdr-mirror/hosts.toml", MIRROR_HOSTS);
        let config = load_hosts(&[ours, theirs.clone()]).expect("load");
        assert_eq!(config.hosts[0].prefix.as_deref(), Some("mini"));
        assert_eq!(config.source.as_ref(), Some(&theirs));
        assert!(config.shadowed.is_empty());
    }

    #[test]
    fn a_malformed_file_names_the_file_in_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "herdr-portfwd/hosts.toml", "[hosts.a\n");
        let err = load_hosts(std::slice::from_ref(&path))
            .expect_err("a broken file must not look like an empty one")
            .to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
    }

    #[test]
    fn config_root_prefers_xdg_then_home() {
        assert_eq!(
            config_root_from(Some("/xdg".into()), Some("/home/u".into())),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            config_root_from(Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.config")
        );
        assert_eq!(config_root_from(None, None), PathBuf::from(".config"));
    }

    #[test]
    fn candidate_order_puts_our_own_config_first() {
        let candidates = hosts_file_candidates();
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].ends_with("herdr-portfwd/hosts.toml"));
        assert!(candidates[1].ends_with("herdr-mirror/hosts.toml"));
        assert!(settings_file().ends_with("herdr-portfwd/config.toml"));
    }

    #[test]
    fn absent_settings_and_an_empty_file_agree_on_the_defaults() {
        let defaults = Settings::default();
        assert_eq!(defaults.poll_seconds, 5);
        assert!(defaults.offer_new_listeners);
        assert!(defaults.require_pane_attribution);
        assert!(defaults.auto.is_empty(), "L3 is off until asked for");
        assert_eq!(parse_settings("").expect("empty file"), defaults);

        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("config.toml");
        assert_eq!(
            load_settings(&missing).expect("missing is defaults"),
            defaults
        );
    }

    #[test]
    fn settings_round_trip_from_a_full_file() {
        let settings = parse_settings(
            "poll_seconds = 15\noffer_new_listeners = false\nrequire_pane_attribution = false\n\
             [[auto]]\nworkspace = \"mini: scholion\"\nports = [5173, 3000]\n\
             [[auto]]\nworkspace = \"w1\"\nports = [65535]\n",
        )
        .expect("full settings file");
        assert_eq!(settings.poll_seconds, 15);
        assert!(!settings.offer_new_listeners);
        assert!(!settings.require_pane_attribution);
        assert!(settings.auto_allows("mini: scholion", 5173));
        assert!(settings.auto_allows("w1", 65535));
        assert!(
            !settings.auto_allows("w1", 5173),
            "rules must not leak across workspaces"
        );
        assert!(!settings.auto_allows("other", 5173));
    }

    #[test]
    fn zero_poll_seconds_disables_discovery() {
        assert!(Settings::default().discovery_enabled());
        let settings = parse_settings("poll_seconds = 0\n").expect("zero is a valid setting");
        assert!(!settings.discovery_enabled());
    }

    #[test]
    fn duplicate_auto_entries_for_one_workspace_are_a_union() {
        let settings = parse_settings(
            "[[auto]]\nworkspace = \"w1\"\nports = [5173]\n\
             [[auto]]\nworkspace = \"w1\"\nports = [3000, 5173]\n",
        )
        .expect("repeating a workspace is legal");
        assert_eq!(settings.auto.len(), 2);
        assert!(settings.auto_allows("w1", 5173));
        assert!(settings.auto_allows("w1", 3000));
    }

    #[test]
    fn an_auto_rule_with_no_ports_matches_nothing() {
        let settings = parse_settings("[[auto]]\nworkspace = \"w1\"\n").expect("ports default");
        assert!(!settings.auto_allows("w1", 5173));
    }

    #[test]
    fn a_port_outside_the_range_fails_the_whole_settings_file() {
        // Failing closed matters here: a dropped rule silently changes which
        // ports get forwarded without asking (invariant 7).
        for text in [
            "[[auto]]\nworkspace = \"w1\"\nports = [65536]\n",
            "[[auto]]\nworkspace = \"w1\"\nports = [-1]\n",
            "[[auto]]\nworkspace = \"w1\"\nports = [0]\n",
            "[[auto]]\nworkspace = \"\"\nports = [5173]\n",
            "[[auto]]\nports = [5173]\n",
            "[[auto]]\nworkspace = \"w1\"\nports = [\"5173\"]\n",
            "poll_seconds = -1\n",
        ] {
            assert!(parse_settings(text).is_err(), "should reject: {text:?}");
        }
        // ...and the boundary values themselves are fine.
        let settings = parse_settings("[[auto]]\nworkspace = \"w\"\nports = [1, 65535]\n")
            .expect("1 and 65535 are ports");
        assert!(settings.auto_allows("w", 1));
        assert!(settings.auto_allows("w", 65535));
    }

    #[test]
    fn unknown_settings_keys_are_ignored() {
        let settings = parse_settings("poll_seconds = 9\nfuture_key = \"x\"\n")
            .expect("a newer file must still load");
        assert_eq!(settings.poll_seconds, 9);
    }
}
