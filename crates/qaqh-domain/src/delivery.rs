use serde::{Deserialize, Serialize};

/// 事件的可靠性等级。由**领域事件定义**显式声明（PLAN 硬规则：
/// "可靠性由事件定义显式声明；Wire 不决定业务可靠性"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// 生命周期、终态、interaction、错误与 revision 变更。
    /// 进入有界 journal，必须按 cursor 回放，不能静默丢弃。
    Reliable,
    /// message/reasoning/tool progress 等增量。允许按 identity 合并或用较新
    /// checkpoint 覆盖，不承诺逐 token 重放。
    Replaceable,
    /// 仅诊断性 live 提示，不进入 snapshot 与 journal。
    Ephemeral,
}
