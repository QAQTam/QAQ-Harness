use std::collections::HashSet;

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedAskMode {
    Single,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NormalizedAskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub allow_custom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NormalizedAsk {
    pub mode: NormalizedAskMode,
    pub questions: Vec<NormalizedAskQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserError {
    pub code: &'static str,
    pub message: String,
}

pub fn normalize_ask_user(args: &serde_json::Value) -> Result<NormalizedAsk, AskUserError> {
    let raw_questions = match args.get("questions") {
        Some(serde_json::Value::Array(questions)) => questions.clone(),
        Some(_) => {
            return Err(AskUserError {
                code: "INVALID_QUESTIONS",
                message: "questions must be an array".into(),
            });
        }
        None => vec![serde_json::json!({
            "id": "q1",
            "question": args.get("question").and_then(serde_json::Value::as_str).unwrap_or(""),
            "options": args.get("options").cloned().unwrap_or_else(|| serde_json::json!([])),
            "allow_custom": args.get("allow_custom").and_then(serde_json::Value::as_bool).unwrap_or(true),
        })],
    };

    if raw_questions.is_empty() {
        return Err(AskUserError {
            code: "EMPTY_QUESTIONS",
            message: "at least one question is required".into(),
        });
    }

    let mut ids = HashSet::new();
    let mut questions = Vec::with_capacity(raw_questions.len());
    for (index, raw) in raw_questions.iter().enumerate() {
        let question = raw
            .get("question")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if question.trim().is_empty() {
            return Err(AskUserError {
                code: "MISSING_QUESTION",
                message: format!("questions[{index}].question is required"),
            });
        }

        let id = match raw.get("id") {
            None => format!("q{}", index + 1),
            Some(serde_json::Value::String(id)) if !id.trim().is_empty() => id.clone(),
            Some(_) => {
                return Err(AskUserError {
                    code: "INVALID_QUESTION_ID",
                    message: format!("questions[{index}].id must be a non-empty string"),
                });
            }
        };
        if !ids.insert(id.clone()) {
            return Err(AskUserError {
                code: "DUPLICATE_QUESTION_ID",
                message: format!("duplicate question id: {id}"),
            });
        }

        let options = match raw.get("options") {
            None => Vec::new(),
            Some(serde_json::Value::Array(values)) => {
                let mut options = Vec::with_capacity(values.len());
                for (option_index, value) in values.iter().enumerate() {
                    let Some(option) = value.as_str() else {
                        return Err(AskUserError {
                            code: "INVALID_OPTION",
                            message: format!(
                                "question {id} option {option_index} must be a non-empty string"
                            ),
                        });
                    };
                    let option = option.trim();
                    if option.is_empty() {
                        return Err(AskUserError {
                            code: "INVALID_OPTION",
                            message: format!(
                                "question {id} option {option_index} must be a non-empty string"
                            ),
                        });
                    }
                    options.push(option.to_string());
                }
                options
            }
            Some(_) => {
                return Err(AskUserError {
                    code: "INVALID_OPTIONS",
                    message: format!("question {id} options must be an array"),
                });
            }
        };
        let mut unique_options = HashSet::new();
        if options
            .iter()
            .any(|option| !unique_options.insert(option.clone()))
        {
            return Err(AskUserError {
                code: "DUPLICATE_OPTION",
                message: format!("question {id} contains duplicate options"),
            });
        }

        let allow_custom = raw
            .get("allow_custom")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if options.is_empty() && !allow_custom {
            return Err(AskUserError {
                code: "UNANSWERABLE_QUESTION",
                message: format!("question {id} has no valid answer path"),
            });
        }

        questions.push(NormalizedAskQuestion {
            id,
            question,
            options,
            allow_custom,
        });
    }

    let mode = if questions.len() == 1 {
        NormalizedAskMode::Single
    } else {
        NormalizedAskMode::Batch
    };
    Ok(NormalizedAsk { mode, questions })
}

pub(super) fn exec_ask_user(args: &serde_json::Value) -> ToolResult {
    match normalize_ask_user(args) {
        Ok(ask) => ToolResult::ok(crate::json_ok(
            serde_json::to_value(ask).expect("NormalizedAsk serializes"),
        )),
        Err(error) => ToolResult::error(crate::json_err(
            error.code,
            format!("ask: {}", error.message),
            "Fix the ask arguments and retry.",
        )),
    }
}

handler!(handle_ask_user, exec_ask_user);

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "ask".to_string(),
        description: "Ask the user one or more questions when clarification or a user decision is needed. This opens a Ringing interaction and is not treated as an ordinary successful tool result.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Array of questions for multi-question prompts.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Unique ID (e.g. 'q1'). Auto-generated if omitted." },
                            "question": { "type": "string", "description": "The question text (supports Markdown)." },
                            "options": { "type": "array", "items": { "type": "string" }, "description": "Preset answer choices." },
                            "allow_custom": { "type": "boolean", "description": "Allow custom text input.", "default": true }
                        },
                        "required": ["question"]
                    }
                },
                "question": {
                    "type": "string",
                    "description": "[deprecated] Single question text. Use 'questions' array instead."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "[deprecated] Preset choices for single question."
                },
                "allow_custom": {
                    "type": "boolean",
                    "description": "[deprecated] Allow custom input for single question.",
                    "default": true
                }
            },
            "anyOf": [
                { "required": ["questions"] },
                { "required": ["question"] }
            ],
            "additionalProperties": false
        }),
        handler: handle_ask_user,
        risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Read,
        default_timeout: std::time::Duration::ZERO,
    },
    crate::ToolPlacement::Workspace,
);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_format_single_question() {
        let args = serde_json::json!({
            "question": "Choose A or B?",
            "options": ["A", "B"],
            "allow_custom": false
        });
        let result = exec_ask_user(&args);
        // Parse the JSON output
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        assert!(value.get("user_query").is_none());
        assert_eq!(value["mode"], "single");
        assert_eq!(value["questions"][0]["id"], "q1");
        assert_eq!(value["questions"][0]["question"], "Choose A or B?");
        assert_eq!(
            value["questions"][0]["options"].as_array().unwrap().len(),
            2
        );
        assert_eq!(value["questions"][0]["allow_custom"], false);
    }

    #[test]
    fn new_format_batch_questions() {
        let args = serde_json::json!({
            "questions": [
                { "id": "arch", "question": "Which architecture?", "options": ["A", "B", "C"] },
                { "id": "strat", "question": "Strategy?", "allow_custom": true }
            ]
        });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        assert!(value.get("user_query").is_none());
        assert_eq!(value["mode"], "batch");
        assert_eq!(value["questions"].as_array().unwrap().len(), 2);
        assert_eq!(value["questions"][1]["id"], "strat");
        assert_eq!(value["questions"][1]["allow_custom"], true);
    }

    #[test]
    fn auto_id_generation() {
        let args = serde_json::json!({
            "questions": [
                { "question": "Q1?" },
                { "question": "Q2?" },
                { "question": "Q3?" }
            ],
            "mode": "batch"
        });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        let qs = value["questions"].as_array().unwrap();
        assert_eq!(qs[0]["id"], "q1");
        assert_eq!(qs[1]["id"], "q2");
        assert_eq!(qs[2]["id"], "q3");
    }

    #[test]
    fn empty_questions_error() {
        let args = serde_json::json!({ "questions": [] });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "error");
    }

    #[test]
    fn missing_question_in_array_error() {
        let args = serde_json::json!({
            "questions": [
                { "id": "q1", "question": "Valid?" },
                { "id": "q2" }
            ]
        });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "error");
    }

    #[test]
    fn old_format_no_options() {
        let args = serde_json::json!({ "question": "What do you think?" });
        let result = exec_ask_user(&args);
        let value: serde_json::Value =
            serde_json::from_str(result.model_text()).expect("valid JSON");
        assert_eq!(value["status"], "ok");
        let q = &value["questions"][0];
        assert_eq!(q["question"], "What do you think?");
        assert!(q["options"].as_array().unwrap().is_empty());
        assert_eq!(q["allow_custom"], true);
    }

    #[test]
    fn multi_question_input_derives_batch_without_mode() {
        let ask = normalize_ask_user(&serde_json::json!({
            "questions": [
                {
                    "id": "arch",
                    "question": "Architecture?",
                    "options": ["A", "B"],
                    "allow_custom": false
                },
                { "question": "Strategy?", "allow_custom": true }
            ]
        }))
        .unwrap();

        assert_eq!(ask.mode, NormalizedAskMode::Batch);
        assert_eq!(ask.questions[0].id, "arch");
        assert_eq!(ask.questions[1].id, "q2");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "questions": [
                { "id": "same", "question": "First?", "allow_custom": true },
                { "id": "same", "question": "Second?", "allow_custom": true }
            ]
        }))
        .unwrap_err();

        assert_eq!(error.code, "DUPLICATE_QUESTION_ID");
    }

    #[test]
    fn duplicate_options_are_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Pick one",
            "options": ["A", "A"],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "DUPLICATE_OPTION");
    }

    #[test]
    fn unanswerable_question_is_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Blocked",
            "options": [],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "UNANSWERABLE_QUESTION");
    }

    #[test]
    fn blank_option_is_rejected_before_presenting_the_prompt() {
        let error = normalize_ask_user(&serde_json::json!({
            "question": "Blocked",
            "options": ["   "],
            "allow_custom": false
        }))
        .unwrap_err();

        assert_eq!(error.code, "INVALID_OPTION");
    }

    #[test]
    fn explicit_blank_question_id_is_rejected() {
        let error = normalize_ask_user(&serde_json::json!({
            "questions": [
                { "id": "  ", "question": "Question?", "allow_custom": true }
            ]
        }))
        .unwrap_err();

        assert_eq!(error.code, "INVALID_QUESTION_ID");
    }
}
