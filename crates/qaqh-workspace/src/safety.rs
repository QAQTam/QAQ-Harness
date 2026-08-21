//! Safety verdicts for tool invocation.

use crate::ToolRisk;

#[derive(Debug, Clone)]
pub enum SafetyVerdict {
    Allow,
    Block(String),
}

pub struct SafetyPolicy;

impl SafetyPolicy {
    pub fn evaluate(risk: ToolRisk, in_workspace: bool) -> SafetyVerdict {
        match (risk, in_workspace) {
            (ToolRisk::ReadOnly, _) => SafetyVerdict::Allow,
            (ToolRisk::Write, _) => SafetyVerdict::Allow,
            (ToolRisk::Destructive, false) => {
                SafetyVerdict::Block("Destructive operation outside workspace is blocked".into())
            }
            (ToolRisk::Destructive, true) => SafetyVerdict::Allow,
            (ToolRisk::Administrative, _) => SafetyVerdict::Allow,
        }
    }
}
