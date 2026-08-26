//! 引擎 [`Config`](crate::Config) ↔ wire DTO（`qaqh-config-api`）双向映射层。
//!
//! PLAN docs/config-revamp-plan.md §2/K1：service 层不再手拼 config json，
//! 所有字段同步收敛到本模块的**穷举字面量**里——任何一端新增字段而另一端
//! 未映射，在此处直接编译失败（S2「人肉同步」病根由编译器接管）。
//!
//! 守卫语义冻结（与 2026-08 前 `update_string/update_u32` 行为对齐，见
//! qaqh-config-api `ConfigPatch` 文档）：
//! - apiKey / subagent.apiKey：`"****"` 或空串 = 保持现值（显式删除走专用接口）；
//! - model / baseUrl / providerId / endpoint / reasoningEffort：空串 = 保持
//!   （防未加载草稿整包写回时清空用户配置——2026-08 根因 1 同类防御）；
//! - lang / theme / tokenizerPath：空串 = 清除（None，跟随系统/未设置）；
//! - fontFamily：原样赋值（空串 = 跟随系统默认）；
//! - 数值：validate() 已保证值域；u64→u32 饱和转换防回绕。

use crate::config::Config;
use qaqh_config_api::{
    ConfigDto, ConfigPatch, EndpointDto, ProviderDto, SubagentDto, WorkspaceDto,
};

/// 引擎配置 → 读模型。api_key 按契约掩码：非空一律 `"****"`（明文永不出 daemon）。
pub fn to_dto(cfg: &Config) -> ConfigDto {
    let providers: Vec<ProviderDto> = crate::registry::all_providers()
        .into_iter()
        .map(|p| ProviderDto {
            id: p.id.clone(),
            display: p.display.clone(),
            endpoints: p
                .endpoints
                .into_iter()
                .map(|e| EndpointDto {
                    id: e.id,
                    display: e.display,
                    protocol: e.protocol,
                    base_url: e.base_url,
                    default_model: e.default_model,
                    models: e.models,
                    stateful: e.stateful,
                    beta: e.beta,
                })
                .collect(),
        })
        .collect();
    ConfigDto {
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        provider_id: cfg.provider_id.clone(),
        endpoint: cfg.endpoint.clone(),
        max_tokens: u64::from(cfg.max_tokens),
        context_limit: u64::from(cfg.context_limit),
        reasoning_effort: cfg.reasoning_effort.clone(),
        auto_compact_threshold: cfg.auto_compact_threshold,
        permission_level: cfg.permission_level,
        api_key: masked_or_empty(&cfg.api_key),
        lang: cfg.lang.clone(),
        font_family: cfg.font_family.clone(),
        theme: cfg.theme.clone(),
        notifications_enabled: cfg.notifications_enabled.unwrap_or(true),
        active_profile: cfg.active_profile.clone(),
        profiles: cfg.profiles.keys().cloned().collect(),
        compliance_enabled: cfg.compliance_enabled,
        providers,
        subagent: SubagentDto {
            model: cfg.subagent.model.clone(),
            base_url: cfg.subagent.base_url.clone(),
            api_key: masked_or_empty(&cfg.subagent.api_key),
            api_key_set: !cfg.subagent.api_key.is_empty(),
            max_tokens: u64::from(cfg.subagent.max_tokens),
            timeout_secs: cfg.subagent.timeout_secs,
            default_tools: cfg.subagent.default_tools.clone(),
        },
        workspace: WorkspaceDto {
            mode: cfg.workspace.mode.clone(),
        },
        tokenizer_path: cfg.tokenizer_path.clone(),
    }
}

/// 把 Merge Patch 应用到引擎配置（`Config::update` 单写口的 mutate 体）。
/// 值域校验先行；逐字段守卫语义见模块文档。
pub fn apply_patch(cfg: &mut Config, patch: &ConfigPatch) -> Result<(), String> {
    patch.validate()?;
    // 历史「空串 = 保持」语义：过滤掩码与空串，仅显式新值放行。
    let meaningful = |s: &Option<String>| {
        s.as_deref()
            .is_some_and(|v| !v.is_empty() && v != "****")
            .then(|| s.clone().expect("checked some"))
    };
    if let Some(v) = meaningful(&patch.api_key) {
        cfg.api_key = v;
    }
    if let Some(v) = meaningful(&patch.model) {
        cfg.model = v;
    }
    if let Some(v) = meaningful(&patch.base_url) {
        cfg.base_url = v;
    }
    if let Some(v) = meaningful(&patch.provider_id) {
        cfg.provider_id = v;
    }
    if let Some(v) = meaningful(&patch.endpoint) {
        cfg.endpoint = v;
    }
    if let Some(v) = patch.max_tokens {
        cfg.max_tokens = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = patch.context_limit {
        cfg.context_limit = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = meaningful(&patch.reasoning_effort) {
        cfg.reasoning_effort = v;
    }
    if let Some(v) = patch.auto_compact_threshold {
        cfg.auto_compact_threshold = v;
    }
    if let Some(v) = patch.compliance_enabled {
        cfg.compliance_enabled = v;
    }
    if let Some(v) = &patch.lang {
        cfg.lang = if v.is_empty() { None } else { Some(v.clone()) };
    }
    if let Some(v) = &patch.font_family {
        if v != "****" {
            cfg.font_family = v.clone();
        }
    }
    if let Some(v) = &patch.theme {
        cfg.theme = if v.is_empty() { None } else { Some(v.clone()) };
    }
    if let Some(v) = patch.notifications_enabled {
        cfg.notifications_enabled = Some(v);
    }
    if let Some(v) = &patch.tokenizer_path {
        cfg.tokenizer_path = (!v.is_empty()).then(|| v.clone());
    }
    if let Some(sub) = &patch.subagent {
        if let Some(v) = meaningful(&sub.model) {
            cfg.subagent.model = v;
        }
        if let Some(v) = meaningful(&sub.base_url) {
            cfg.subagent.base_url = v;
        }
        if let Some(v) = meaningful(&sub.api_key) {
            cfg.subagent.api_key = v;
        }
        if let Some(v) = sub.max_tokens {
            cfg.subagent.max_tokens = u32::try_from(v).unwrap_or(u32::MAX);
        }
        if let Some(v) = sub.timeout_secs {
            cfg.subagent.timeout_secs = v;
        }
        if let Some(v) = &sub.default_tools {
            // 允许空数组（= 全部工具可用）；工具名归一化在 load 路径统一做。
            cfg.subagent.default_tools = v.clone();
        }
    }
    Ok(())
}

fn masked_or_empty(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 穷举映射回归：改任一字段后此测试必须同步更新（K1 编译期+运行期双保险）。
    #[test]
    fn to_dto_masks_secrets_and_projects_all_sections() {
        let mut cfg = Config::default();
        cfg.api_key = "sk-secret".into();
        cfg.model = "m".into();
        cfg.context_limit = 1_000_000;
        cfg.auto_compact_threshold = 0.95;
        cfg.subagent.api_key = "sk-sub".into();
        cfg.profiles.insert(
            "default".into(),
            qaqh_types::ProfileConfig {
                model: "m".into(),
                max_tokens: 4096,
                effort: Some("high".into()),
                context_limit: 128_000,
                base_url: String::new(),
                endpoint: None,
            },
        );

        let dto = to_dto(&cfg);
        assert_eq!(dto.api_key, "****");
        assert_eq!(dto.subagent.api_key, "****");
        assert!(dto.subagent.api_key_set);
        assert_eq!(dto.context_limit, 1_000_000);
        assert!((dto.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert!(dto.profiles.contains(&"default".to_string()));
        let empty = to_dto(&Config::default());
        assert_eq!(empty.api_key, "");
        assert!(!empty.subagent.api_key_set);
    }

    #[test]
    fn apply_patch_updates_fields_and_keeps_untouched() {
        let mut cfg = Config::default();
        cfg.model = "old".into();
        cfg.auto_compact_threshold = 0.3;
        let patch = ConfigPatch {
            model: Some("new".into()),
            context_limit: Some(2_000_000),
            auto_compact_threshold: Some(0.95),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.model, "new");
        assert_eq!(cfg.context_limit, 2_000_000);
        assert!((cfg.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_patch_keeps_secret_and_empty_semantics() {
        let mut cfg = Config::default();
        cfg.api_key = "sk-keep".into();
        cfg.model = "m".into();
        cfg.lang = Some("zh".into());
        cfg.theme = Some("dark".into());
        // 掩码/空串 = 保持现值。
        let patch = ConfigPatch {
            api_key: Some("****".into()),
            model: Some(String::new()),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.api_key, "sk-keep");
        assert_eq!(cfg.model, "m");
        // 显式新值 = 替换。
        let replace = ConfigPatch {
            api_key: Some("sk-new".into()),
            ..Default::default()
        };
        apply_patch(&mut cfg, &replace).expect("apply");
        assert_eq!(cfg.api_key, "sk-new");
        // 空串 lang/theme = 清除（跟随系统）。
        let clear = ConfigPatch {
            lang: Some(String::new()),
            theme: Some(String::new()),
            ..Default::default()
        };
        apply_patch(&mut cfg, &clear).expect("apply");
        assert_eq!(cfg.lang, None);
        assert_eq!(cfg.theme, None);
    }

    #[test]
    fn apply_patch_subagent_partial_and_empty_tools_allowed() {
        let mut cfg = Config::default();
        cfg.subagent.timeout_secs = 120;
        cfg.subagent.default_tools = vec!["read".into()];
        let patch = ConfigPatch {
            subagent: Some(qaqh_config_api::SubagentPatch {
                timeout_secs: Some(240),
                default_tools: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.subagent.timeout_secs, 240);
        assert!(cfg.subagent.default_tools.is_empty());
    }

    #[test]
    fn apply_patch_rejects_invalid_before_mutating() {
        let mut cfg = Config::default();
        cfg.auto_compact_threshold = 0.75;
        let bad = ConfigPatch {
            auto_compact_threshold: Some(1.5),
            ..Default::default()
        };
        assert!(apply_patch(&mut cfg, &bad).is_err());
        assert!((cfg.auto_compact_threshold - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_patch_saturates_u32_overflow() {
        let mut cfg = Config::default();
        let patch = ConfigPatch {
            context_limit: Some(u64::MAX),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.context_limit, u32::MAX);
    }
}
