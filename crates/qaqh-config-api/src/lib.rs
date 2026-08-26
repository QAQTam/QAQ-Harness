//! QAQ-Harness 配置契约层（wire DTO）。
//!
//! 前后端共享的**唯一真相**（PLAN docs/config-revamp-plan.md §2 约束 K1-K4）：
//!
//! - K1 叶子 crate：只依赖 serde，不依赖 runtime/msgloop/daemon/config 引擎；
//!   winui / ratatui / web(axum) 三端只依赖本 crate 即可参与配置读写。
//! - K2 键风格定死 camelCase（`rename_all`），snake/camel 双键 shim 就此终结。
//! - K3 写语义 = JSON Merge Patch（RFC 7386 风格）：[`ConfigPatch`] 只含
//!   `Option` 字段，缺失 = 不动；多端并发编辑互不覆盖，且天然免疫
//!   「未加载字段整包写回」类毒化（2026-08-25 设置页事故 R5）。
//! - K4 fingerprint/字节序属 ringing 传输层，本 crate 零耦合。
//!
//! 映射纪律：引擎侧 [`crate`] 与 `qaqh_config::Config` 的互转**不用**
//! `..Default::default()` 兜底——穷举字面量让"新增字段未同步映射"变成编译错误。
//!
//! 兼容策略：**写路径只发 camelCase**；读路径额外接受历史 snake_case 别名
//! （`alias`）——新前端对旧 daemon、新 daemon 对旧前端均不炸（PLAN §4）。

use serde::{Deserialize, Serialize};

/// 桌面通知缺省 = 开启（daemon 契约；字段缺失时的读侧兜底）。
fn default_notifications_enabled() -> bool {
    true
}

/// 读模型：daemon `config.load` 的完整投影。所有消费者（设置页/Info 面板/
/// TUI/web）从这里取值；`serde(default)` 保证旧 daemon 缺字段时向前兼容。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigDto {
    pub model: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "provider_id")]
    pub provider_id: String,
    pub endpoint: String,
    #[serde(alias = "max_tokens")]
    pub max_tokens: u64,
    #[serde(alias = "context_limit")]
    pub context_limit: u64,
    #[serde(alias = "reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(alias = "auto_compact_threshold")]
    pub auto_compact_threshold: f64,
    #[serde(alias = "permission_level")]
    pub permission_level: u8,
    /// 密钥永不出 daemon：空串 = 未配置、`"****"` = 已配置（掩码）。
    #[serde(alias = "api_key")]
    pub api_key: String,
    pub lang: Option<String>,
    #[serde(alias = "font_family")]
    pub font_family: String,
    /// None/空 = 跟随系统。
    pub theme: Option<String>,
    #[serde(
        default = "default_notifications_enabled",
        alias = "notifications_enabled"
    )]
    pub notifications_enabled: bool,
    #[serde(alias = "active_profile")]
    pub active_profile: String,
    /// profile 名列表（管理 UI 用；不含敏感字段）。
    pub profiles: Vec<String>,
    #[serde(alias = "compliance_enabled")]
    pub compliance_enabled: bool,
    pub providers: Vec<ProviderDto>,
    pub subagent: SubagentDto,
    pub workspace: WorkspaceDto,
    #[serde(alias = "tokenizer_path")]
    pub tokenizer_path: Option<String>,
}

/// provider 目录项（endpoint 预设树）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderDto {
    pub id: String,
    pub display: String,
    pub endpoints: Vec<EndpointDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointDto {
    pub id: String,
    pub display: String,
    pub protocol: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "default_model")]
    pub default_model: String,
    pub models: Vec<String>,
    pub stateful: bool,
    pub beta: bool,
}

/// 子代理配置段（读模型）。api_key 语义同顶层：空串/"****" 掩码。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentDto {
    pub model: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "api_key")]
    pub api_key: String,
    #[serde(alias = "api_key_set")]
    pub api_key_set: bool,
    #[serde(alias = "max_tokens")]
    pub max_tokens: u64,
    #[serde(alias = "timeout_secs")]
    pub timeout_secs: u64,
    /// 空数组 = 全部工具可用（配置语义，非缺省）。
    #[serde(alias = "default_tools")]
    pub default_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkspaceDto {
    pub mode: String,
}

/// 写模型：JSON Merge Patch（K3）。反序列化时缺失即 None = 不动；
/// 序列化时跳过 None，保证 wire 上永不出现 `"field": null`。
///
/// 刻意**不含**：providers/profiles 名录（服务端派生）、active_profile
/// （切换走 `profile.apply`）、permission_level（走 `set_permission_level`
/// 单写口）、api_key_set（服务端派生）。
///
/// 特例语义冻结：`apiKey`/`subagentApiKey` 沿用既有守卫——`"****"` 或空串 =
/// 保持现值（显式删除须专用接口）；其余字符串字段 Some(空串) = 显式置空。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigPatch {
    /// 主密钥：仅用户显式输入新值时 Some；掩码 `"****"`/空串 = 保持现值。
    #[serde(skip_serializing_if = "Option::is_none", alias = "api_key")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "base_url")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "provider_id")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "max_tokens")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "context_limit")]
    pub context_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "reasoning_effort")]
    pub reasoning_effort: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "auto_compact_threshold"
    )]
    /// 值域 `[0,1]`；`0` = 关闭自动压缩。
    pub auto_compact_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "compliance_enabled")]
    pub compliance_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "font_family")]
    pub font_family: Option<String>,
    /// None = 不动；Some("") = 跟随系统。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "notifications_enabled"
    )]
    pub notifications_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "tokenizer_path")]
    pub tokenizer_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentPatch>,
}

/// 子代理配置段（写模型），嵌套于 [`ConfigPatch::subagent`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "base_url")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "api_key")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "max_tokens")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "timeout_secs")]
    pub timeout_secs: Option<u64>,
    /// 允许空数组（= 全部工具可用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
}

impl ConfigPatch {
    /// 边界值域校验（写入前必过；P3 收敛为统一 validate 层的第一块）。
    /// 返回首个违规字段的人类可读错误。
    pub fn validate(&self) -> Result<(), String> {
        // 0.0 合法：= 关闭自动压缩（开关 OFF 时前端显式上报；2026-08-25
        // 真机回归抓出开区间误杀禁用流）。仅拒绝 NaN 与 [0,1] 之外。
        if let Some(t) = self.auto_compact_threshold
            && (t.is_nan() || !(0.0..=1.0).contains(&t))
        {
            return Err(format!(
                "autoCompactThreshold 必须在 [0, 1] 区间（0=关闭），收到 {t}"
            ));
        }
        if let Some(v) = self.max_tokens
            && v == 0
        {
            return Err("maxTokens 必须大于 0".to_string());
        }
        if let Some(v) = self.context_limit
            && v == 0
        {
            return Err("contextLimit 必须大于 0".to_string());
        }
        if let Some(e) = &self.reasoning_effort
            && !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
        {
            return Err(format!(
                "reasoningEffort 仅允许 low|medium|high|xhigh|max，收到 {e}"
            ));
        }
        if let Some(sub) = &self.subagent {
            if let Some(v) = sub.max_tokens
                && v == 0
            {
                return Err("subagent.maxTokens 必须大于 0".to_string());
            }
            if let Some(v) = sub.timeout_secs
                && v == 0
            {
                return Err("subagent.timeoutSecs 必须大于 0".to_string());
            }
        }
        Ok(())
    }

    /// 是否为空补丁（无任何字段要改）——调用方可据此跳过落盘与 reload 广播。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// K2：键风格必须是 camelCase（wire 契约冻结，防回退到 snake 双键时代）。
    #[test]
    fn dto_serializes_camel_case() {
        let dto = ConfigDto {
            base_url: "https://x/v1".into(),
            auto_compact_threshold: 0.95,
            notifications_enabled: true,
            ..Default::default()
        };
        let s = serde_json::to_string(&dto).expect("serialize");
        assert!(s.contains("\"baseUrl\""), "{s}");
        assert!(s.contains("\"autoCompactThreshold\""), "{s}");
        assert!(s.contains("\"notificationsEnabled\""), "{s}");
        assert!(!s.contains("base_url"), "{s}");
    }

    /// 读路径向前兼容：旧 daemon 缺字段 → serde(default) 兜底不报错。
    #[test]
    fn dto_tolerates_missing_fields() {
        let dto: ConfigDto =
            serde_json::from_value(json!({ "model": "m1" })).expect("tolerant parse");
        assert_eq!(dto.model, "m1");
        assert_eq!(dto.auto_compact_threshold, 0.0);
        assert!(dto.profiles.is_empty());
    }

    /// 活体 fixture：2026-08-25 对运行中 daemon lease 探测的真实响应形状
    /// （截选关键字段）必须能无损解析进 ConfigDto——C2 切换时零惊吓。
    #[test]
    fn dto_parses_live_daemon_response_shape() {
        let payload = json!({
            "api_key": "****",
            "api_key_set": true,
            "model": "ox-alpha-free",
            "base_url": "https://opencode.ai/zen/go/v1",
            "provider_id": "opencode-go",
            "endpoint": "openai",
            "max_tokens": 96000,
            "context_limit": 1000000,
            "reasoning_effort": "max",
            "auto_compact_threshold": 0.95,
            "permission_level": 4,
            "lang": null,
            "font_family": "",
            "theme": null,
            "notifications_enabled": true,
            "active_profile": "default",
            "profiles": ["default"],
            "compliance_enabled": false,
            "providers": [{
                "id": "opencode-go",
                "display": "OpenCode",
                "endpoints": [{
                    "id": "openai",
                    "display": "OpenAI",
                    "protocol": "openai",
                    "base_url": "https://opencode.ai/zen/go/v1",
                    "default_model": "",
                    "models": ["ox-alpha-free"],
                    "stateful": false,
                    "beta": false
                }]
            }],
            "subagent": {
                "model": "",
                "base_url": "",
                "api_key": "",
                "api_key_set": false,
                "max_tokens": 4096,
                "timeout_secs": 120,
                "default_tools": ["read"]
            },
            "workspace": { "mode": "local" },
            "tokenizer_path": null
        });
        let dto: ConfigDto = serde_json::from_value(payload).expect("live shape parse");
        assert_eq!(dto.context_limit, 1_000_000);
        assert_eq!(dto.reasoning_effort, "max");
        assert!((dto.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert_eq!(dto.api_key, "****");
        assert_eq!(dto.subagent.timeout_secs, 120);
    }

    /// K3：空 Patch 序列化为 `{}`——wire 上不发 null、不发未改动字段。
    #[test]
    fn empty_patch_serializes_to_empty_object() {
        let s = serde_json::to_string(&ConfigPatch::default()).expect("serialize");
        assert_eq!(s, "{}");
    }

    #[test]
    fn patch_roundtrip_with_nested_subagent() {
        let patch = ConfigPatch {
            model: Some("m".into()),
            context_limit: Some(1_000_000),
            auto_compact_threshold: Some(0.95),
            subagent: Some(SubagentPatch {
                timeout_secs: Some(240),
                default_tools: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&patch).expect("serialize");
        assert_eq!(v["model"], json!("m"));
        assert_eq!(v["contextLimit"], json!(1_000_000));
        assert_eq!(v["autoCompactThreshold"], json!(0.95));
        assert_eq!(v["subagent"]["timeoutSecs"], json!(240));
        assert_eq!(v["subagent"]["defaultTools"], json!([]));
        // None 字段不得出现在 wire 上。
        assert!(v.get("baseUrl").is_none());
        assert!(v.get("theme").is_none());
        let back: ConfigPatch = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, patch);

        // 读宽容：历史 snake 键同样能进 Patch（新旧版本共存期双向兼容）。
        let legacy: ConfigPatch = serde_json::from_value(json!({
            "context_limit": 500_000,
            "subagent": { "timeout_secs": 60 }
        }))
        .expect("legacy snake parse");
        assert_eq!(legacy.context_limit, Some(500_000));
        assert_eq!(legacy.subagent.expect("subagent").timeout_secs, Some(60));
    }

    #[test]
    fn patch_validate_rejects_out_of_range() {
        let bad = ConfigPatch {
            auto_compact_threshold: Some(1.5),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let bad_effort = ConfigPatch {
            reasoning_effort: Some("ultra".into()),
            ..Default::default()
        };
        assert!(bad_effort.validate().is_err());
        // 0.0 = 关闭自动压缩，合法（真机回归：开区间曾误杀禁用流）。
        let disabled = ConfigPatch {
            auto_compact_threshold: Some(0.0),
            ..Default::default()
        };
        assert!(disabled.validate().is_ok());
        let good = ConfigPatch {
            auto_compact_threshold: Some(0.95),
            reasoning_effort: Some("max".into()),
            max_tokens: Some(96_000),
            ..Default::default()
        };
        assert!(good.validate().is_ok());
        assert!(!good.is_empty());
        assert!(ConfigPatch::default().is_empty());
    }
}
