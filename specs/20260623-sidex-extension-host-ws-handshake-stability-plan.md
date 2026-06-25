# SideX Node 扩展宿主 WebSocket 握手稳定性计划

日期：2026-06-23

## 背景

此前 SideX 启动后出现过“找不到 ZuAn 扩展 / ZuAn view 没有正常运行”的现象。经过启动链路分析，这类问题不一定来自 `extensions-meta.json`、内置扩展元数据或 SQLite storage 损坏；更可能的主因是 SideX 的 Node 扩展宿主没有真正跑起来，或者 WebView 与 Node 扩展宿主之间的 WebSocket 握手没有完成。

当前链路是：

```text
Workbench WebView
  -> Tauri invoke("extension_platform_bootstrap")
  -> Rust ExtensionPlatformSupervisor
  -> node extension-host/server.cjs
  -> server.cjs fork host.cjs
  -> Workbench WebView 连接 ws://127.0.0.1:<port>
  -> server.cjs 发送 sidex:handshake
  -> Workbench 开始 activateExtension
```

这个链路中，Rust 只等待 `server.cjs` 输出端口，并不等待前端 WebSocket 握手完成，也不等待 `host.cjs` 初始化完成。因此“端口启动成功”不等于“扩展宿主可用”。

本计划的核心判断是：

```text
Rust 收到 port
  ≠ WebSocket 连接成功
WebSocket 连接成功
  ≠ host.cjs 已可接受扩展激活
host.cjs ready
  ≠ 某个扩展已成功 activate
扩展 activate 成功
  ≠ 对应 view provider 已完成注册和渲染
```

## 目标

让 SideX Node 扩展宿主启动状态可观测、可诊断、可恢复，并减少“握手成功但扩展未真正可用”的假阳性。

最低目标：

- 可以明确区分 Node runtime 不存在、`server.cjs` 启动失败、WebSocket 连接失败、`host.cjs` 初始化失败、扩展 activate 失败。
- ZuAn 扩展未出现时，日志能直接指出断在哪一段。
- 前端不在 `host.cjs` 未 ready 时批量激活扩展。
- 重连和重启行为可控，不再静默停在 `handshake not ready`。
- 启动、握手、请求、激活结果具有可关联的 `sessionId` / `requestId`。

## 非目标

- 不重写完整 VS Code extension host 协议。
- 不把 Node 扩展宿主立即替换为纯 Tauri event/invoke 架构。
- 不在本轮修复 ZuAn 业务层 API 兼容问题。
- 不把所有 SideX 扩展扫描和元数据生成问题混入本计划；本计划只处理 Node 扩展宿主启动与握手。

## 状态模型

扩展宿主启动与连接状态应显式建模，避免用单个 `handshakeReady` 布尔值表达所有阶段。

建议状态：

```text
Idle
  -> Bootstrapping
  -> ServerStarting
  -> ServerListening
  -> WsConnecting
  -> WsConnected
  -> HostStarting
  -> HostReady
  -> ActivatingExtensions
  -> Ready
```

失败状态：

```text
NodeMissing
ServerScriptMissing
ServerStartFailed
PortTimeout
WsConnectFailed
WsClosed
HostReadyTimeout
HostExited
ActivationTimeout
ActivationFailed
Degraded
```

状态含义：

- `ServerListening`：Rust 已从 `server.cjs` 收到可信端口，说明 WebSocket server 已监听。
- `WsConnected`：Workbench WebView 已连接 WebSocket。
- `HostReady`：`host.cjs` 已完成初始化，且可接受 `activateExtension` 请求。
- `Ready`：至少目标扩展或当前需要的扩展已完成激活，相关 provider 注册流程已执行。
- `Degraded`：宿主出现 crash loop、恢复失败或存在不可自动恢复错误，只允许用户手动重启或查看诊断。

## 全链路关联字段

每次 `extension_platform_bootstrap` 应生成一个 `sessionId`，贯穿 Rust、Node server、Node host、Workbench WebView 日志与协议消息。

要求：

- Rust bootstrap 创建或返回 `sessionId`。
- `server.cjs` 启动参数或 init data 中包含 `sessionId`。
- `host.cjs` IPC ready、exit、activation result 中包含 `sessionId`。
- Workbench WebView 所有握手、重连、activation 日志包含 `sessionId`。
- 每个 request/response 消息必须包含 `requestId`。
- 事件型消息如 `host-exited`、`activation-result` 应包含相关 `requestId`，如果没有则包含 `eventId`。

示例：

```json
{
  "sessionId": "ext-host-20260623-abcdef",
  "requestId": "req-000001",
  "type": "activateExtension",
  "extensionId": "zuan.zuan-vscode-ext"
}
```

## stdout 协议约束

当前 Rust 读取 `server.cjs` stdout 第一行作为端口消息：

```json
{"port": 12345}
```

这个机制容易被普通日志污染。后续应明确：

- `stdout` 只输出机器可解析协议消息。
- 普通日志一律写入 `stderr`。
- Rust 不应盲目相信 stdout 第一行；应识别固定协议前缀或固定 JSON 类型。
- 端口消息必须带 `sessionId` 和 `type`。

建议格式：

```text
SIDEX_EXT_HOST_PORT {"type":"sidex:server-port","sessionId":"ext-host-20260623-abcdef","port":12345}
```

Rust 只接受符合上述格式且 `sessionId` 匹配当前 bootstrap 的端口消息。其他 stdout 行应记录为协议异常，并纳入启动失败诊断。

## 当前失败点

### 1. Node runtime 解析失败

`extension_platform_bootstrap` 依赖 `resolve_node_runtime()`。如果用户机器没有可用 Node，或者打包版没有内置 Node，扩展宿主不会启动。

风险表现：

```text
[ExtHost] platform bootstrap failed Node runtime not found...
```

需要记录：

- Node 查找候选列表。
- 最终选中的 Node 路径、版本和来源。
- 失败时的完整错误。

### 2. `server.cjs` 路径或资源打包失败

Rust 会尝试解析 `extension-host/server.cjs`。开发态和打包态路径不同，如果资源未打入包或 fallback 路径不可用，宿主启动失败。

需要记录：

- `resolve_server_script()` 的最终路径。
- 文件是否存在。
- 当前工作目录。
- Tauri resource 路径解析结果。

### 3. `server.cjs` 启动但没有输出端口

Rust 只读取 stdout 第一行作为端口消息。如果 Node 脚本语法错误、require 失败、端口监听失败，Rust 会失败，但当前 stderr 只在部分错误路径拼接，后续可见性不足。

需要改进：

- 捕获并持久化最近 stderr。
- 端口读取超时，而不是无限等待或只依赖 EOF。
- 错误中包含 `sessionId`、`init_data_file`、`server_js`、Node path、stderr 摘要。
- 端口消息使用固定 stdout 协议，避免被日志污染。

### 4. WebSocket 被 WebView 或环境阻断

前端连接：

```text
ws://127.0.0.1:<port>/
```

可能受 mixed content、WebView 本地网络权限、安全软件、端口占用、短暂启动慢等影响。

需要记录：

- `ws.onopen`、`ws.onerror`、`ws.onclose` 的时间和状态码。
- endpoint。
- 当前页面 origin。
- `sessionId`。
- 重连次数和最终放弃原因。

### 5. `sidex:handshake` 早于 `host.cjs` ready

`server.cjs` 当前在 `cp.fork(host.cjs)` 后立即发送 `sidex:handshake`。这只能证明 server 已连接，并不能证明真正的扩展宿主已初始化完成。

风险表现：

```text
前端收到 handshake
前端批量发送 activateExtension
host.cjs 尚未 ready 或刚 crash
ZuAn 未激活
```

需要改造为分层状态：

```text
serverListening: Rust 已拿到 port
wsConnected: WebView 已连接 WebSocket
hostReady: host.cjs 已完成初始化并可接受 activateExtension
```

### 6. 批量激活所有扩展过早且难排查

前端收到 `sidex:handshake` 后会对握手列表中的每个扩展发送 `activateExtension`。如果某个扩展卡住、报错或污染全局，ZuAn 的失败可能被淹没。

需要改进：

- 为每个 `activateExtension` 记录 id、开始时间、结果、耗时、错误。
- 优先激活贡献当前 view/command 的扩展，而不是无差别批量激活所有扩展。
- 对 ZuAn 这类目标扩展提供显式诊断日志。

## 实施计划

### 阶段 1：诊断增强

目标：不改变协议行为，先把断点暴露出来。

任务：

1. Rust `spawn_host_process` 增加结构化日志：
   - `sessionId`。
   - Node runtime path/version/source。
   - `server.cjs` 路径。
   - extension search paths。
   - scanned extension ids。
   - init data file 路径。
   - child pid。
   - port received 时间。
2. Rust 记录 Node server 退出：
   - port 前退出。
   - port 后退出。
   - exit code/signal。
   - 最近 stderr 摘要。
3. Rust 增加端口读取超时和 stdout 协议校验：
   - 只接受 `SIDEX_EXT_HOST_PORT` 或指定 `type` 的端口消息。
   - 非协议 stdout 记录为 warning。
4. 前端 `_connect` 增加日志：
   - `sessionId`。
   - ws endpoint。
   - open/error/close。
   - reconnect attempt。
   - handshake latency。
5. `server.cjs` 增加日志：
   - received init data extension count。
   - websocket client connected。
   - server/session ready sent。
   - host.cjs spawned pid。
   - host.cjs ready / exit / stderr。
6. `host.cjs` 增加日志：
   - init data loaded。
   - extension registry initialized。
   - IPC handlers installed。
   - ready sent。

验收：

```text
启动一次 SideX，日志能按 sessionId 显示：
bootstrap invoked
node resolved
server spawned
port received
ws open
server session established
host starting
host ready
zuan activation request
zuan activation result
provider registration result
```

### 阶段 2：握手拆分

目标：避免前端把 `server.cjs` ready 误判为 extension host ready。

任务：

1. 区分三层状态：
   - `serverListening`：Rust 已收到端口。
   - `wsConnected` / `serverSession`：Workbench WebSocket 已连上 server。
   - `hostReady`：`host.cjs` 已初始化完毕，可接受 activation request。
2. `server.cjs` 在 WebSocket 连接建立后向前端发送会话消息，例如：

```json
{
  "type": "sidex:server-session",
  "sessionId": "ext-host-20260623-abcdef",
  "serverReady": true
}
```

该消息只表示 WebSocket server 与当前前端连接可用，不表示扩展宿主可激活。

3. `host.cjs` 初始化完成后，通过 IPC 给 `server.cjs` 发送：

```json
{
  "type": "sidex:host-ready",
  "sessionId": "ext-host-20260623-abcdef",
  "extensionCount": 12,
  "extensionIds": ["zuan.zuan-vscode-ext"],
  "capabilities": ["activateExtension"]
}
```

4. `host-ready` 的定义必须至少满足：
   - `host.cjs` 文件加载成功。
   - init data 成功解析。
   - extension registry 初始化完成。
   - 基础 extension API bridge 就绪。
   - IPC message handler 已注册。
   - 能安全处理 `activateExtension` 请求。
5. `server.cjs` 收到 host-ready 后再向前端发送正式 handshake：

```json
{
  "type": "sidex:handshake",
  "sessionId": "ext-host-20260623-abcdef",
  "hostReady": true,
  "extensionCount": 12,
  "extensions": []
}
```

6. 前端只在 `sidex:handshake.hostReady === true` 后允许普通 `_request()` 和 `activateExtension`。
7. 若 host-ready 超时，前端展示明确日志：

```text
ExtHost server connected but Node extension host did not become ready within N ms.
```

8. 如果 host-ready 前收到 activation request，必须明确处理，不能静默丢弃：
   - 首选返回 `HostNotReady` 错误；
   - 如选择 queue，必须有队列长度、超时和 flush 规则。

验收：

- `host.cjs` 人为延迟 ready 时，前端不提前激活扩展。
- `host.cjs` 启动失败时，前端显示 host-ready timeout 或 host exited，而不是长期 handshake not ready。
- WebSocket 连接成功但 host 未 ready 时，状态显示为 `WsConnected` / `HostStarting`，而不是 `Ready`。

### 阶段 3：激活可观测性与 request/response

目标：把“扫描到扩展”和“扩展已激活”分开，并为每次 activation 建立可追踪 request/response。

任务：

1. `activateExtension` 请求必须 request/response 化，并带 `requestId`：

```json
{
  "type": "request",
  "sessionId": "ext-host-20260623-abcdef",
  "requestId": "req-000001",
  "method": "activateExtension",
  "params": {
    "extensionId": "zuan.zuan-vscode-ext",
    "reason": "view:zuan.agentView"
  }
}
```

2. `server.cjs` / `host.cjs` 返回：

```json
{
  "type": "response",
  "sessionId": "ext-host-20260623-abcdef",
  "requestId": "req-000001",
  "ok": true,
  "result": {
    "extensionId": "zuan.zuan-vscode-ext",
    "durationMs": 123
  }
}
```

失败时返回：

```json
{
  "type": "response",
  "sessionId": "ext-host-20260623-abcdef",
  "requestId": "req-000001",
  "ok": false,
  "error": {
    "code": "ActivationFailed",
    "message": "...",
    "stack": "..."
  }
}
```

3. 前端维护 activation table：
   - discovered。
   - activationRequested。
   - activating。
   - activated。
   - failed。
   - providerRegistered。
4. 优先激活当前需要的扩展：
   - 打开 ZuAn view 时优先激活贡献 `zuan.agentView` 的扩展。
   - 不应在 host-ready 后立即无差别批量激活所有扩展，除非有明确需求。
5. 增加 `extension_platform_status` 或新的诊断命令，返回当前宿主状态和扩展激活状态。

验收：

- ZuAn 未出现时能区分：
  - 未扫描到 manifest。
  - 扫描到但 entry/main 缺失。
  - 扫描到但未发送 activate。
  - activate 发送但 host 没响应。
  - activate 报错。
  - activate 成功但 view provider 未注册。
  - provider 注册但 view resolve 未执行或失败。

### 阶段 4：恢复策略

目标：减少一次短暂失败导致扩展长期不可用，同时避免自动重启风暴。

任务：

1. 前端重连次数从固定 3 次改为带状态的退避策略：
   - 启动期更积极。
   - host-ready 前失败允许更多重试。
   - Ready 后断开应显示宿主已断开并允许用户重启。
   - 用户可触发 restart。
2. Rust supervisor 检测 child port 后退出，更新 `total_crashes` 并允许 restart。
3. 增加 crash loop 保护：
   - 例如 5 分钟内崩溃超过 3 次进入 `Degraded`。
   - `NodeMissing`、`ServerScriptMissing` 这类不可自动恢复错误不自动重启。
   - 用户手动 restart 可重置部分计数，但仍需记录历史 crash。
4. 增加 `extension_platform_restart` UI 入口或命令。
5. 重启后重新拉取 init data、重新握手、重新注册 provider。
6. 旧 WebSocket 与旧 request 必须被明确 fail：
   - 未完成 request 返回或标记 `SessionClosed`。
   - 新 session 使用新的 `sessionId`，禁止混用旧结果。

验收：

- 杀掉 `server.cjs` 后，SideX 能显示宿主断开并允许重启。
- 重启成功后 ZuAn view 可以重新激活。
- `host.cjs` 启动即 crash 时，不会无限重启；超过阈值后进入 `Degraded`。
- 重启后旧 session 的 activation response 不会污染新 session 状态。

## 建议日志关键字

Rust：

```text
[ext-host/bootstrap]
[ext-host/node]
[ext-host/spawn]
[ext-host/port]
[ext-host/exit]
[ext-host/status]
```

Node server：

```text
[ext-host/server]
[ext-host/ws]
[ext-host/host]
[ext-host/activation]
[ext-host/protocol]
```

Workbench：

```text
[ExtHost/bootstrap]
[ExtHost/ws]
[ExtHost/handshake]
[ExtHost/activation]
[ExtHost/providers]
[ExtHost/status]
```

所有日志建议至少包含：

```text
sessionId=<...> requestId=<...?> state=<...?>
```

## 验证清单

1. 正常启动：
   - Node runtime resolved。
   - server port received。
   - ws open。
   - server session established。
   - host-ready。
   - handshake。
   - ZuAn activation request/response ok。
   - `zuan.agentView` provider registered。
2. 无 Node：
   - 启动不崩溃。
   - 日志明确提示 Node runtime missing。
   - ZuAn view 若显示，应显示 extension host unavailable。
3. `server.cjs` 缺失：
   - bootstrap 失败信息包含 server path。
   - 状态进入 `ServerScriptMissing`。
4. `server.cjs` stdout 被普通日志污染：
   - Rust 不误读端口。
   - 日志记录协议异常。
5. `host.cjs` 人为延迟 ready：
   - ws 可连接。
   - 前端不提前激活扩展。
   - ready 后再发送 activation。
6. `host.cjs` 人为 crash：
   - ws server 不误报 host-ready。
   - 前端显示 host-ready timeout 或 host exited。
7. `host.cjs` ready 后 crash：
   - 前端状态从 Ready 变为 HostExited/WsClosed。
   - 未完成请求被标记失败。
   - UI 提示可重启。
8. WebSocket 断开但 host 仍在：
   - 前端重连后 server 重新发送 session/handshake。
   - 新旧 request 状态不混淆。
9. ZuAn entry 缺失：
   - scan 阶段明确记录 entry missing。
10. ZuAn activate 抛错：
   - activation response 记录错误 code、message 和 stack 摘要。
11. ZuAn provider 未注册：
   - activation 成功但 provider 缺失时能单独诊断。
12. crash loop：
   - 多次快速崩溃后进入 `Degraded`，不无限自动重启。

## 后续方向

如果 WebSocket 仍然带来不可接受的不稳定性，可以设计第二阶段架构：由 Rust 承担 Node 宿主中继，前端只使用 Tauri `invoke`/`listen`。但这需要 Rust 实现 request/response correlation、child IPC 路由、重连、背压和日志聚合，工作量明显大于当前握手加固。
