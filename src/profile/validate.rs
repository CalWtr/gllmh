//! Load a workflow's profile graph and collect its configuration violations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::paths::profile_dirs;
use crate::protocol::broker::{Edges, HelloOutput};
use crate::types::{Sender, check_name};

use super::model::*;
use super::system::Accounts;

/// A configuration error reported by both `gllmh check` and broker startup.
#[derive(Debug, thiserror::Error)]
pub enum Violation {
    #[error("{path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path:?}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Rule(String),
}

/// One validated runtime and its users, as consumed by the broker and unit generator.
#[derive(Debug, Clone)]
pub struct LoadedRuntime {
    pub runtime: RuntimeProfile,
    /// Keyed by user profile name. Paths are preserved as configured.
    pub users: BTreeMap<String, ResolvedUser>,
    /// Effective permissions, including empty entries for users and services with no edges.
    pub edges: BTreeMap<Sender, Edges>,
}

impl LoadedRuntime {
    /// Build the section 6.1 response for a user already identified by the broker.
    /// Returns `Ok(None)` when the user is outside this runtime.
    pub fn hello(&self, user: &str) -> serde_json::Result<Option<HelloOutput>> {
        let Some(profile) = self.users.get(user) else {
            return Ok(None);
        };
        let senders = std::iter::once(Sender::user(user))
            .chain(profile.services.keys().map(|service| Sender::service(user, service)));
        let edges = senders
            .map(|sender| {
                let edges = self.edges.get(&sender).cloned().unwrap_or_default();
                (sender, edges)
            })
            .collect();
        Ok(Some(HelloOutput { user: user.into(), profile: serde_json::to_value(profile)?, edges }))
    }
}

/// Load and validate the named runtime, its participating users and their referenced
/// profiles. Returns the resolved snapshot on success, or every violation found while
/// reading and checking the available profiles. This performs blocking filesystem and
/// account lookups. Dev mode skips OS account and filesystem permission checks.
pub fn validate(config_dir: &Path, name: &str, dev: bool) -> Result<LoadedRuntime, Vec<Violation>> {
    validate_with_accounts(config_dir, name, (!dev).then_some(Accounts::SYSTEM))
}

pub(super) fn validate_with_accounts(
    config_dir: &Path,
    name: &str,
    accounts: Option<Accounts>,
) -> Result<LoadedRuntime, Vec<Violation>> {
    let mut validator = Validator::new(config_dir, accounts);
    let Some(runtime) = validator.read::<RuntimeProfile>(profile_dirs::RUNTIME, name, |p| &p.name) else {
        return Err(validator.errors);
    };

    let mut user_names = BTreeSet::new();
    for name in &runtime.users {
        if !user_names.insert(name.as_str()) {
            validator.rule(format!("runtime lists user {name:?} more than once"));
        }
    }
    let users = validator.read_profiles::<UserProfile>(profile_dirs::USER, user_names, |p| &p.name);
    let composers = validator.read_profiles::<ComposerProfile>(
        profile_dirs::COMPOSER,
        users.values().filter_map(|user| user.composer_profile.as_deref()).collect(),
        |p| &p.name,
    );
    let llms = validator.read_profiles::<LlmProfile>(
        profile_dirs::LLM,
        users.values().filter_map(|user| user.llm_profiles.as_ref()).flatten().map(String::as_str).collect(),
        |p| &p.name,
    );
    let mut tool_names = BTreeSet::new();
    for selection in users.values().filter_map(|user| user.tool_profiles.as_ref()) {
        if let NameSelection::Names(names) = selection {
            tool_names.extend(names.iter().cloned());
        }
    }
    if users.values().any(|user| matches!(user.tool_profiles, Some(NameSelection::All(_)))) {
        tool_names.extend(validator.all_tool_names());
    }
    let tools = validator.read_profiles::<ToolProfile>(
        profile_dirs::TOOL,
        tool_names.iter().map(String::as_str).collect(),
        |p| &p.name,
    );

    validator.check_users(&users, &llms);
    for composer in composers.values() {
        validator.check_composer(composer);
    }
    for tool in tools.values() {
        validator.check_tool(tool);
    }
    let edges = validator.check_edges(&runtime, &users, &composers);
    if !validator.errors.is_empty() {
        return Err(validator.errors);
    }

    let users = users
        .into_iter()
        .map(|(name, user)| {
            let resolved = ResolvedUser {
                name: user.name,
                system_user: user.system_user,
                cwd: user.cwd,
                user_data_store: user.user_data_store,
                services: user.services,
                composer_profile: user.composer_profile.map(|composer| composers[&composer].clone()),
                llm_profiles: user.llm_profiles.into_iter().flatten().map(|llm| llms[&llm].clone()).collect(),
                tool_profiles: match user.tool_profiles {
                    Some(NameSelection::Names(names)) => names.iter().map(|tool| tools[tool].clone()).collect(),
                    Some(NameSelection::All(_)) => tools.values().cloned().collect(),
                    None => Vec::new(),
                },
            };
            (name, resolved)
        })
        .collect();
    Ok(LoadedRuntime { runtime, users, edges })
}

struct Validator<'a> {
    config_dir: &'a Path,
    accounts: Option<Accounts>,
    config_gid: Option<u32>,
    checked_dirs: BTreeSet<&'static str>,
    errors: Vec<Violation>,
}

impl<'a> Validator<'a> {
    fn new(config_dir: &'a Path, accounts: Option<Accounts>) -> Self {
        let mut validator =
            Self { config_dir, accounts, config_gid: None, checked_dirs: BTreeSet::new(), errors: Vec::new() };
        if let Some(accounts) = &validator.accounts {
            match (accounts.group)("gllmh") {
                Ok(Some(gid)) => validator.config_gid = Some(gid),
                Ok(None) => validator.rule("system group gllmh does not exist".into()),
                Err(err) => validator.rule(format!("could not look up system group gllmh: {err}")),
            }
            validator.check_config_directory(config_dir);
        }
        validator
    }

    fn rule(&mut self, message: String) {
        self.errors.push(Violation::Rule(message));
    }

    fn io_error(&mut self, path: PathBuf, source: io::Error) {
        self.errors.push(Violation::Io { path, source });
    }

    fn read_profiles<T: DeserializeOwned>(
        &mut self,
        kind: &'static str,
        names: BTreeSet<&str>,
        declared_name: fn(&T) -> &str,
    ) -> BTreeMap<String, T> {
        names
            .into_iter()
            .filter_map(|name| self.read(kind, name, declared_name).map(|profile| (name.to_owned(), profile)))
            .collect()
    }

    fn read<T: DeserializeOwned>(
        &mut self,
        kind: &'static str,
        name: &str,
        declared_name: fn(&T) -> &str,
    ) -> Option<T> {
        if name.is_empty() || name.contains(['/', '\0']) || matches!(name, "." | "..") {
            self.rule(format!("{kind} profile reference {name:?} is not a single file name"));
            return None;
        }
        let dir = self.config_dir.join(kind);
        if self.accounts.is_some() && self.checked_dirs.insert(kind) {
            self.check_config_directory(&dir);
        }
        let path = dir.join(format!("{name}.json"));
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                self.io_error(path, err);
                return None;
            }
        };
        if self.accounts.is_some() {
            self.check_config_metadata(&path, &metadata, false);
        }
        if !metadata.is_file() {
            if self.accounts.is_none() {
                self.rule(format!("{path:?}: expected a regular file"));
            }
            return None;
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(err) => {
                self.io_error(path, err);
                return None;
            }
        };
        match serde_json::from_slice(&data) {
            Ok(profile) => {
                if declared_name(&profile) != name {
                    self.rule(format!(
                        "{path:?}: profile name {:?} must match file name {name:?}",
                        declared_name(&profile)
                    ));
                }
                Some(profile)
            }
            Err(source) => {
                self.errors.push(Violation::Json { path, source });
                None
            }
        }
    }

    fn all_tool_names(&mut self) -> Vec<String> {
        let dir = self.config_dir.join(profile_dirs::TOOL);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                self.io_error(dir, err);
                return Vec::new();
            }
        };
        if self.accounts.is_some() && self.checked_dirs.insert(profile_dirs::TOOL) {
            self.check_config_directory(&dir);
        }
        let mut names = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let file_name = entry.file_name();
                    if let Some(name) = file_name.to_str().and_then(|name| name.strip_suffix(".json")) {
                        names.push(name.to_owned());
                    } else if Path::new(&file_name).extension().is_some_and(|ext| ext == "json") {
                        self.rule(format!("{:?}: tool profile file name must be UTF-8", entry.path()));
                    }
                }
                Err(err) => self.io_error(dir.clone(), err),
            }
        }
        names
    }

    fn check_config_directory(&mut self, path: &Path) {
        match fs::metadata(path) {
            Ok(metadata) => self.check_config_metadata(path, &metadata, true),
            Err(err) => self.io_error(path.to_owned(), err),
        }
    }

    fn check_config_metadata(&mut self, path: &Path, metadata: &Metadata, directory: bool) {
        // Section 5's 0640 is for profile files. Directories need search permission.
        let (expected_type, expected_mode, correct_type) = if directory {
            ("directory", 0o750, metadata.is_dir())
        } else {
            ("regular file", 0o640, metadata.is_file())
        };
        if !correct_type {
            self.rule(format!("{path:?}: expected a {expected_type}"));
        }
        self.check_mode(path, metadata, expected_mode);
        if metadata.uid() != 0 {
            self.rule(format!("{path:?}: expected owner root, found UID {}", metadata.uid()));
        }
        if self.config_gid.is_some_and(|gid| gid != metadata.gid()) {
            self.rule(format!("{path:?}: expected group gllmh, found GID {}", metadata.gid()));
        }
    }

    fn check_mode(&mut self, path: &Path, metadata: &Metadata, expected: u32) {
        let actual = metadata.mode() & 0o7777;
        if expected != actual {
            self.rule(format!("{path:?}: expected mode {expected:04o}, found {actual:04o}"));
        }
    }

    fn check_users(&mut self, users: &BTreeMap<String, UserProfile>, llms: &BTreeMap<String, LlmProfile>) {
        let mut names = BTreeMap::new();
        let mut uids = BTreeMap::new();
        for user in users.values() {
            if let Err(err) = check_name(&user.name) {
                self.rule(format!("user profile: {err}"));
            }
            for service in user.services.keys() {
                if let Err(err) = check_name(service) {
                    self.rule(format!("service of user {:?}: {err}", user.name));
                }
            }
            if user.composer_profile.is_none() {
                for (field, present) in
                    [("llm_profiles", user.llm_profiles.is_some()), ("tool_profiles", user.tool_profiles.is_some())]
                {
                    if present {
                        self.rule(format!("user {:?}: {field} requires composer_profile", user.name));
                    }
                }
            }
            if let Some(other) = names.get(user.system_user.as_str()) {
                self.rule(format!("users {other:?} and {:?} share system_user {:?}", user.name, user.system_user));
            } else {
                names.insert(user.system_user.as_str(), user.name.as_str());
            }

            let Some(accounts) = &self.accounts else {
                continue;
            };
            let uid = match (accounts.user)(&user.system_user) {
                Ok(Some(uid)) => {
                    let (prior_name, prior_account) =
                        *uids.entry(uid).or_insert((user.name.as_str(), user.system_user.as_str()));
                    if prior_account != user.system_user {
                        self.rule(format!(
                            "users {prior_name:?} and {:?} share UID {uid} through system_user aliases {prior_account:?} and {:?}",
                            user.name, user.system_user
                        ));
                    }
                    Some(uid)
                }
                Ok(None) => {
                    self.rule(format!(
                        "user {:?}: system_user {:?} is missing from passwd",
                        user.name, user.system_user
                    ));
                    None
                }
                Err(err) => {
                    self.rule(format!(
                        "user {:?}: could not look up system_user {:?}: {err}",
                        user.name, user.system_user
                    ));
                    None
                }
            };
            if let Some(names) = &user.llm_profiles {
                for name in names.iter().collect::<BTreeSet<_>>() {
                    if let Some(path) = llms.get(name).and_then(|llm| llm.api_key_file.as_deref()) {
                        self.check_api_key(path, user, uid);
                    }
                }
            }
        }
    }

    fn check_api_key(&mut self, path: &Path, user: &UserProfile, uid: Option<u32>) {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) => {
                self.io_error(path.to_owned(), err);
                return;
            }
        };
        if !metadata.is_file() {
            self.rule(format!("{path:?}: api_key_file must be a regular file"));
        }
        self.check_mode(path, &metadata, 0o400);
        if uid.is_some_and(|uid| uid != metadata.uid()) {
            self.rule(format!(
                "{path:?}: expected api_key_file owner {:?} for user {:?}, found UID {}",
                user.system_user,
                user.name,
                metadata.uid(),
            ));
        }
    }

    fn check_composer(&mut self, composer: &ComposerProfile) {
        if composer.engine == ComposerEngine::Custom && composer.custom_composer_bin.is_none() {
            self.rule(format!("composer {:?}: custom engine requires custom_composer_bin", composer.name));
        }
        if composer.max_parallel_turns > 1 && builtin_mode(composer) == Some(BuiltinMode::Message) {
            self.rule(format!(
                "composer {:?}: max_parallel_turns > 1 requires builtin directive mode or a custom engine",
                composer.name
            ));
        }
    }

    fn check_tool(&mut self, tool: &ToolProfile) {
        match (tool.builtin.is_some(), tool.mcp.is_some(), tool.tool_bin.is_some(), tool.tool_schema.is_some()) {
            (true, false, false, false) | (false, true, false, false) | (false, false, true, true) => {}
            _ => self.rule(format!(
                "tool {:?}: expected exactly one of builtin, mcp, or tool_bin with tool_schema",
                tool.name
            )),
        }
    }

    fn check_edges(
        &mut self,
        runtime: &RuntimeProfile,
        users: &BTreeMap<String, UserProfile>,
        composers: &BTreeMap<String, ComposerProfile>,
    ) -> BTreeMap<Sender, Edges> {
        let mut edges: BTreeMap<_, Edges> = users
            .iter()
            .flat_map(|(name, user)| {
                std::iter::once(Sender::user(name))
                    .chain(user.services.keys().map(move |service| Sender::service(name, service)))
            })
            .map(|sender| (sender, Edges::default()))
            .collect();
        for (field, configured_edges) in
            [("allow_messages", &runtime.allow_messages), ("allow_directives", &runtime.allow_directives)]
        {
            for (from, recipients) in configured_edges {
                let sender_edges = Sender::parse(from).ok().and_then(|sender| edges.get_mut(&sender));
                if sender_edges.is_none() {
                    self.rule(format!("{field}: unknown or invalid sender {from:?}"));
                }
                for recipient in recipients {
                    let Some(user) = users.get(recipient) else {
                        self.rule(format!("{field}: sender {from:?} has unknown recipient {recipient:?}"));
                        continue;
                    };
                    let composer = user.composer_profile.as_ref().and_then(|name| composers.get(name));
                    if field == "allow_directives" && user.composer_profile.is_none() {
                        self.rule(format!("{field}: recipient {recipient:?} of {from:?} needs a composer_profile"));
                    }
                    if field == "allow_messages" && composer.and_then(builtin_mode) == Some(BuiltinMode::Directive) {
                        self.rule(format!(
                            "{field}: recipient {recipient:?} of {from:?} is a builtin directive-mode agent"
                        ));
                    }
                }
                if let Some(sender_edges) = sender_edges {
                    if field == "allow_messages" {
                        sender_edges.messages.clone_from(recipients);
                    } else {
                        sender_edges.directives.clone_from(recipients);
                    }
                }
            }
        }
        edges
    }
}

fn builtin_mode(composer: &ComposerProfile) -> Option<BuiltinMode> {
    composer.builtin_settings().map(|builtin| builtin.mode)
}
