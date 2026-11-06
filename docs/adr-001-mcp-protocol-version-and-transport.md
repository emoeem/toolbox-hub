# ADR-001：MCP 协议版本与传输实现方式

> 状态：Accepted | 日期：2026-11-07 | 关联：P0-08（基线审计遗留）、P3（stdio Server）

## 背景

P0 审计（`p0-baseline-audit.md` §遗留）明确 P0-08 留待 P3 前补：确定 MCP 协议版本与 RMCP 依赖决策。本 ADR 关闭该项。

核实到的事实（2026-11-07）：

- MCP 规范已发布五个版本：2024-11-05、2025-03-26、2025-06-18、2025-11-25、2026-07-28（当前最新）。
- 官方 Rust SDK 为 `rmcp`（modelcontextprotocol/rust-sdk），覆盖 server/client 与宏生成。
- 规范要求版本协商：客户端在 initialize 提出版本，服务端不支持时必须回自己支持的版本，由客户端决定是否继续；不得盲目回显更高版本（等于宣称实现了未实现的语义）。

## 决策

1. **协议版本钉在 2025-06-18**，协商时永远回该版本。P3 所需面（initialize / tools/list / tools/call / elicitation 确认 / ping / notifications）在该版本内完整可用。
2. **传输与协议层自研**（`src/mcp/protocol.rs` + `server.rs`，行分隔 JSON over stdio），不引入 `rmcp`。
3. **升级触发条件**（满足其一即重开本 ADR）：
   - P4 接入 MCP Tasks（Tasks 是 2025-11-25 起的规范能力，届时需把实现升级到该版本并扩展协商）；
   - P5-05 HTTP/SSE 传输立项（stdio 行协议的代码不适用）；
   - 客户端生态普遍要求更高版本才可用。

## 理由

- 所需协议面很小：五个方法 + 通知，行分隔 JSON 直读直写；自研约 150 行协议代码，rmcp 引入的是整套 async 运行时与依赖树。
- 服务器的价值与风险都在网关与策略（`security/`、`service/gateway.rs`），不在传输层；依赖面越小，安全审计面越小。仓库同时要求 `--no-default-features` 可构建，自研层不添依赖即天然满足。
- rmcp 在本项目当前需求下没有不可替代的能力；未来需要 Tasks/HTTP 时再评估引入或继续自研升级，成本可控（协议层代码集中且带集成测试）。

## 后果

- 必须自行跟踪规范修订（见上面升级触发条件），不能依赖 SDK 升级带来兼容。
- 对请求 2025-11-25 / 2026-07-28 的客户端，服务端按规范降级协商到 2025-06-18（集成测试 `server_negotiates_to_the_version_it_implements` 覆盖）；这些客户端若不接受降级则连接失败，属预期行为。
- Elicitation（确认流程）按 2025-06-18 语义实现；若升级到 2025-11-25+，需核对确认相关字段是否有破坏性变更。

## 参考

- 规范版本历史：modelcontextprotocol.io/specification（2025-06-18 与 2026-07-28 页面）
- 官方 Rust SDK：github.com/modelcontextprotocol/rust-sdk
