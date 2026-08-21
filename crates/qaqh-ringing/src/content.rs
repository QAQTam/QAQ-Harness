//! 大内容外置引用（wire 视图）。

/// 大内容引用。事件只携带可渲染 tail/统计信息与 `content_ref`；
/// 完整内容经带鉴权的 HTTP GET 按需读取（range/分页、会话所有权与生命周期）。
///
/// wire 名固定为 `RingingContentRef`（PLAN 公共类型）；域类型为
/// `qaqh_domain::ContentRef`，同一份数据不复制。
pub use qaqh_domain::ContentRef as RingingContentRef;
