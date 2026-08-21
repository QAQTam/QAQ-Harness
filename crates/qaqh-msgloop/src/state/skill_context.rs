use qaqh_skills::{SkillActivation, SkillBodyChange, SkillCatalogSnapshot, SkillEffect};
use qaqh_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

pub const MAX_TOTAL_SKILL_TOKENS: usize = 64 * 1024;
pub const MAX_SINGLE_SKILL_TOKENS: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRuntimeState {
    Catalog,
    Requested,
    Active,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRuntimeInfo {
    pub name: String,
    pub description: String,
    pub state: SkillRuntimeState,
    pub source: String,
    pub token_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTurnSnapshot {
    pub context_epoch: u64,
    pub catalog_revision: String,
    pub catalog: String,
    pub requested_annotation: Option<String>,
    pub envelope: String,
}

#[derive(Debug, Clone)]
struct RuntimeEntry {
    activation: Option<SkillActivation>,
    state: SkillRuntimeState,
    source: String,
    activation_order: u64,
    ignored_request_laps: u8,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum UiTransition {
    Request { name: String, source: String },
}

#[derive(Debug)]
pub struct SkillContextManager {
    workspace: PathBuf,
    catalog: SkillCatalogSnapshot,
    entries: BTreeMap<String, RuntimeEntry>,
    queued_ui: VecDeque<UiTransition>,
    notices: Vec<String>,
    changes: Vec<(String, SkillBodyChange)>,
    context_epoch: u64,
    operation_revision: u64,
    activation_order: u64,
    effective_input_tokens: usize,
    frozen: Option<SkillTurnSnapshot>,
    resolved_operations: BTreeMap<String, (bool, u64, Option<String>)>,
}

impl SkillContextManager {
    pub fn new(workspace: &Path, effective_input_tokens: usize) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            catalog: SkillCatalogSnapshot::discover(workspace),
            entries: BTreeMap::new(),
            queued_ui: VecDeque::new(),
            notices: Vec::new(),
            changes: Vec::new(),
            context_epoch: 0,
            operation_revision: 0,
            activation_order: 0,
            effective_input_tokens,
            frozen: None,
            resolved_operations: BTreeMap::new(),
        }
    }

    pub fn begin_user_turn(&mut self, user_text: &str) -> SkillTurnSnapshot {
        self.refresh_catalog();
        self.apply_boundary_transitions();

        let mentions = qaqh_skills::explicit_mentions(user_text, &self.catalog.catalog);
        for metadata in mentions {
            let _ = self.request_now(&metadata.name, "user");
        }
        let snapshot = self.build_snapshot();
        self.frozen = Some(snapshot.clone());
        snapshot
    }

    pub fn set_workspace(&mut self, workspace: &Path) {
        if self.workspace == workspace {
            return;
        }
        let stable = self.session_state();
        self.workspace = workspace.to_path_buf();
        self.catalog = SkillCatalogSnapshot::discover(workspace);
        self.restore(&stable);
    }

    pub fn refresh(&mut self) {
        self.refresh_catalog();
    }

    pub fn snapshot_for_context(&mut self) -> SkillTurnSnapshot {
        self.frozen.clone().unwrap_or_else(|| self.build_snapshot())
    }

    /// The initial catalog text at discovery time, before any skills are activated.
    /// Persisted in the MessageStore on session creation so the cache prefix survives
    /// daemon restarts — the catalog sits right after the base system prompt and a
    /// different catalog on resume would invalidate the entire conversation cache.
    pub fn initial_catalog_text(&self) -> &str {
        &self.catalog.rendered
    }

    pub fn apply_tool_effect(&mut self, effect: SkillEffect) -> Result<(), String> {
        match effect {
            SkillEffect::Activate(activation) => self.activate(activation, "model"),
        }?;
        self.refresh_frozen_envelope();
        Ok(())
    }

    /// Called after a model lap. The first ignored requested skill adds an
    /// authoritative reminder; the second is activated by the host.
    pub fn complete_model_lap(&mut self) -> Result<Vec<String>, String> {
        let requested = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == SkillRuntimeState::Requested)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let mut forced = Vec::new();
        for name in requested {
            let ignored = self
                .entries
                .get(&name)
                .map_or(0, |entry| entry.ignored_request_laps);
            if ignored == 0 {
                if let Some(entry) = self.entries.get_mut(&name) {
                    entry.ignored_request_laps = 1;
                }
            } else {
                let activation = self.load_named(&name)?;
                self.activate(activation, "user_forced")?;
                forced.push(name);
            }
        }
        self.refresh_frozen_envelope();
        Ok(forced)
    }

    pub fn complete_user_turn(&mut self) {
        // 自动卸载已删除（产品决策）：skill 激活后保持注入直到显式卸载，
        // 避免长程任务后的下一轮系统提示词变化破坏缓存命中。
        self.frozen = None;
        self.operation_revision = self.operation_revision.saturating_add(1);
    }

    pub fn abort_user_turn(&mut self) {
        self.frozen = None;
    }

    pub fn queue_request(&mut self, name: &str, source: &str) -> Result<(), String> {
        self.ensure_known(name)?;
        if self.frozen.is_some() {
            self.queued_ui.push_back(UiTransition::Request {
                name: name.to_string(),
                source: source.to_string(),
            });
            return Ok(());
        }
        self.request_now(name, source)
    }

    pub fn apply_ui_operation(
        &mut self,
        operation_id: &str,
        expected_revision: u64,
        action: &str,
        name: &str,
    ) -> (bool, u64, Option<String>) {
        if let Some(resolved) = self.resolved_operations.get(operation_id) {
            return resolved.clone();
        }
        let result = if expected_revision != self.operation_revision {
            Err(format!(
                "SKILL_OPERATION_STALE: expected {expected_revision}, current {}",
                self.operation_revision
            ))
        } else {
            match action {
                "request" | "activate" => self.queue_request(name, "user"),
                _ => Err(format!("SKILL_INVALID_ACTION: '{action}'")),
            }
        };
        let resolved = (result.is_ok(), self.operation_revision, result.err());
        if self.resolved_operations.len() >= 256
            && let Some(first) = self.resolved_operations.keys().next().cloned()
        {
            self.resolved_operations.remove(&first);
        }
        self.resolved_operations
            .insert(operation_id.to_string(), resolved.clone());
        resolved
    }

    pub fn turn_snapshot(&self) -> Option<&SkillTurnSnapshot> {
        self.frozen.as_ref()
    }

    pub fn catalog_snapshot(&self) -> &SkillCatalogSnapshot {
        &self.catalog
    }

    pub fn context_epoch(&self) -> u64 {
        self.context_epoch
    }

    /// True when at least one skill is activated with a loaded body.
    /// Used by the injection gate: an empty active set injects nothing.
    pub fn has_active(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.state == SkillRuntimeState::Active && entry.activation.is_some())
    }

    pub fn operation_revision(&self) -> u64 {
        self.operation_revision
    }

    pub fn has_requested(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.state == SkillRuntimeState::Requested)
    }

    pub fn token_budget(&self) -> usize {
        (self.effective_input_tokens / 10).min(MAX_TOTAL_SKILL_TOKENS)
    }

    pub fn token_usage(&self) -> usize {
        self.entries
            .values()
            .filter_map(|entry| entry.activation.as_ref())
            .map(activation_tokens)
            .sum()
    }

    pub fn runtime_info(&self) -> Vec<SkillRuntimeInfo> {
        let mut states = self.entries.clone();
        for skill in &self.catalog.catalog.skills {
            states.entry(skill.name.clone()).or_insert(RuntimeEntry {
                activation: None,
                state: SkillRuntimeState::Catalog,
                source: "catalog".into(),
                activation_order: 0,
                ignored_request_laps: 0,
                error: None,
            });
        }
        for diagnostic in &self.catalog.catalog.diagnostics {
            if diagnostic.severity != qaqh_skills::DiagnosticSeverity::Error {
                continue;
            }
            let name = diagnostic
                .path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .unwrap_or("unknown-skill")
                .to_string();
            states.entry(name).or_insert(RuntimeEntry {
                activation: None,
                state: SkillRuntimeState::Unavailable,
                source: diagnostic.path.to_string_lossy().into_owned(),
                activation_order: 0,
                ignored_request_laps: 0,
                error: Some(diagnostic.message.clone()),
            });
        }
        states
            .into_iter()
            .map(|(name, entry)| {
                let metadata = entry
                    .activation
                    .as_ref()
                    .map(|activation| &activation.metadata)
                    .or_else(|| {
                        self.catalog
                            .catalog
                            .skills
                            .iter()
                            .find(|skill| skill.name == name)
                    });
                let catalog_only = entry.source == "catalog";
                SkillRuntimeInfo {
                    name,
                    description: metadata.map_or_else(String::new, |item| item.description.clone()),
                    state: if catalog_only {
                        SkillRuntimeState::Catalog
                    } else {
                        entry.state
                    },
                    source: entry.source,
                    token_count: entry.activation.as_ref().map_or(0, activation_tokens),
                    error: entry.error,
                }
            })
            .collect()
    }

    pub fn session_state(&self) -> SkillSessionStateV2 {
        let mut entries = self
            .entries
            .iter()
            .filter_map(|(name, entry)| {
                let state = match entry.state {
                    SkillRuntimeState::Active => SkillSessionEntryState::Active,
                    SkillRuntimeState::Unavailable => SkillSessionEntryState::Unavailable,
                    SkillRuntimeState::Requested | SkillRuntimeState::Catalog => return None,
                };
                Some(SkillSessionEntry {
                    name: name.clone(),
                    activation_order: entry.activation_order,
                    source: entry.source.clone(),
                    state,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.activation_order);
        SkillSessionStateV2 {
            version: 2,
            context_epoch: self.context_epoch,
            operation_revision: self.operation_revision,
            entries,
        }
    }

    pub fn restore(&mut self, state: &SkillSessionStateV2) {
        self.entries.clear();
        self.resolved_operations.clear();
        self.activation_order = 0;
        self.context_epoch = state.context_epoch;
        self.operation_revision = state.operation_revision;
        let mut ordered = state.entries.clone();
        ordered.sort_by_key(|entry| entry.activation_order);
        for saved in ordered {
            self.activation_order = self.activation_order.max(saved.activation_order);
            match self.load_named(&saved.name) {
                Ok(activation) => {
                    let _ =
                        self.activate_with_order(activation, &saved.source, saved.activation_order);
                }
                Err(error) => {
                    self.entries.insert(
                        saved.name,
                        RuntimeEntry {
                            activation: None,
                            state: SkillRuntimeState::Unavailable,
                            source: saved.source,
                            activation_order: saved.activation_order,
                            ignored_request_laps: 0,
                            error: Some(error),
                        },
                    );
                }
            }
        }
        // Loading stable state is not a new runtime operation. The first turn
        // receives exactly the persisted authoritative revisions.
        self.context_epoch = state.context_epoch;
        self.operation_revision = state.operation_revision;
    }

    fn request_now(&mut self, name: &str, source: &str) -> Result<(), String> {
        self.ensure_known(name)?;
        match self.entries.get(name).map(|entry| &entry.state) {
            Some(SkillRuntimeState::Active) => return Ok(()),
            _ => {}
        }
        self.entries.insert(
            name.to_string(),
            RuntimeEntry {
                activation: None,
                state: SkillRuntimeState::Requested,
                source: source.to_string(),
                activation_order: 0,
                ignored_request_laps: 0,
                error: None,
            },
        );
        self.operation_revision = self.operation_revision.saturating_add(1);
        Ok(())
    }

    fn activate(&mut self, activation: SkillActivation, source: &str) -> Result<(), String> {
        self.activation_order = self.activation_order.saturating_add(1);
        self.activate_with_order(activation, source, self.activation_order)
    }

    fn activate_with_order(
        &mut self,
        activation: SkillActivation,
        source: &str,
        order: u64,
    ) -> Result<(), String> {
        let name = activation.metadata.name.clone();
        let tokens = activation_tokens(&activation);
        if tokens > MAX_SINGLE_SKILL_TOKENS {
            return Err(format!(
                "SKILL_BUDGET_SINGLE: '{name}' requires {tokens} tokens"
            ));
        }
        let current = self
            .entries
            .get(&name)
            .and_then(|entry| entry.activation.as_ref())
            .map_or(0, activation_tokens);
        let projected = self
            .token_usage()
            .saturating_sub(current)
            .saturating_add(tokens);
        if projected > self.token_budget() {
            self.reclaim_budget(projected - self.token_budget(), Some(&name));
        }
        let projected = self
            .token_usage()
            .saturating_sub(current)
            .saturating_add(tokens);
        if projected > self.token_budget() {
            return Err(format!(
                "SKILL_BUDGET_TOTAL: '{name}' would use {projected}/{} tokens",
                self.token_budget()
            ));
        }
        self.entries.insert(
            name,
            RuntimeEntry {
                activation: Some(activation),
                state: SkillRuntimeState::Active,
                source: source.to_string(),
                activation_order: order,
                ignored_request_laps: 0,
                error: None,
            },
        );
        self.context_epoch = self.context_epoch.saturating_add(1);
        self.operation_revision = self.operation_revision.saturating_add(1);
        Ok(())
    }

    fn reclaim_budget(&mut self, mut required: usize, except: Option<&str>) {
        let mut candidates = self
            .entries
            .iter()
            .filter(|(name, entry)| entry.activation.is_some() && except != Some(name.as_str()))
            .map(|(name, entry)| (name.clone(), entry.activation_order))
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        for (name, _) in candidates {
            if required == 0 {
                break;
            }
            if let Some(entry) = self.entries.remove(&name) {
                required =
                    required.saturating_sub(entry.activation.as_ref().map_or(0, activation_tokens));
                self.notices
                    .push(format!("{name} 已因 skill 预算回收而移除"));
                self.context_epoch = self.context_epoch.saturating_add(1);
            }
        }
    }

    fn apply_boundary_transitions(&mut self) {
        while let Some(transition) = self.queued_ui.pop_front() {
            let result = match transition {
                UiTransition::Request { name, source } => self.request_now(&name, &source),
            };
            if let Err(error) = result {
                self.notices.push(error);
            }
        }
    }

    fn refresh_catalog(&mut self) {
        let next = SkillCatalogSnapshot::discover(&self.workspace);
        if next.fingerprint != self.catalog.fingerprint || next.catalog != self.catalog.catalog {
            self.catalog = next;
        }
        let active = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.activation.is_some())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in active {
            let old = self
                .entries
                .get(&name)
                .and_then(|entry| entry.activation.clone());
            match self.load_named(&name) {
                Ok(new) => {
                    if let Some(old) = old
                        && old.body != new.body
                    {
                        self.changes.push((
                            name.clone(),
                            qaqh_skills::describe_body_change(&old.body, &new.body, 200),
                        ));
                        if let Some(entry) = self.entries.get_mut(&name) {
                            entry.activation = Some(new);
                            entry.state = SkillRuntimeState::Active;
                        }
                        self.context_epoch = self.context_epoch.saturating_add(1);
                    }
                }
                Err(error) => {
                    self.entries.remove(&name);
                    self.notices.push(format!("{name} 已移除: {error}"));
                    self.context_epoch = self.context_epoch.saturating_add(1);
                }
            }
        }
    }

    fn build_snapshot(&mut self) -> SkillTurnSnapshot {
        SkillTurnSnapshot {
            context_epoch: self.context_epoch,
            catalog_revision: self.catalog.fingerprint.clone(),
            catalog: self.catalog.rendered.clone(),
            requested_annotation: self.requested_annotation(),
            envelope: self.render_envelope(),
        }
    }

    fn refresh_frozen_envelope(&mut self) {
        let annotation = self.requested_annotation();
        let envelope = self.render_envelope();
        if let Some(snapshot) = &mut self.frozen {
            snapshot.requested_annotation = annotation;
            snapshot.envelope = envelope;
        }
    }

    fn requested_annotation(&self) -> Option<String> {
        let requested = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == SkillRuntimeState::Requested)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        (!requested.is_empty()).then(|| format!(
            "<requested_skills>\nUser requested: {}. Call the fixed `skills` tool with action=`activate` and the exact name.\n</requested_skills>",
            requested.join(", ")
        ))
    }

    fn render_envelope(&mut self) -> String {
        let mut output = format!(
            "<skill_context_envelope version=\"2\" epoch=\"{}\">\nThis is the complete authoritative active skill set. It replaces all older skill instructions.\n",
            self.context_epoch
        );
        let mut active = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, SkillRuntimeState::Active))
            .filter_map(|(name, entry)| {
                entry
                    .activation
                    .as_ref()
                    .map(|activation| (entry.activation_order, name, activation))
            })
            .collect::<Vec<_>>();
        active.sort_by_key(|item| item.0);
        if active.is_empty() {
            output.push_str("<active_skills />\n");
        } else {
            output.push_str("<active_skills>\n");
            for (_, name, activation) in active {
                output.push_str(&format!(
                    "<skill name=\"{}\" hash=\"{}\">\n{}\n</skill>\n",
                    name,
                    qaqh_skills::content_hash(&activation.body),
                    qaqh_skills::render_activation(activation)
                ));
            }
            output.push_str("</active_skills>\n");
        }
        for (name, change) in self.changes.drain(..) {
            if let Some(diff) = change.diff {
                output.push_str(&format!("<skill_updated name=\"{name}\" old_hash=\"{}\" new_hash=\"{}\"><![CDATA[{diff}]]></skill_updated>\n", change.old_hash, change.new_hash));
            } else {
                output.push_str(&format!("<skill_replaced name=\"{name}\" old_hash=\"{}\" new_hash=\"{}\" added=\"{}\" removed=\"{}\" />\n", change.old_hash, change.new_hash, change.added_lines, change.removed_lines));
            }
        }
        for notice in self.notices.drain(..) {
            output.push_str(&format!("<notice>{notice}</notice>\n"));
        }
        let reminded = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.state == SkillRuntimeState::Requested && entry.ignored_request_laps > 0
            })
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        if !reminded.is_empty() {
            output.push_str(&format!(
                "<activation_required>Activate requested skills now: {}</activation_required>\n",
                reminded.join(", ")
            ));
        }
        output.push_str("</skill_context_envelope>");
        output
    }

    fn load_named(&self, name: &str) -> Result<SkillActivation, String> {
        let metadata = self
            .catalog
            .catalog
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| format!("SKILL_NOT_FOUND: '{name}'"))?;
        qaqh_skills::load(metadata)
    }

    fn ensure_known(&self, name: &str) -> Result<(), String> {
        self.catalog
            .catalog
            .skills
            .iter()
            .any(|skill| skill.name == name)
            .then_some(())
            .ok_or_else(|| format!("SKILL_NOT_FOUND: '{name}'"))
    }
}

fn activation_tokens(activation: &SkillActivation) -> usize {
    // Deterministic conservative estimate. Provider tokenizers may refine this
    // later, but budgeting never truncates the body.
    qaqh_skills::token_count(&qaqh_skills::render_activation(activation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (tempfile::TempDir, SkillContextManager) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".agents/skills/alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: alpha\ndescription: Alpha workflow.\n---\n\nALPHA_BODY",
        )
        .unwrap();
        let manager = SkillContextManager::new(temp.path(), 100_000);
        (temp, manager)
    }

    #[test]
    fn request_is_annotation_only_until_typed_activation() {
        let (_temp, mut manager) = manager();
        let snapshot = manager.begin_user_turn("use $alpha");
        assert!(snapshot.requested_annotation.unwrap().contains("alpha"));
        assert!(!snapshot.envelope.contains("ALPHA_BODY"));

        let activation = manager.load_named("alpha").unwrap();
        manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap();
        assert!(
            manager
                .turn_snapshot()
                .unwrap()
                .envelope
                .contains("ALPHA_BODY")
        );
    }

    #[test]
    fn activated_skill_stays_active_after_many_turns() {
        // 自动卸载已删除：skill 激活后保持 Active，直到显式卸载。
        let (_temp, mut manager) = manager();
        manager.begin_user_turn("use $alpha");
        let activation = manager.load_named("alpha").unwrap();
        manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap();
        manager.abort_user_turn();
        for _ in 0..10 {
            manager.begin_user_turn("continue");
            manager.complete_user_turn();
        }
        assert!(
            manager
                .runtime_info()
                .iter()
                .any(|item| item.name == "alpha" && item.state == SkillRuntimeState::Active),
            "skill must remain active across turns (no auto-unload)"
        );
        let snapshot = manager.begin_user_turn("continue");
        assert!(
            snapshot.envelope.contains("ALPHA_BODY"),
            "skill body must stay in the injected context (cache stability)"
        );
    }

    #[test]
    fn ignored_request_is_forced_on_second_model_lap() {
        let (_temp, mut manager) = manager();
        manager.begin_user_turn("use $alpha");
        assert!(manager.complete_model_lap().unwrap().is_empty());
        assert_eq!(manager.complete_model_lap().unwrap(), vec!["alpha"]);
        assert!(
            manager
                .turn_snapshot()
                .unwrap()
                .envelope
                .contains("source: ")
        );
        assert_eq!(manager.session_state().entries[0].source, "user_forced");
    }

    #[test]
    fn session_restore_reloads_latest_body() {
        let (temp, mut manager) = manager();
        manager.begin_user_turn("use $alpha");
        let activation = manager.load_named("alpha").unwrap();
        manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap();
        manager.complete_user_turn();
        let state = manager.session_state();
        std::fs::write(
            temp.path().join(".agents/skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: Alpha workflow.\n---\n\nLATEST_BODY",
        )
        .unwrap();

        let mut restored = SkillContextManager::new(temp.path(), 100_000);
        restored.restore(&state);
        let snapshot = restored.begin_user_turn("continue");
        assert!(snapshot.envelope.contains("LATEST_BODY"));
    }

    #[test]
    fn skill_survives_many_turn_boundaries_without_unloading() {
        let (_temp, mut manager) = manager();
        manager.begin_user_turn("use $alpha");
        let activation = manager.load_named("alpha").unwrap();
        manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap();
        for _ in 0..5 {
            manager.complete_user_turn();
            manager.begin_user_turn("continue");
        }
        let next = manager.begin_user_turn("continue");
        assert!(
            next.envelope.contains("ALPHA_BODY"),
            "no auto-unload: skill must stay in context"
        );
    }

    #[test]
    fn active_body_hot_update_emits_small_diff() {
        let (temp, mut manager) = manager();
        manager.begin_user_turn("use $alpha");
        let activation = manager.load_named("alpha").unwrap();
        manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap();
        manager.complete_user_turn();
        std::fs::write(
            temp.path().join(".agents/skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: Alpha workflow.\n---\n\nALPHA_CHANGED",
        )
        .unwrap();
        let next = manager.begin_user_turn("continue");
        assert!(next.envelope.contains("ALPHA_CHANGED"));
        assert!(next.envelope.contains("skill_updated"));
    }

    #[test]
    fn activation_over_budget_is_rejected_without_truncation() {
        let (temp, _) = manager();
        let mut manager = SkillContextManager::new(temp.path(), 100);
        manager.begin_user_turn("continue");
        let activation = manager.load_named("alpha").unwrap();
        let error = manager
            .apply_tool_effect(SkillEffect::Activate(activation))
            .unwrap_err();
        assert!(error.contains("SKILL_BUDGET_TOTAL"));
        assert!(
            !manager
                .turn_snapshot()
                .unwrap()
                .envelope
                .contains("ALPHA_BODY")
        );
    }

    #[test]
    fn duplicate_ui_operation_id_is_idempotent_and_stale_revision_is_structured() {
        let (_temp, mut manager) = manager();
        let first = manager.apply_ui_operation("op-1", 0, "request", "alpha");
        let duplicate = manager.apply_ui_operation("op-1", 0, "request", "alpha");
        assert_eq!(first, duplicate);
        assert_eq!(manager.operation_revision(), 1);
        let stale = manager.apply_ui_operation("op-2", 0, "release", "alpha");
        assert!(!stale.0);
        assert!(
            stale
                .2
                .as_deref()
                .is_some_and(|error| error.contains("SKILL_OPERATION_STALE"))
        );
    }
}
