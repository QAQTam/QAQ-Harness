//! Agent Skill runtime: discovery, parsing, catalog rendering, and activation.
//!
//! The catalog exposes only metadata; the body is read only on explicit mention
//! or `skills(action=activate)`.
//!
//! ## Key concepts
//!
//! - **Discovery**: scans project and user directories for `SKILL.md` files
//! - **Activation**: reads the skill body and injects it into the agent context
//! - **Catalog**: lightweight metadata index for browsing without loading bodies
//! - **Scope**: each skill belongs to either `Project` or `User` scope

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Magic marker that identifies a SKILL.md file as an active skill definition.
pub const ACTIVATION_MARKER: &str = "[QAQH_SKILL_V1]";
/// Maximum skill body size in bytes (512 KB). Larger files are rejected.
pub const MAX_SKILL_BYTES: u64 = 512 * 1024;
const MAX_SCAN_DEPTH: usize = 6;
const MAX_SCANNED_DIRS: usize = 2_000;
const MAX_SKILLS: usize = 256;
const MAX_RESOURCES: usize = 200;
const MAX_CATALOG_CHARS: usize = 8_000;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1_024;
const MAX_COMPATIBILITY_CHARS: usize = 500;

/// Scope of a skill: where it was discovered.
///
/// Project skills live in the workspace and are shared via version control.
/// User skills live in the home directory and are personal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// Workspace-relative directory (`.deepx/skills`, `.agents/skills`, `skills`).
    Project,
    /// User home directory (`~/.deepx/skills`, `~/.agents/skills`).
    User,
}

/// Lightweight metadata from a SKILL.md file for catalog listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Agent Skills experimental field. It never grants permissions by itself;
    /// QAQ-Harness always intersects it with the active permission policy.
    pub allowed_tools: Vec<String>,
    pub path: PathBuf,
    pub scope: SkillScope,
}

/// Skill diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Non-fatal issue (e.g. deprecated field, oversized name).
    Warning,
    /// Fatal issue preventing the skill from loading.
    Error,
}

/// A diagnostic produced during skill discovery or validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// Complete result of skill discovery (metadata index + diagnostics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl SkillCatalog {
    /// Semantic search over available skills.
    ///
    /// Returns matching skills sorted by keyword relevance over skill name and
    /// description.
    ///
    /// `top_k` is a hint; the actual count may be lower if fewer skills match.
    pub fn search_semantic(&self, query: &str, top_k: usize) -> Vec<SkillMetadata> {
        if query.is_empty() || self.skills.is_empty() {
            return Vec::new();
        }

        self.search_semantic_keyword(query, top_k)
    }

    /// Keyword-based fallback: score skills by token overlap with the query.
    fn search_semantic_keyword(&self, query: &str, top_k: usize) -> Vec<SkillMetadata> {
        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower
            .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
            .filter(|s| !s.is_empty())
            .collect();

        let mut scored: Vec<(usize, usize)> = self
            .skills
            .iter()
            .enumerate()
            .map(|(i, skill)| {
                let text = format!(
                    "{} {} {}",
                    skill.name,
                    skill.description,
                    skill.description // double-weight description
                )
                .to_lowercase();
                let hits = query_tokens.iter().filter(|t| text.contains(*t)).count();
                (i, hits)
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let k = top_k.min(scored.len());

        scored[..k]
            .iter()
            .filter(|(_, score)| *score > 0)
            .map(|(idx, _)| self.skills[*idx].clone())
            .collect()
    }
}

/// A fully loaded skill ready for injection into the agent context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActivation {
    pub metadata: SkillMetadata,
    pub body: String,
    pub resources: Vec<PathBuf>,
}

/// A bundled resource file within a skill directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillResource {
    pub skill_name: String,
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

#[derive(Debug)]
struct ParsedMetadata {
    metadata: SkillMetadata,
    warnings: Vec<String>,
}

/// 动态发现当前工作区和用户目录中的 skills。
///
/// 优先级固定为：项目 `.deepx` > `.agents` > `skills`，随后是用户
/// `.deepx` > `.agents`。同名 skill 只保留优先级最高者。
pub fn discover(workspace: &Path) -> SkillCatalog {
    let workspace = absolutize(workspace);
    let mut roots = vec![
        (workspace.join(".deepx/skills"), SkillScope::Project),
        (workspace.join(".agents/skills"), SkillScope::Project),
        (workspace.join("skills"), SkillScope::Project),
    ];
    if let Some(home) = home_dir() {
        roots.push((home.join(".deepx/skills"), SkillScope::User));
        roots.push((home.join(".agents/skills"), SkillScope::User));
    }
    discover_roots(&roots)
}

fn discover_roots(roots: &[(PathBuf, SkillScope)]) -> SkillCatalog {
    let mut catalog = SkillCatalog::default();
    let mut seen_names: HashMap<String, PathBuf> = HashMap::new();
    let mut scanned_dirs = 0usize;

    for (root, scope) in roots {
        scan_root(
            root,
            *scope,
            &mut catalog,
            &mut seen_names,
            &mut scanned_dirs,
        );
        if scanned_dirs >= MAX_SCANNED_DIRS || catalog.skills.len() >= MAX_SKILLS {
            break;
        }
    }
    catalog
}

fn scan_root(
    root: &Path,
    scope: SkillScope,
    catalog: &mut SkillCatalog,
    seen_names: &mut HashMap<String, PathBuf>,
    scanned_dirs: &mut usize,
) {
    if !root.is_dir() {
        return;
    }
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if *scanned_dirs >= MAX_SCANNED_DIRS || catalog.skills.len() >= MAX_SKILLS {
            return;
        }
        *scanned_dirs += 1;

        let mut entries = match fs::read_dir(&dir) {
            Ok(entries) => entries.flatten().collect::<Vec<_>>(),
            Err(error) => {
                catalog.diagnostics.push(SkillDiagnostic {
                    path: dir,
                    severity: DiagnosticSeverity::Error,
                    message: format!("cannot scan directory: {error}"),
                });
                continue;
            }
        };
        entries.sort_by_key(|entry| entry.file_name());
        entries.reverse();

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if !file_type.is_dir() || file_type.is_symlink() || should_skip_dir(&name) {
                continue;
            }

            let skill_path = path.join("SKILL.md");
            if skill_path.is_file() {
                match parse_metadata(&skill_path, scope) {
                    Ok(parsed) => {
                        let skill = parsed.metadata;
                        for message in parsed.warnings {
                            catalog.diagnostics.push(SkillDiagnostic {
                                path: skill.path.clone(),
                                severity: DiagnosticSeverity::Warning,
                                message,
                            });
                        }
                        if let Some(selected) = seen_names.get(&skill.name) {
                            catalog.diagnostics.push(SkillDiagnostic {
                                path: skill.path.clone(),
                                severity: DiagnosticSeverity::Warning,
                                message: format!(
                                    "skill '{}' shadowed by {}",
                                    skill.name,
                                    selected.display()
                                ),
                            });
                        } else {
                            seen_names.insert(skill.name.clone(), skill.path.clone());
                            catalog.skills.push(skill);
                        }
                    }
                    Err(message) => catalog.diagnostics.push(SkillDiagnostic {
                        path: skill_path,
                        severity: DiagnosticSeverity::Error,
                        message,
                    }),
                }
            }
            if depth < MAX_SCAN_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
}

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | ".git")
}

fn parse_metadata(path: &Path, scope: SkillScope) -> Result<ParsedMetadata, String> {
    let raw = read_bounded(path)?;
    let (frontmatter, _) = split_skill_file(&raw)?;
    let parsed = parse_frontmatter(frontmatter)?;
    let name = parsed.name.trim().to_string();
    let description = parsed.description.trim().to_string();
    if description.is_empty() {
        return Err("missing skill description".into());
    }
    let mut warnings =
        validate_standard_fields(path, &name, &description, parsed.compatibility.as_deref());
    if name.is_empty() {
        return Err("missing skill name".into());
    }
    if let Some(parent_name) = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        && parent_name != name
    {
        warnings.push(format!(
            "name '{name}' does not match parent directory '{parent_name}'"
        ));
    }
    Ok(ParsedMetadata {
        metadata: SkillMetadata {
            name,
            description,
            license: parsed
                .license
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            compatibility: parsed
                .compatibility
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            metadata: parsed.metadata,
            allowed_tools: parsed
                .allowed_tools
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            path: absolutize(path),
            scope,
        },
        warnings,
    })
}

fn validate_standard_fields(
    _path: &Path,
    name: &str,
    description: &str,
    compatibility: Option<&str>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let name_chars = name.chars().count();
    if name_chars == 0 || name_chars > MAX_NAME_CHARS {
        warnings.push(format!("name must contain 1-{MAX_NAME_CHARS} characters"));
    }
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_chars || name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        warnings.push(
            "name is not Agent Skills compliant (lowercase letters, digits, single hyphens only)"
                .into(),
        );
    }
    let description_chars = description.chars().count();
    if description_chars == 0 || description_chars > MAX_DESCRIPTION_CHARS {
        warnings.push(format!(
            "description must contain 1-{MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if compatibility.is_some_and(|value| value.chars().count() > MAX_COMPATIBILITY_CHARS) {
        warnings.push(format!(
            "compatibility exceeds {MAX_COMPATIBILITY_CHARS} characters"
        ));
    }
    warnings
}

fn parse_frontmatter(frontmatter: &str) -> Result<Frontmatter, String> {
    serde_saphyr::from_str(frontmatter).map_err(|error| format!("invalid YAML frontmatter: {error}"))
}

/// Parse the YAML frontmatter boundary. Only `---\n` start is accepted;
/// legacy `[SKILLS]` prefix and any other content before the frontmatter
/// are rejected so that incompatible skills are skipped.
fn split_skill_file(raw: &str) -> Result<(&str, &str), String> {
    let normalized = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if !normalized.starts_with("---\n") {
        return Err("SKILL.md must start with YAML frontmatter (---)".into());
    }
    let start = 0;
    let after_open = start + 4;
    let remainder = normalized
        .get(after_open..)
        .ok_or("invalid YAML frontmatter opening boundary")?;
    let close = remainder
        .find("\n---\n")
        .or_else(|| remainder.strip_suffix("\n---").map(|body| body.len()))
        .ok_or("unclosed YAML frontmatter")?;
    let body_start = after_open + close + 5;
    let body = normalized.get(body_start..).unwrap_or("").trim();
    let frontmatter = remainder
        .get(..close)
        .ok_or("invalid YAML frontmatter closing boundary")?;
    Ok((frontmatter, body))
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("cannot stat file: {error}"))?;
    if metadata.len() > MAX_SKILL_BYTES {
        return Err(format!("skill file exceeds {} bytes", MAX_SKILL_BYTES));
    }
    fs::read_to_string(path)
        .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(|error| format!("cannot read skill: {error}"))
}

/// Load and activate a skill by name.
///
/// Discovers all skills in the workspace, finds the one with the given name,
/// and reads its full body. Returns an error if the skill is not found or
/// cannot be parsed.
pub fn load_named(workspace: &Path, name: &str) -> Result<SkillActivation, String> {
    let catalog = discover(workspace);
    let metadata = catalog
        .skills
        .into_iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| format!("unknown skill '{name}'"))?;
    load(&metadata)
}

/// Validate one SKILL.md against the portable Agent Skills specification.
/// Compatibility parsing remains available during discovery, but every
/// standards violation is returned as an error here.
pub fn validate_file(path: &Path) -> Vec<SkillDiagnostic> {
    match parse_metadata(path, SkillScope::Project) {
        Ok(parsed) => parsed
            .warnings
            .into_iter()
            .map(|message| SkillDiagnostic {
                path: absolutize(path),
                severity: DiagnosticSeverity::Error,
                message,
            })
            .collect(),
        Err(message) => vec![SkillDiagnostic {
            path: absolutize(path),
            severity: DiagnosticSeverity::Error,
            message,
        }],
    }
}

/// Read the body of a skill from its SKILL.md file and construct an activation.
pub fn load(metadata: &SkillMetadata) -> Result<SkillActivation, String> {
    let raw = read_bounded(&metadata.path)?;
    let (_, body) = split_skill_file(&raw)?;
    let dir = metadata.path.parent().unwrap_or(Path::new("."));
    Ok(SkillActivation {
        metadata: metadata.clone(),
        body: body.to_string(),
        resources: list_resources(dir),
    })
}

/// Read a bundled text resource while enforcing containment in the selected
/// skill directory. Absolute paths, parent traversal, directories, symlink
/// escapes, binary data, and oversized files are rejected.
pub fn read_resource(
    workspace: &Path,
    skill_name: &str,
    relative_path: &Path,
) -> Result<SkillResource, String> {
    if relative_path.as_os_str().is_empty()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("resource path must be a non-empty relative path without '..'".into());
    }
    let activation = load_named(workspace, skill_name)?;
    let root = activation
        .metadata
        .path
        .parent()
        .ok_or("skill has no parent directory")?
        .canonicalize()
        .map_err(|error| format!("cannot resolve skill directory: {error}"))?;
    let candidate = root.join(relative_path);
    let resolved = candidate
        .canonicalize()
        .map_err(|error| format!("cannot resolve skill resource: {error}"))?;
    if !resolved.starts_with(&root) {
        return Err("skill resource escapes its skill directory".into());
    }
    if !resolved.is_file() {
        return Err("skill resource is not a file".into());
    }
    let content = read_bounded(&resolved)?;
    Ok(SkillResource {
        skill_name: activation.metadata.name,
        relative_path: relative_path.to_path_buf(),
        absolute_path: resolved,
        content,
    })
}

/// Return the owning discovered skill when a generic file tool targets its
/// SKILL.md or any bundled resource.
pub fn managed_skill_for_path(workspace: &Path, candidate: &Path) -> Option<String> {
    let resolved = candidate.canonicalize().ok()?;
    let workspace = absolutize(workspace);
    let mut roots = vec![
        workspace.join(".deepx/skills"),
        workspace.join(".agents/skills"),
        workspace.join("skills"),
    ];
    if let Some(home) = home_dir() {
        roots.push(home.join(".deepx/skills"));
        roots.push(home.join(".agents/skills"));
    }
    for root in roots {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&root) {
            continue;
        }
        let mut directory = if resolved.is_dir() {
            resolved.clone()
        } else {
            resolved.parent()?.to_path_buf()
        };
        loop {
            let skill_file = directory.join("SKILL.md");
            if skill_file.is_file() {
                return parse_metadata(&skill_file, SkillScope::Project)
                    .map(|parsed| parsed.metadata.name)
                    .ok()
                    .or_else(|| directory.file_name()?.to_str().map(str::to_string));
            }
            if directory == root || !directory.pop() {
                break;
            }
        }
    }
    None
}

fn list_resources(root: &Path) -> Vec<PathBuf> {
    let mut resources = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match fs::read_dir(&dir) {
            Ok(entries) => entries.flatten().collect::<Vec<_>>(),
            Err(_) => continue,
        };
        entries.sort_by_key(|entry| entry.file_name());
        entries.reverse();
        for entry in entries {
            if resources.len() >= MAX_RESOURCES {
                return resources;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if !should_skip_dir(&entry.file_name().to_string_lossy()) {
                    stack.push(path);
                }
            } else if entry.file_name() != "SKILL.md"
                && let Ok(relative) = path.strip_prefix(root)
            {
                resources.push(relative.to_path_buf());
            }
        }
    }
    resources.sort();
    resources
}

/// Render a catalog into the injection text format displayed in the system prompt.
///
/// Truncates descriptions at 600 chars and the total catalog at `MAX_CATALOG_CHARS`.
pub fn render_catalog(catalog: &SkillCatalog) -> String {
    if catalog.skills.is_empty() {
        return String::new();
    }
    let mut output = String::from(
        "Skills use progressive disclosure. Match a task against name and description only; do not infer the body. For an implicit match or a user-requested skill, call `skills` with action=`activate` and the exact name before acting — the full instructions are returned in the tool result. Load bundled files on demand with action=`resource`, the same name, and a manifest-relative path; generic read/search cannot access managed skill files. Use `list` for catalog diagnostics and `validate` for portability checks.\n\n<available_skills>\n",
    );
    let mut omitted = 0usize;
    for skill in &catalog.skills {
        let description: String = skill.description.chars().take(600).collect();
        let line = format!(
            "- {}: {}\n",
            skill.name,
            description.replace(['\r', '\n'], " "),
        );
        if output.chars().count() + line.chars().count() > MAX_CATALOG_CHARS {
            omitted += 1;
        } else {
            output.push_str(&line);
        }
    }
    if omitted > 0 {
        output.push_str(&format!(
            "- ... {omitted} skills omitted by catalog budget\n"
        ));
    }
    output.push_str("</available_skills>");
    output
}

/// Render an activated skill as the full context injection block.
///
/// Produces the `[QAQH_SKILL_V1] ... [END_QAQH_SKILL_V1]` envelope with
/// metadata header, body, compatibility section, and resource listing.
pub fn render_activation(activation: &SkillActivation) -> String {
    let dir = activation.metadata.path.parent().unwrap_or(Path::new("."));
    let mut output = format!(
        "{ACTIVATION_MARKER}\nname: {}\nsource: {}\ndirectory: {}\nRelative paths in these instructions are resolved from this directory.\n\n--- instructions ---\n{}",
        activation.metadata.name,
        activation.metadata.path.display(),
        dir.display(),
        activation.body,
    );
    if let Some(compatibility) = &activation.metadata.compatibility {
        output.push_str("\n\n--- compatibility ---\n");
        output.push_str(compatibility);
    }
    if !activation.metadata.allowed_tools.is_empty() {
        output.push_str("\n\n--- requested tools (permission policy still applies) ---\n");
        output.push_str(&activation.metadata.allowed_tools.join(" "));
    }
    if !activation.resources.is_empty() {
        output.push_str("\n\n--- bundled resources (load on demand) ---\n");
        for resource in &activation.resources {
            output.push_str("- ");
            output.push_str(&resource.display().to_string());
            output.push('\n');
        }
    }
    output.push_str("\n[END_QAQH_SKILL_V1]");
    output
}

/// Check if a text block is a rendered skill activation (contains markers).
pub fn is_activation_text(text: &str) -> bool {
    text.starts_with(ACTIVATION_MARKER) && text.contains("[END_QAQH_SKILL_V1]")
}

/// Extract the skill name from an activation text block.
///
/// Returns `None` if the text is not a valid activation.
pub fn activation_name(text: &str) -> Option<&str> {
    if !is_activation_text(text) {
        return None;
    }
    text.lines().find_map(|line| line.strip_prefix("name: "))
}

/// Find skills explicitly mentioned via `$skill-name` syntax.
///
/// Scans text for `$name` patterns where `name` matches an available
/// skill. Each skill is returned at most once.
pub fn explicit_mentions(text: &str, catalog: &SkillCatalog) -> Vec<SkillMetadata> {
    let available = catalog
        .skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill))
        .collect::<HashMap<_, _>>();
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_'))
        {
            end += 1;
        }
        if end > start
            && let Some(mention) = text.get(start..end)
            && let Some(skill) = available.get(mention)
            && seen.insert(skill.name.clone())
        {
            found.push((*skill).clone());
        }
        index = end.max(index + 1);
    }
    found
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn write_skill(root: &Path, dir: &str, content: &str) -> PathBuf {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn rejects_legacy_skills_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_skill(
            &temp.path().join("skills"),
            "qaqh/debug",
            "[SkILLS]\n---\nname: qaqh-debug\ndescription: Debug failures.\n---\n\n# Full body",
        );
        // Legacy [SKILLS] prefix is no longer accepted — the skill must be skipped.
        assert!(parse_metadata(&path, SkillScope::Project).is_err());
    }

    #[test]
    fn accepts_windows_line_endings() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_skill(
            temp.path(),
            "windows",
            "---\r\nname: windows\r\ndescription: Windows lines.\r\n---\r\n\r\nFull body",
        );
        let metadata = parse_metadata(&path, SkillScope::Project).unwrap().metadata;
        assert_eq!(load(&metadata).unwrap().body, "Full body");
    }

    #[test]
    fn project_precedence_is_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        write_skill(
            &first,
            "same",
            "---\nname: same\ndescription: First.\n---\n\nOne",
        );
        write_skill(
            &second,
            "same",
            "---\nname: same\ndescription: Second.\n---\n\nTwo",
        );
        let catalog = discover_roots(&[(first, SkillScope::Project), (second, SkillScope::User)]);
        assert_eq!(catalog.skills.len(), 1);
        assert_eq!(catalog.skills[0].description, "First.");
        assert_eq!(catalog.diagnostics.len(), 1);
    }

    #[test]
    fn activation_is_full_and_lists_resources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let path = write_skill(
            &root,
            "large",
            &format!(
                "---\nname: large\ndescription: Large skill.\n---\n\n{}",
                "instruction\n".repeat(350)
            ),
        );
        fs::create_dir_all(path.parent().unwrap().join("references")).unwrap();
        fs::write(path.parent().unwrap().join("references/info.md"), "info").unwrap();
        let metadata = parse_metadata(&path, SkillScope::Project).unwrap().metadata;
        let activation = load(&metadata).unwrap();
        let rendered = render_activation(&activation);
        assert!(rendered.matches("instruction\n").count() >= 350);
        assert!(
            rendered.contains("references\\info.md") || rendered.contains("references/info.md")
        );
        assert!(is_activation_text(&rendered));
    }

    #[test]
    fn extracts_only_known_explicit_mentions_once() {
        let catalog = SkillCatalog {
            skills: vec![SkillMetadata {
                name: "review-code".into(),
                description: "Review code.".into(),
                license: None,
                compatibility: None,
                metadata: BTreeMap::new(),
                allowed_tools: Vec::new(),
                path: PathBuf::from("review/SKILL.md"),
                scope: SkillScope::Project,
            }],
            diagnostics: Vec::new(),
        };
        let found = explicit_mentions("use $review-code and $missing then $review-code", &catalog);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "review-code");
    }

    #[test]
    fn parses_standard_optional_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_skill(
            temp.path(),
            "portable",
            "---\nname: portable\ndescription: Use when testing portable skills.\nlicense: MIT\ncompatibility: Requires git.\nmetadata:\n  author: qaqh\n  version: \"1\"\nallowed-tools: read exec\n---\n\nBody",
        );
        let parsed = parse_metadata(&path, SkillScope::Project).unwrap();
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.metadata.license.as_deref(), Some("MIT"));
        assert_eq!(
            parsed.metadata.compatibility.as_deref(),
            Some("Requires git.")
        );
        assert_eq!(
            parsed.metadata.metadata.get("author").map(String::as_str),
            Some("qaqh")
        );
        assert_eq!(parsed.metadata.allowed_tools, vec!["read", "exec"]);
        assert!(validate_file(&path).is_empty());
    }

    #[test]
    fn strict_validation_reports_nonportable_name_and_directory_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_skill(
            temp.path(),
            "actual-dir",
            "---\nname: Bad_Name\ndescription: Compatibility discovery still loads this.\n---\n\nBody",
        );
        let parsed = parse_metadata(&path, SkillScope::Project).unwrap();
        assert_eq!(parsed.metadata.name, "Bad_Name");
        let diagnostics = validate_file(&path);
        assert!(
            diagnostics
                .iter()
                .all(|item| item.severity == DiagnosticSeverity::Error)
        );
        assert!(
            diagnostics
                .iter()
                .any(|item| item.message.contains("not Agent Skills compliant"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|item| item.message.contains("does not match parent"))
        );
    }

    #[test]
    fn invalid_yaml_frontmatter_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_skill(
            temp.path(),
            "fallback",
            "---\nname: fallback\ndescription: [invalid yaml\n---\n\nBody",
        );
        // Invalid YAML is no longer accepted via compatibility parser.
        assert!(parse_metadata(&path, SkillScope::Project).is_err());
    }

    #[test]
    fn resource_reads_are_contained_and_complete() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        let skill_path = write_skill(
            &workspace.join(".agents/skills"),
            "resource-skill",
            "---\nname: resource-skill\ndescription: Use for resource tests.\n---\n\nRead references/info.md",
        );
        fs::create_dir_all(skill_path.parent().unwrap().join("references")).unwrap();
        fs::write(
            skill_path.parent().unwrap().join("references/info.md"),
            "complete resource",
        )
        .unwrap();
        let resource =
            read_resource(workspace, "resource-skill", Path::new("references/info.md")).unwrap();
        assert_eq!(resource.content, "complete resource");
        assert!(read_resource(workspace, "resource-skill", Path::new("../outside.md")).is_err());
        assert!(read_resource(workspace, "resource-skill", Path::new("references")).is_err());
        assert_eq!(
            managed_skill_for_path(workspace, &skill_path).as_deref(),
            Some("resource-skill")
        );
        assert_eq!(
            managed_skill_for_path(
                workspace,
                &skill_path.parent().unwrap().join("references/info.md")
            )
            .as_deref(),
            Some("resource-skill")
        );
    }

    #[test]
    fn catalog_snapshot_reuses_deterministic_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            &temp.path().join("skills"),
            "alpha",
            "---\nname: alpha\ndescription: Alpha workflow.\n---\n\nBody",
        );

        let first = SkillCatalogSnapshot::discover(temp.path());
        let second = SkillCatalogSnapshot::discover(temp.path());

        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.rendered, second.rendered);
    }

    #[test]
    fn body_change_includes_small_diff_and_summarizes_large_diff() {
        let small = describe_body_change("one\ntwo\n", "one\nchanged\n", 200);
        assert!(
            small
                .diff
                .as_deref()
                .is_some_and(|d| d.contains("-two") && d.contains("+changed"))
        );

        let old = (0..250)
            .map(|i| format!("old-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..250)
            .map(|i| format!("new-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let large = describe_body_change(&old, &new, 200);
        assert!(large.diff.is_none());
        assert!(large.changed_lines > 200);
        assert_ne!(large.old_hash, large.new_hash);
    }

    #[test]
    fn skill_effect_supports_activation() {
        let activation = SkillActivation {
            metadata: SkillMetadata {
                name: "alpha".into(),
                description: String::new(),
                license: None,
                compatibility: None,
                metadata: BTreeMap::new(),
                allowed_tools: Vec::new(),
                path: PathBuf::new(),
                scope: SkillScope::Project,
            },
            body: String::new(),
            resources: Vec::new(),
        };
        assert!(matches!(
            SkillEffect::Activate(activation),
            SkillEffect::Activate(_)
        ));
    }
}

/// A snapshot of the skill catalog at a point in time, with fingerprint for change detection.
///
/// Stored in session state so that context updates can detect when skills
/// have been added, removed, or modified between turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogSnapshot {
    /// The full catalog at snapshot time.
    pub catalog: SkillCatalog,
    /// Rendered catalog text as injected into the system prompt.
    pub rendered: String,
    /// Content-based fingerprint for detecting catalog changes.
    pub fingerprint: String,
}

impl SkillCatalogSnapshot {
    /// Discover skills and create a snapshot with fingerprint.
    pub fn discover(workspace: &Path) -> Self {
        let catalog = discover(workspace);
        let rendered = render_catalog(&catalog);
        let mut fingerprint_input = rendered.clone();
        for skill in &catalog.skills {
            fingerprint_input.push_str(&skill.path.to_string_lossy());
            if let Ok(metadata) = fs::metadata(&skill.path) {
                fingerprint_input.push_str(&format!(":{}", metadata.len()));
                if let Ok(modified) = metadata.modified()
                    && let Ok(elapsed) = modified.duration_since(std::time::UNIX_EPOCH)
                {
                    fingerprint_input.push_str(&format!(":{}", elapsed.as_nanos()));
                }
            }
        }
        let fingerprint = content_hash(&fingerprint_input);
        Self {
            catalog,
            rendered,
            fingerprint,
        }
    }
}

/// A skill context operation requested by the agent.
///
/// Generated by the `skills` tool and applied by the message loop to
/// update the agent's context window with activated skills. The active set
/// only grows within a session; re-activating the same skill replaces its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillEffect {
    /// Activate a skill; its full body is delivered in the tool result.
    Activate(SkillActivation),
}

/// Describes how a skill body changed between two versions.
///
/// Used by the context manager to decide whether to emit a
/// `SkillUpdated` diagnostic event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBodyChange {
    pub old_hash: String,
    pub new_hash: String,
    pub changed_lines: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub diff: Option<String>,
}

/// FNV-1a 64-bit hash for cheap content fingerprinting.
pub fn content_hash(content: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Deterministic provider-neutral token estimate used for admission budgets.
/// Provider-specific tokenizers may report more precise UI telemetry, but this
/// calculation remains stable across processes and never causes truncation.
pub fn token_count(content: &str) -> usize {
    content.chars().count().div_ceil(4)
}

/// Compare two skill bodies and produce a change summary.
///
/// If the total changed lines ≤ `max_diff_lines`, a unified diff is included.
/// Otherwise only the line counts and content hashes are returned.
pub fn describe_body_change(old: &str, new: &str, max_diff_lines: usize) -> SkillBodyChange {
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let common = old_lines.len().min(new_lines.len());
    let replaced = (0..common)
        .filter(|&i| old_lines[i] != new_lines[i])
        .count();
    let removed_lines = replaced + old_lines.len().saturating_sub(common);
    let added_lines = replaced + new_lines.len().saturating_sub(common);
    let changed_lines = removed_lines + added_lines;
    let diff = (changed_lines <= max_diff_lines).then(|| {
        let mut out = String::new();
        for index in 0..old_lines.len().max(new_lines.len()) {
            match (old_lines.get(index), new_lines.get(index)) {
                (Some(before), Some(after)) if before == after => {}
                (Some(before), Some(after)) => {
                    out.push_str(&format!("-{}\n+{}\n", before, after));
                }
                (Some(before), None) => out.push_str(&format!("-{}\n", before)),
                (None, Some(after)) => out.push_str(&format!("+{}\n", after)),
                (None, None) => {}
            }
        }
        out
    });
    SkillBodyChange {
        old_hash: content_hash(old),
        new_hash: content_hash(new),
        changed_lines,
        added_lines,
        removed_lines,
        diff,
    }
}
