//! Agent Skill 激活工具。
//!
//! 与通用 `read` 不同，本工具按发现目录中的 skill 名解析并返回完整正文，
//! 不执行 200 行截断，也不允许模型传入任意文件路径。

use std::path::Path;

use crate::{JsonArgs, ToolHandler, ToolResult, ToolRisk};

fn current_workspace() -> String {
    crate::current_workspace()
}

pub(crate) fn load_activation(
    args: &serde_json::Value,
) -> Result<qaqh_skills::SkillActivation, String> {
    let name = args.s("name");
    if name.is_empty() {
        return Err("skill name is required".into());
    }
    let workspace = current_workspace();
    qaqh_skills::load_named(Path::new(&workspace), &name)
}

fn load_skill_resource(
    args: &serde_json::Value,
) -> Result<String, (&'static str, String, &'static str)> {
    let name = args.s("name");
    let path = args.s("path");
    if name.is_empty() || path.is_empty() {
        return Err((
            "MISSING_ARGUMENT",
            "skill resource requires name and path".into(),
            "Use an exact skill name and a relative path from its resource manifest.",
        ));
    }
    let workspace = current_workspace();
    match qaqh_skills::read_resource(Path::new(&workspace), &name, Path::new(&path)) {
        Ok(resource) => Ok(resource.content),
        Err(error) => Err((
            "SKILL_RESOURCE_UNAVAILABLE",
            error,
            "Use a relative file path listed by the activated skill.",
        )),
    }
}

fn handle_skill(ctx: crate::ToolCallCtx) -> ToolResult {
    match load_activation(&ctx.args) {
        Ok(activation) => {
            let name = activation.metadata.name.clone();
            let resources: Vec<String> = activation
                .resources
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            ctx.push_skill_effect(qaqh_skills::SkillEffect::Activate(activation));
            ToolResult::ok(serde_json::json!({
                "status": "ok",
                "skill": name,
                "resources": resources,
                "content": format!(
                    "[OK] skill '{name}' activated. The full instructions are injected as the trailing <skill_context_envelope> system message (authoritative — it replaces all older skill instructions). If the envelope is not visible, call resource to read bundled files on demand."
                )
            }).to_string())
        }
        Err(error) => ToolResult::error(crate::json_err(
            "SKILL_NOT_AVAILABLE",
            error,
            "Use an exact name from the current skill catalog.",
        )),
    }
}

fn handle_skill_resource(ctx: crate::ToolCallCtx) -> ToolResult {
    match load_skill_resource(&ctx.args) {
        Ok(content) => ToolResult::ok(content),
        Err((code, message, hint)) => ToolResult::error(crate::json_err(code, message, hint)),
    }
}

fn handle_skills_list(_ctx: crate::ToolCallCtx) -> ToolResult {
    let workspace = current_workspace();
    let catalog = qaqh_skills::discover(Path::new(&workspace));
    let skills = catalog
        .skills
        .iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "scope": match skill.scope {
                    qaqh_skills::SkillScope::Project => "project",
                    qaqh_skills::SkillScope::User => "user",
                },
                "source": skill.path,
            })
        })
        .collect::<Vec<_>>();
    let diagnostics = catalog
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "severity": match diagnostic.severity {
                    qaqh_skills::DiagnosticSeverity::Warning => "warning",
                    qaqh_skills::DiagnosticSeverity::Error => "error",
                },
                "source": diagnostic.path,
                "message": diagnostic.message,
            })
        })
        .collect::<Vec<_>>();
    ToolResult::ok(serde_json::json!({"skills": skills, "diagnostics": diagnostics}).to_string())
}

fn handle_skill_validate(ctx: crate::ToolCallCtx) -> ToolResult {
    let name = ctx.args.s("name");
    if name.is_empty() {
        return ToolResult::error(crate::json_err(
            "MISSING_NAME",
            "skill name is required",
            "Use an exact name from the skill catalog.",
        ));
    }
    let workspace = current_workspace();
    let catalog = qaqh_skills::discover(Path::new(&workspace));
    let Some(skill) = catalog.skills.iter().find(|skill| skill.name == name) else {
        return ToolResult::error(crate::json_err(
            "SKILL_NOT_AVAILABLE",
            format!("unknown skill '{name}'"),
            "Use an exact name from the current skill catalog.",
        ));
    };
    let diagnostics = qaqh_skills::validate_file(&skill.path);
    let errors = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    ToolResult::ok(
        serde_json::json!({
            "name": name,
            "source": skill.path,
            "valid": errors.is_empty(),
            "errors": errors,
        })
        .to_string(),
    )
}

fn handle_skills(ctx: crate::ToolCallCtx) -> ToolResult {
    let action = ctx.args.s("action");
    let has_name = ctx.args.get("name").is_some();
    let has_path = ctx.args.get("path").is_some();
    match action.as_str() {
        "activate" if has_name && !has_path => handle_skill(ctx),
        "list" if !has_name && !has_path => handle_skills_list(ctx),
        "resource" if has_name && has_path => handle_skill_resource(ctx),
        "validate" if has_name && !has_path => handle_skill_validate(ctx),
        "activate" | "list" | "resource" | "validate" => ToolResult::error(crate::json_err(
            "INVALID_ARGUMENTS",
            format!("arguments do not match skills action '{action}'"),
            "activate and validate require name; list accepts only action; resource requires name and path.",
        )),
        _ => ToolResult::error(crate::json_err(
            "INVALID_ACTION",
            "skills action must be activate, list, resource, or validate",
            "Choose the action matching the required skill operation.",
        )),
    }
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "skills".to_string(),
        description: "Manage Agent Skills through one fixed interface. Use activate before acting when a task matches the catalog — the full instructions are injected as a trailing skill_context_envelope system message; resource reads bundled files on demand; list for catalog diagnostics; validate for portability checks. Skill metadata never bypasses QAQ-Harness permissions.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["activate", "list", "resource", "validate"],
                    "description": "activate: load instructions and return the full body; list: inspect catalog diagnostics; resource: read a bundled resource; validate: validate one SKILL.md"
                },
                "name": {
                    "type": "string",
                    "description": "Exact skill name from the injected catalog. Required for activate, resource, and validate; forbidden for list"
                },
                "path": {
                    "type": "string",
                    "description": "Skill-directory-relative resource path from the activation manifest. Required only for resource; absolute paths and parent traversal are rejected"
                }
            },
            "required": ["action"],
            "additionalProperties": false,
            "oneOf": [
                {
                    "title": "Activate a skill",
                    "properties": {"action": {"const": "activate"}},
                    "required": ["action", "name"]
                },
                {
                    "title": "List effective skills",
                    "properties": {"action": {"const": "list"}},
                    "required": ["action"],
                    "not": {"anyOf": [{"required": ["name"]}, {"required": ["path"]}]}
                },
                {
                    "title": "Read a skill resource",
                    "properties": {"action": {"const": "resource"}},
                    "required": ["action", "name", "path"]
                },
                {
                    "title": "Validate a skill",
                    "properties": {"action": {"const": "validate"}},
                    "required": ["action", "name"]
                }
            ]
        }),
        handler: handle_skills,
        risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Read,
        default_timeout: std::time::Duration::from_secs(15),
    },
    crate::ToolPlacement::Workspace,
);
}
