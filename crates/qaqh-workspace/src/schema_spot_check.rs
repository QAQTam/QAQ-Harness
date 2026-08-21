#[cfg(test)]
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

        // image: anyOf 互斥
        let img = &by_name("image").function.parameters;
        assert!(img["anyOf"].is_array(), "image missing anyOf");
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

        // todo: id 描述
        let tid = &params("todo")["id"];
        assert!(
            tid["description"]
                .as_str()
                .unwrap()
                .contains("Omit for action=create"),
            "todo.id description missing"
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

        // edit：读/编辑模式 oneOf 互斥（P3）——编辑分支要求 hunks 非空且
        // 禁止读模式字段；读分支拒绝非空 hunks。
        let edit_params = &by_name("edit").function.parameters;
        let one_of = edit_params["oneOf"]
            .as_array()
            .expect("edit missing oneOf read/edit branches");
        assert_eq!(one_of.len(), 2, "edit oneOf must have edit+read branches");
        assert!(
            one_of[0]["required"]
                .as_array()
                .is_some_and(|r| r.contains(&serde_json::json!("hunks"))),
            "edit branch must require hunks"
        );
        assert_eq!(
            one_of[0]["properties"]["hunks"]["minItems"].as_u64(),
            Some(1),
            "edit branch hunks must be non-empty"
        );
        assert!(
            one_of[1]["not"].is_object(),
            "read branch must reject non-empty hunks via not"
        );
    }
}
