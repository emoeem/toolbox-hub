//! Recipe Loader（计划书 P5-01）：把精选 Recipe 注册成目录里的普通工具。
//!
//! ```text
//! registry/featured → Recipe Loader → Recipe Schema → Safety Policy → MCP Tool
//! ```
//!
//! 关键决定：**Loader 产出的不是「另一条执行通道」，而是普通的
//! [`ToolDefinition`]**。Recipe 进目录后走与 Manifest 工具完全相同的路径：
//! 名称规范化（`recipe:dns-lookup` → `toolbox_recipe_dns_lookup`）、JSON Schema
//! 生成、`mcp.toml` 授权、参数校验、网关执行、审计。MCP 接入因此不会削弱
//! `recipe run` 的任何安全检查 —— 那些检查要么在这里前置（`validate_recipe`、
//! SHA-256 校验），要么由策略层以更强的方式替代（见下）。
//!
//! 与 `recipe run`（`ecosystem::run_recipe_cli`）的对应关系：
//!
//! | recipe run 的检查 | Loader / 网关里的等价物 |
//! | --- | --- |
//! | `validate_recipe`（结构 + 高危命令闸） | 加载时原样调用；不过的条目跳过 |
//! | 参数名必须已声明、必填必须给值 | 参数成为声明的 [`Argument`]，由服务层校验 |
//! | caution 要显式 `--confirm` | `Danger::Caution` → 策略默认拒绝 + 确认门槛 |
//! | destructive 要显式审计三件套 | **不暴露**：模型无法携带该语义，宁缺毋滥 |
//!
//! # 无损转换（S-02 的前置）
//!
//! Recipe 的参数是「把 `{name}` 替换成值」的模板；[`Action`] 模型的 argv 是
//! `base_argv + 各参数值`。两者要**逐元素相等**才允许注册（否则命令语义就被
//! 接入改变了）。能证明无损的形状只有一种：
//!
//! ```text
//! [字面量…] {参数A} [单个字面量 → 作为参数B的 flag] {参数B} …
//! ```
//!
//! 嵌入式占位符（`1,{count}p`）、参数之间的多字面量、结尾的字面量都表达不了
//! —— 这样的条目**跳过并给警告**，绝不近似转换。实测精选 35 条里 31 条可无损
//! 注册（`sed-preview` / `find-files` / `archive-create` 之外的形状都简单）。
//!
//! # S-02（参数注入）
//!
//! 位置替换的值以 `-` 开头就会被目标程序当成选项。策略层对 provider 为
//! [`RECIPE_PROVIDER`] 的工具拒绝所有以 `-` 开头的参数值
//! （见 `security::policy::SafetyPolicy::check_recipe_values`），这一注入面清零。

use std::path::{Path, PathBuf};

use crate::{
    ecosystem::{self, Recipe, SafetyLevel},
    model::{Action, Argument, Danger, Domain, McpExposure, RunMode, ToolDefinition},
    providers::metadata,
    registry::Registry,
    security::policy::RECIPE_PROVIDER,
    service::catalog::ToolCatalog,
};

/// 一次加载的产物：可注册的工具 + 每条跳过原因（给 stderr 与测试）。
#[derive(Debug, Default)]
pub struct RecipeLoad {
    pub tools: Vec<ToolDefinition>,
    pub warnings: Vec<String>,
}

impl RecipeLoad {
    fn skip(&mut self, recipe_id: &str, reason: &str) {
        self.warnings
            .push(format!("recipe {recipe_id} 跳过：{reason}"));
    }
}

/// 加载全部配置目录里的精选 Recipe（P5-01）。
///
/// 单个文件 / 单条 Recipe 的问题只跳过那一条（附警告），不影响其余 —— 与
/// Manifest 发现的「逐文件容错」同一纪律。
pub fn load_dirs(dirs: &[PathBuf]) -> RecipeLoad {
    let mut load = RecipeLoad::default();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for dir in dirs {
        load_dir(dir, &mut load, &mut seen);
    }
    load
}

fn load_dir(dir: &Path, load: &mut RecipeLoad, seen: &mut std::collections::BTreeSet<String>) {
    let index_text = match std::fs::read_to_string(dir.join("index.json")) {
        Ok(text) => text,
        Err(error) => {
            load.warnings.push(format!(
                "recipe 目录 {} 跳过：读不到 index.json（{error}）",
                dir.display()
            ));
            return;
        }
    };
    let Ok(index) = serde_json::from_str::<serde_json::Value>(&index_text) else {
        load.warnings.push(format!(
            "recipe 目录 {} 跳过：index.json 不是合法 JSON",
            dir.display()
        ));
        return;
    };
    let Some(rows) = index.get("recipes").and_then(serde_json::Value::as_array) else {
        load.warnings.push(format!(
            "recipe 目录 {} 跳过：index.json 缺 recipes 数组",
            dir.display()
        ));
        return;
    };

    for row in rows {
        let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
            load.warnings.push(String::from("recipe 索引行缺 id，跳过"));
            continue;
        };
        let Some(file) = row.get("file").and_then(serde_json::Value::as_str) else {
            load.skip(id, "索引行缺 file");
            continue;
        };
        let path = dir.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            load.skip(id, &format!("读不到 {}", path.display()));
            continue;
        };
        // SHA-256 以 index.json 为准：文件被改过（哪怕只是手滑保存）就不注册，
        // 与 `recipe check` / `build-featured.py --check` 的口径一致。
        if let Ok(actual) = ecosystem::sha256(&path) {
            let expected = row.get("sha256").and_then(serde_json::Value::as_str);
            if expected.is_some_and(|expected| expected != actual) {
                load.skip(id, "SHA-256 与 index.json 不符");
                continue;
            }
        }
        let Ok(recipe) = serde_json::from_str::<Recipe>(&text) else {
            load.skip(id, "不是合法的 Recipe JSON");
            continue;
        };
        if let Err(error) = ecosystem::validate_recipe(&recipe) {
            load.skip(id, &error);
            continue;
        }
        if recipe.id != id {
            load.skip(id, &format!("文件内 id 是 {}，与索引不符", recipe.id));
            continue;
        }
        if !seen.insert(recipe.id.clone()) {
            load.skip(id, "重复 id（先注册的胜出）");
            continue;
        }
        match to_tool(&recipe) {
            Ok(tool) => load.tools.push(tool),
            Err(reason) => load.skip(id, &reason),
        }
    }
}

/// 一条 Recipe → 一件目录工具。失败 = 无法无损表达，返回原因。
fn to_tool(recipe: &Recipe) -> Result<ToolDefinition, String> {
    if recipe.safety == SafetyLevel::Destructive {
        return Err(String::from(
            "destructive 的 Recipe 不通过 MCP 暴露（工具模型携带不了这套审计语义）",
        ));
    }
    if recipe.command.chars().any(char::is_whitespace) {
        return Err(format!(
            "command「{}」不是单个程序名，无法表达",
            recipe.command
        ));
    }
    let shape = Shape::parse(&recipe.args, &recipe.params)?;
    let danger = match recipe.safety {
        SafetyLevel::Safe => Danger::Safe,
        SafetyLevel::Caution => Danger::Caution,
        SafetyLevel::Destructive => unreachable!("destructive 已在上面返回"),
    };

    let deps = metadata::dependencies_of(std::slice::from_ref(&recipe.command));
    let ready = deps.is_ready();
    Ok(ToolDefinition {
        mcp: Some(McpExposure::default()),
        id: format!("recipe:{}", recipe.id),
        name: recipe.title.trim().to_string(),
        provider: String::from(RECIPE_PROVIDER),
        domain: domain_of(&recipe.tags),
        tags: recipe.tags.clone(),
        summary: if recipe.description.trim().is_empty() {
            recipe.title.trim().to_string()
        } else {
            recipe.description.trim().to_string()
        },
        input: None,
        output: None,
        features: None,
        requires: deps.required,
        missing_deps: deps.missing,
        install_hint: None,
        pin: None,
        action: Some(Action {
            program: recipe.command.clone(),
            base_argv: shape.base_argv,
            arguments: shape.arguments,
            duration_from: None,
            limit_from: None,
            foreach: None,
            ok_exit_codes: vec![0],
            // Recipe 允许「所有可选参数都不填」地跑（与 recipe run 一致）：
            // 必填参数由 Argument::required 单独拦，allow_empty 只管「一个都不给」。
            allow_empty: true,
        }),
        mode: RunMode::Capture,
        danger,
        path: metadata::resolve_program(&recipe.command)
            .unwrap_or_else(|| PathBuf::from(&recipe.command)),
        ready,
    })
}

/// 从标签猜域：第一个能解析成域的标签胜出，猜不出归「工具」域。
fn domain_of(tags: &[String]) -> Domain {
    tags.iter()
        .find_map(|tag| Domain::parse(tag))
        .unwrap_or(Domain::Tools)
}

/// 无损形状解析的结果。
struct Shape {
    base_argv: Vec<String>,
    arguments: Vec<Argument>,
}

impl Shape {
    /// 把 `args` 模板解析成 `base_argv + 参数声明`，并证明逐元素无损。
    ///
    /// 规则（见模块文档）：
    ///
    /// 1. 每个元素要么是纯字面量，要么恰好是**一个**完整占位符 `{name}`；
    /// 2. 首段字面量（第一个占位符之前）进 `base_argv`；
    /// 3. 占位符之间的**单个**字面量成为下一个参数的 flag（值仍在自己的元素里）；
    /// 4. 占位符之间的多个字面量、结尾的字面量、一个元素里多个/部分占位符 ——
    ///    都表达不了，报错跳过；
    /// 5. 每个占位符必须对应一条已声明参数，且每条声明参数都要被用到。
    fn parse(args: &[String], params: &[ecosystem::RecipeParam]) -> Result<Shape, String> {
        let declared = |name: &str| {
            params
                .iter()
                .find(|param| param.name == name)
                .ok_or_else(|| format!("模板参数 {name} 未声明"))
        };

        let mut base_argv = Vec::new();
        let mut arguments: Vec<Argument> = Vec::new();
        let mut used: Vec<String> = Vec::new();
        // 相邻占位符之间的那一个字面量（下一个参数的 flag 候选）。
        let mut pending_flag: Option<String> = None;
        let mut in_params = false;

        for element in args {
            match placeholder_name(element) {
                Err(reason) => return Err(reason),
                Ok(None) => {
                    if !in_params {
                        base_argv.push(element.clone());
                    } else if pending_flag.is_some() {
                        return Err(format!(
                            "占位符之间出现多个字面量（{:?} 之后又有 {:?}），无法无损表达",
                            pending_flag, element
                        ));
                    } else {
                        pending_flag = Some(element.clone());
                    }
                }
                Ok(Some(name)) => {
                    in_params = true;
                    let param = declared(&name)?;
                    if used.contains(&name) {
                        return Err(format!("参数 {name} 在模板里出现多次，无法无损表达"));
                    }
                    used.push(name.clone());
                    arguments.push(Argument {
                        key: name,
                        label: if param.description.trim().is_empty() {
                            param.name.clone()
                        } else {
                            param.description.trim().to_string()
                        },
                        kind: crate::model::ArgKind::Text,
                        default: None,
                        choices: Vec::new(),
                        required: param.required,
                        flag: pending_flag.take(),
                        flag_join: false,
                        // 位置参数：排在 base_argv 之后、按声明顺序。
                        placement: crate::model::ArgPlacement::Trailing,
                        sensitive: false,
                        repeatable: false,
                        repeat_flag: false,
                        separator: String::from(","),
                        source: None,
                        dir_only: false,
                        help: None,
                    });
                }
            }
        }
        if let Some(trailing) = pending_flag {
            return Err(format!(
                "最后一个占位符之后还有字面量（{trailing:?}），无法无损表达"
            ));
        }
        for param in params {
            if !used.contains(&param.name) {
                return Err(format!("声明参数 {} 没有出现在模板里", param.name));
            }
        }
        Ok(Shape {
            base_argv,
            arguments,
        })
    }
}

/// 元素恰好是一个完整占位符 `{name}` 时返回 `Some(name)`；
/// 嵌入/多个占位符返回错误；纯字面量返回 `None`。
fn placeholder_name(element: &str) -> Result<Option<String>, String> {
    let Some(open) = element.find('{') else {
        return Ok(None);
    };
    let Some(close) = element[open + 1..].find('}') else {
        return Err(format!("元素「{element}」的花括号不匹配"));
    };
    let name = &element[open + 1..open + 1 + close];
    if open != 0 || open + close + 2 != element.len() {
        return Err(format!(
            "元素「{element}」里占位符不是独占整个参数，无法无损表达"
        ));
    }
    Ok(Some(name.to_string()))
}

/// Registry 快照 + Recipe 工具 → 一份合并目录（MCP 入口 / 热更新 / CLI JSON 共用）。
///
/// Manifest 工具在前、Recipe 在后：`ToolCatalog` 对重复 id「先注册的胜出」，
/// 所以用户自己的 manifest 稳定压过同名 Recipe —— 与 TUI 的 Provider 优先级
/// 语义一致。
pub fn catalog_with_recipes(
    registry: &Registry,
    recipe_dirs: &[PathBuf],
) -> (ToolCatalog, Vec<String>) {
    let load = load_dirs(recipe_dirs);
    let mut tools = registry.tools().to_vec();
    tools.extend(load.tools);
    (ToolCatalog::new(tools), load.warnings)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{load_dirs, placeholder_name};
    use crate::{
        ecosystem::Recipe,
        model::{ArgKind, Danger, Domain, RunMode, TypedValues},
        security::policy::RECIPE_PROVIDER,
    };

    fn recipe(id: &str, command: &str, args: &[&str], params: &[&str], safety: &str) -> Recipe {
        serde_json::from_str(
            &serde_json::json!({
                "id": id,
                "title": format!("标题 {id}"),
                "description": format!("说明 {id}"),
                "command": command,
                "params": params.iter().map(|name| serde_json::json!({
                    "name": name, "description": name, "required": true
                })).collect::<Vec<_>>(),
                "args": args,
                "safety": safety,
                "source": "toolbox-hub/featured",
                "tags": ["工具"],
            })
            .to_string(),
        )
        .expect("测试 Recipe 应能解析")
    }

    /// 把 Recipe 写进一个临时目录（含 index.json 与正确 SHA-256），跑加载。
    struct Dir {
        path: PathBuf,
    }

    impl Dir {
        fn with(recipes: &[Recipe]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "toolbox-hub-recipe-it-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("mkdir");
            let mut rows = Vec::new();
            for recipe in recipes {
                let file = format!("{}.json", recipe.id);
                std::fs::write(
                    path.join(&file),
                    serde_json::to_string_pretty(recipe).expect("序列化"),
                )
                .expect("写 recipe");
                rows.push(serde_json::json!({
                    "id": recipe.id,
                    "file": file,
                    "sha256": crate::ecosystem::sha256(&path.join(&file)).expect("哈希"),
                }));
            }
            std::fs::write(
                path.join("index.json"),
                serde_json::json!({ "schema_version": 1, "recipes": rows }).to_string(),
            )
            .expect("写 index");
            Self { path }
        }

        fn load(&self) -> super::RecipeLoad {
            load_dirs(std::slice::from_ref(&self.path))
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// recipe run 的参数替换语义（测试内的第二实现，用来对照 Loader 的 argv）。
    fn recipe_run_argv(recipe: &Recipe, values: &[(&str, &str)]) -> Vec<String> {
        recipe
            .args
            .iter()
            .map(|arg| {
                let mut value = arg.clone();
                for (key, replacement) in values {
                    value = value.replace(&format!("{{{key}}}"), replacement);
                }
                value
            })
            .collect()
    }

    /// Loader 产出的 Action 构造的 argv 必须与 recipe run 的替换结果逐元素相等。
    #[test]
    fn converted_actions_reproduce_recipe_run_argv_element_for_element() {
        let cases = [
            // 末尾参数（git-log 形状）
            (
                "a",
                "git",
                vec!["log", "--oneline", "-n", "{count}"],
                vec!["count"],
            ),
            // 占位符之间夹一个字面量（archive-create 形状：作为下一个参数的 flag）
            (
                "b",
                "tar",
                vec!["-czf", "{output}", "-C", "{parent}", "{name}"],
                vec!["output", "parent", "name"],
            ),
            // 开头一串字面量（text-search 形状）
            (
                "c",
                "grep",
                vec!["-RIn", "--exclude-dir=.git", "--", "{pattern}", "{path}"],
                vec!["pattern", "path"],
            ),
            // 无参数（kernel-version 形状）
            ("d", "uname", vec!["-r"], vec![]),
            // 第一个元素就是占位符
            ("e", "printenv", vec!["{name}"], vec!["name"]),
        ];
        for (id, command, args, params) in cases {
            let recipe = recipe(id, command, &args, &params, "safe");
            let tool = super::to_tool(&recipe).unwrap_or_else(|e| panic!("{id}: {e}"));
            let action = tool.action.as_ref().expect("action");
            assert_eq!(action.program, command, "{id}");
            assert_eq!(tool.provider, RECIPE_PROVIDER, "{id}");
            assert_eq!(tool.id, format!("recipe:{id}"), "{id}");

            let mut values = TypedValues::new();
            let pairs: Vec<(String, String)> = params
                .iter()
                .enumerate()
                .map(|(index, name)| ((*name).to_string(), format!("值-{index} x")))
                .collect();
            for (name, value) in &pairs {
                values.set_text(name, value);
            }
            let argv = action.build_argv_typed(&values).expect("构建 argv");
            let expected = recipe_run_argv(
                &recipe,
                &pairs
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect::<Vec<_>>(),
            );
            assert_eq!(argv, expected, "{id} 的 argv 必须与 recipe run 逐元素一致");
        }
    }

    /// 表达不了的形状必须跳过并说明原因，绝不近似转换。
    #[test]
    fn shapes_that_cannot_be_expressed_losslessly_are_skipped_with_reasons() {
        let cases = [
            // 嵌入式占位符（sed-preview 形状）
            (vec!["-n", "1,{count}p", "{path}"], vec!["count", "path"]),
            // 占位符之间多个字面量（find-files 形状）
            (
                vec!["{path}", "-type", "f", "-iname", "{pattern}"],
                vec!["path", "pattern"],
            ),
            // 结尾字面量
            (vec!["{a}", "tail"], vec!["a"]),
            // 一个占位符出现两次
            (vec!["{a}", "-x", "{a}"], vec!["a"]),
        ];
        for (args, params) in cases {
            let recipe = recipe("shape", "echo", &args, &params, "safe");
            let error = super::to_tool(&recipe).expect_err("应跳过");
            assert!(
                error.contains("无法无损表达") || error.contains("没有出现在模板里"),
                "{error}"
            );
        }

        // 声明了但模板里没用的参数同样跳过。
        let recipe = recipe("unused", "echo", &["{a}"], &["a", "b"], "safe");
        let error = super::to_tool(&recipe).expect_err("应跳过");
        assert!(error.contains("没有出现在模板里"), "{error}");
    }

    #[test]
    fn destructive_and_dangerous_recipes_never_register() {
        let dir = Dir::with(&[
            recipe("wipe", "shred", &["{path}"], &["path"], "destructive"),
            // safe 命令带危险词：validate_recipe 直接拒绝
            recipe("bad", "rm", &["-rf", "{path}"], &["path"], "safe"),
            recipe("ok", "echo", &["{text}"], &["text"], "safe"),
        ]);
        let load = dir.load();
        assert_eq!(
            load.tools
                .iter()
                .map(|tool| tool.id.clone())
                .collect::<Vec<_>>(),
            vec![String::from("recipe:ok")],
            "destructive 与校验不过的都不注册：{:?}",
            load.warnings
        );
        assert_eq!(load.warnings.len(), 2, "{:?}", load.warnings);
    }

    #[test]
    fn sha256_mismatch_and_broken_files_are_skipped() {
        let dir = Dir::with(&[recipe("good", "echo", &["hi"], &[], "safe")]);
        // 篡改文件内容（index.json 里的 SHA-256 不再匹配）。
        std::fs::write(dir.path.join("good.json"), "{\"id\":\"good\"}").expect("改写");
        let load = dir.load();
        assert!(load.tools.is_empty());
        assert!(
            load.warnings
                .iter()
                .any(|warning| warning.contains("SHA-256")),
            "{:?}",
            load.warnings
        );

        // index.json 缺失 → 整个目录跳过，一句警告。
        let empty = std::env::temp_dir().join(format!(
            "toolbox-hub-recipe-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty).expect("mkdir");
        let load = super::load_dirs(std::slice::from_ref(&empty));
        assert!(load.tools.is_empty());
        assert!(
            load.warnings
                .iter()
                .any(|warning| warning.contains("index.json"))
        );
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn dependency_state_and_danger_follow_the_recipe() {
        let dir = Dir::with(&[
            recipe("present", "echo", &["{text}"], &["text"], "safe"),
            recipe(
                "absent",
                "no-such-command-xyz",
                &["{text}"],
                &["text"],
                "caution",
            ),
        ]);
        let load = dir.load();
        assert_eq!(load.tools.len(), 2, "{:?}", load.warnings);

        let present = &load.tools[0];
        assert!(present.ready);
        assert_eq!(present.danger, Danger::Safe);
        assert_eq!(present.mode, RunMode::Capture);
        assert!(
            present.mcp.is_some(),
            "Loader 声明暴露，授权仍由 mcp.toml 决定"
        );

        let absent = &load.tools[1];
        assert!(!absent.ready);
        assert_eq!(
            absent.missing_deps,
            vec![String::from("no-such-command-xyz")]
        );
        assert_eq!(absent.danger, Danger::Caution);
        // 策略层会对未就绪的工具拒绝（dependencies_missing），Loader 不需要额外闸。
    }

    #[test]
    fn domain_comes_from_the_first_resolvable_tag() {
        let mut recipe = recipe("dom", "echo", &["hi"], &[], "safe");
        recipe.tags = vec![String::from("不认识"), String::from("网络")];
        let tool = super::to_tool(&recipe).expect("转换");
        assert_eq!(tool.domain, Domain::Network);

        recipe.tags = vec![String::from("归档")];
        let tool = super::to_tool(&recipe).expect("转换");
        assert_eq!(tool.domain, Domain::Tools, "解析不了的标签归工具域");
    }

    #[test]
    fn placeholder_name_only_accepts_full_element_placeholders() {
        assert_eq!(placeholder_name("{host}"), Ok(Some(String::from("host"))));
        assert_eq!(placeholder_name("-n"), Ok(None));
        assert!(
            placeholder_name("--flag={v}")
                .expect_err("嵌入")
                .contains("独占")
        );
        assert!(
            placeholder_name("{a}{b}")
                .expect_err("多个")
                .contains("独占")
        );
        assert!(placeholder_name("{open").is_err(), "花括号不匹配");
    }

    /// 合并入口：Manifest 工具在前、Recipe 在后，重复 id 先到者胜。
    #[test]
    fn catalog_merge_keeps_manifest_tools_ahead_of_recipes() {
        let dir = Dir::with(&[recipe("echo", "echo", &["{text}"], &["text"], "safe")]);
        let _unused = dir.load().warnings;

        let manifest_tool = crate::model::ToolDefinition {
            mcp: None,
            id: String::from("recipe:echo"),
            name: String::from("同名 Manifest 动作"),
            provider: String::from("Test"),
            domain: Domain::Tools,
            tags: Vec::new(),
            summary: String::new(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action: None,
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from("/bin/echo"),
            ready: true,
        };
        let registry = crate::registry::Registry::from_tools(vec![manifest_tool]);
        let (catalog, warnings) =
            super::catalog_with_recipes(&registry, &[PathBuf::from(&dir.path)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(catalog.len(), 1, "重复 id：manifest 胜出");
        assert_eq!(
            catalog.get("recipe:echo").expect("在目录里").name,
            "同名 Manifest 动作"
        );
    }

    /// 冒烟：真 featured 目录（仓库内容）应当能无损注册绝大部分条目。
    /// 当前已知 3 条表达不了（sed-preview / find-files 的形状 + archive-create 可表达），
    /// 这里只断言「能注册的是多数、跳过的都有理由」，防止 Loader 静默退化。
    #[test]
    fn the_real_featured_directory_loads_mostly_losslessly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("registry/featured");
        let load = super::load_dirs(&[dir]);
        assert!(
            load.tools.len() >= 30,
            "featured 35 条里至少 30 条应能注册：{}（{:?}）",
            load.tools.len(),
            load.warnings
        );
        for tool in &load.tools {
            assert!(tool.id.starts_with("recipe:"));
            assert_eq!(tool.provider, RECIPE_PROVIDER);
            assert!(matches!(tool.danger, Danger::Safe | Danger::Caution));
            // 每个参数声明都能构造 argv：必填给了值以后不报错。
            if let Some(action) = &tool.action {
                let values = action.default_values();
                let filled: TypedValues = {
                    let mut typed = TypedValues::new();
                    for argument in &action.arguments {
                        let raw = values.get(&argument.key).unwrap_or_default().to_string();
                        let value = if raw.is_empty() {
                            format!("x{argument_key}", argument_key = argument.key)
                        } else {
                            raw
                        };
                        typed.set_text(&argument.key, value);
                    }
                    typed
                };
                assert!(
                    action.build_argv_typed(&filled).is_ok(),
                    "{} 的参数声明应能构建 argv",
                    tool.id
                );
            }
        }
        for warning in &load.warnings {
            assert!(warning.contains("跳过"), "{warning}");
        }
    }

    #[test]
    fn arg_kind_is_text_for_every_recipe_parameter() {
        let dir = Dir::with(&[recipe(
            "kinds",
            "echo",
            &["{a}", "{b}"],
            &["a", "b"],
            "safe",
        )]);
        let load = dir.load();
        let tool = &load.tools[0];
        let action = tool.action.as_ref().expect("action");
        assert!(
            action
                .arguments
                .iter()
                .all(|argument| argument.kind == ArgKind::Text)
        );
        assert!(action.arguments.iter().all(|argument| argument.required));
    }
}
