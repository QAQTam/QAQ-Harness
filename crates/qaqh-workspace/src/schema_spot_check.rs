#[cfg(test)]
#[allow(clippy::module_inception)]
mod schema_spot_check {
    use crate::registration::build_tool_manager;

    #[test]
    fn schema_descriptions_are_effective() {
        let defs = build_tool_manager(&[]).all_defs();
        let by_name = |n: &str| defs.iter().find(|d| d.function.name == n).unwrap();
        let params = |n: &str| &by_name(n).function.parameters["properties"];

        // process: action enum 带描述
        let pa = &params("process")["action"];
        assert!(
            pa["description"].as_str().unwrap().contains("check"),
            "process.action missing per-action description"
        );

        // read_image: anyOf 互斥（image_index / path）
        let img = &by_name("read_image").function.parameters;
        assert!(img["anyOf"].is_array(), "read_image missing anyOf");
        assert!(
            img["anyOf"][0]["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("image_index"))
        );

        // web_fetch: url required
        let web = &by_name("web_fetch").function.parameters;
        assert!(
            web["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("url")),
            "web_fetch.url not required"
        );

        // Todo v3（write/update/list 混合制）：
        // write = items-only + 空数组清空语义。
        let tw = params("todo_write");
        assert!(tw["items"].is_object(), "todo_write.items missing");
        assert!(
            tw["items"]["maxItems"].is_number(),
            "todo_write.items.maxItems missing"
        );
        // update = 单一形态（无 ids/updates，required [id, status]）。
        let tu = params("todo_update");
        assert!(
            tu.get("ids").is_none() && tu.get("updates").is_none(),
            "todo_update 应为单一形态（批量/updates 已移除）"
        );
        assert!(
            by_name("todo_update").function.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("status")),
            "todo_update required 应含 status"
        );
        assert!(
            !by_name("todo_update")
                .function
                .description
                .contains("updates"),
            "todo_update description 不应再宣传 updates 形态"
        );

        // 文件修改工具选择指引
        for (tool, needle) in [
            ("edit", "replace_all"),
            ("write", "use edit for targeted changes"),
        ] {
            let desc = by_name(tool).function.description.as_str();
            assert!(desc.contains(needle), "{tool} missing guidance: {needle}");
        }

        // read：单文件模式字段必须有描述（曾缺失导致模型不知 if_hash 语义）
        for field in ["path", "start_line", "end_line", "if_hash"] {
            let f = &params("read")[field];
            assert!(
                f["description"].as_str().is_some_and(|d| !d.is_empty()),
                "read.{field} missing description"
            );
        }
        let if_hash = &params("read")["if_hash"];
        assert!(
            if_hash["description"]
                .as_str()
                .unwrap()
                .contains("NOT_MODIFIED"),
            "read.if_hash description must explain NOT_MODIFIED"
        );

        // edit：描述不得残留历史版本号/旧名（schema 更新不及时的回归保护）
        let edit_desc = by_name("edit").function.description.as_str();
        assert!(
            !edit_desc.contains("v2"),
            "edit description must not mention v2"
        );
        assert!(
            !edit_desc.contains("edit_file_v2"),
            "edit description must not mention legacy name"
        );

        // edit：read 模式已移除（行号系统归 read 工具）——hunks required 且
        // schema 不得再宣传 read 形态。
        let edit_params = &by_name("edit").function.parameters;
        assert!(
            edit_params["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("hunks")),
            "edit must require hunks"
        );
        assert!(
            edit_params.get("oneOf").is_none()
                && edit_params["properties"].get("start_line").is_none(),
            "edit schema must not carry read-mode branches"
        );
        assert_eq!(
            edit_params["properties"]["hunks"]["minItems"].as_u64(),
            Some(1),
            "edit hunks must be non-empty"
        );
    }
}
