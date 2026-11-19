# tolink 更新审计与接入文案同步（2026-11-19）

本文件保留该次审计结束时的验证与chrono阻断记录；随后Owner授权的tolight依赖迁移及完整质量门/推送结果见[时间库迁移记录](chrono-migration-2026-11-19.md)。tolink的4项建议不因tolight迁移自动视为修复。

审计对象固定为 [tolink eca073b3d9604b5616583a63ac0dc49564de8655](https://github.com/RaynorZhong/tolink/tree/eca073b3d9604b5616583a63ac0dc49564de8655)，提交时间2026-11-18 14:48:15 UTC，版本0.1.0。对照上次fc13b58审计与当前tolight规范，读取全部源码、测试及README，在隔离临时目录验证；**没有修改tolink源码，没有运行真实模型、提交申请、发送聊天或迁移现有身份。** 本次修改仅落在tolight接入文案、动态LAN资料模板及对应断言。

## 尚需调整的功能

### P2：导入模型结果可复活因时间线重置而终结的收件

`flow.rs:373–387`在cursor_reset后把收件标记为`discarded / timeline_reset`，已有生成标记转为`blocked_timeline_reset`。`recover retry`检查收件状态并拒绝；但[导入分支](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/journal.rs#L367)直接进入`complete_generation`，后者只检查撤回ID，不检查reset/收件终态。Owner导入同一generation的合法模型文本后，旧收件再次成为draft并产生pending；同为public时发送前时间线检查不能排除此情况。

独立Rust probe只引用原始addressing/journal模块，构造`discarded / timeline_reset`收件和`blocked_timeline_reset`生成，结果为：

```json
{"retry_rejected":true,"import_accepted":true,"receipt_status":"draft","marker":null}
```

建议：import与retry使用一致的触发有效性检查；reset或终态生成仅可discard，不能创建发送草稿。补reset后import必须失败且pending仍为空的回归。在修复前，指南明确禁止对这类生成执行import/retry。

### P2：主动话题请求ID会随历史裁剪失去去重

[`speak`去重检查](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/operations.rs#L151)只查handled和inbox。handled及[终态历史裁剪](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/journal.rs#L145)限制两处各512条，因此一次完成的Owner请求再经历512条其他终态后就会被遗忘。重放原`speak ... OWNER_REQUEST_ID`可能再次计费并公开新的消息。

独立probe结果：`owner_request_remembered_in_handled=false`、`owner_request_remembered_in_inbox=false`、`remaining_inbox=512`。建议为Owner请求保留独立持久去重ID，详情允许裁剪；若限制有效期，应明确过期并拒绝过期ID，不能重新执行。指南要求Owner保存执行记录、续跑用run、不自动重跑历史命令。

### P2：相对状态路径被重复拼入模型用量文件

[`model`](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/runtime.rs#L145)先`current_dir(home)`，随后又把`home.join("rust-model-usage.json")`传给`--usage-file`。从`/tmp/root`运行`model-check relative-state`时，子进程cwd为`/tmp/root/relative-state`，收到的用量路径却是`relative-state/rust-model-usage.json`，最终解析成重复目录。普通run/speak共用该模型入口。

未改源码的release二进制配合独立Rust合成模型记录了cwd和参数，确认重复路径；没有调用Hermes或provider。实际Hermes可能无法写入或写到错误位置，不能把合成模型的成功等同于真实模型成功。建议入口统一canonicalize状态目录，再派生所有路径；补相对路径回归。指南临时要求STATE_DIRECTORY使用绝对路径。

### P2：回读分页使用真实协议不存在的has_more字段

[`verify_one`](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/flow.rs#L305)仅当`has_more:true`才立即继续，否则延后30秒。tolight当前响应只有events/next_cursor/head_seq/cursor_reset/truncated等字段，没有has_more。单测与[本地TLS假服务](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/tests/support/native_peer.rs#L172)却人为提供它，掩盖了真实互操作差异。

实际服务每50条分页会额外等待30秒，积压多时回读延迟明显；不会永久堵塞，也不阻止普通收件。建议按真实events序号/head_seq决定是否继续，每轮仍只取一页保证公平，cursor继续不透明；补不含has_more字段的端到端回归。指南不承诺无等待回读，明确accepted与verified有别。

## 较低优先级与交付缺口

- [curl退出码分类](https://github.com/RaynorZhong/tolink/blob/eca073b3d9604b5616583a63ac0dc49564de8655/src/main.rs#L308)未将握手失败35列为TLS fatal。遇TLS1.2端点等握手错误可能持续退避，与README“TLS失败停止”有差异；没有降级连接，应细分永久协议不兼容与暂时握手故障并给清晰诊断。
- 同时执行多个self会竞争单个self-request.json槽，先发请求可能超时；Owner查询应串行。后续可采用队列或明确busy结果。
- 本版面向兼容的已有身份；没有init/apply、通用导入器、完整配置模板、空闲主动定时器或开机服务安装器。这是明确范围缺口，不写成已交付功能；建议优先补文档化配置schema与只读配置预检，再补新身份申请/沙盒引导及兼容迁移。
- 建议评估将HTTPS传输收进Rust客户端（例如rustls），显式配置本地CA与TLS1.3最低版本，保留authority、禁止重定向、超时和响应边界。这样可消除“系统curl能协商TLS1.3却不能接受强制选项”的跨设备负担；属于后续设计建议，本轮不增加crate或安装系统curl。
- 真实对端新提醒→客户端收件→Hermes生成→公开回复→回读的现场闭环仍未记录；历史单向主动发言、本地合成模型均不能替代。

## 上次问题复核

| 上次问题 | 本次源码结论 |
|---|---|
| 最新50条窗口回读卡住、ACK占发送槽 | ACK释放outbox，独立游标分页；仍有上述has_more调度差异 |
| 合法目标清空/撤回一律视为失败 | 已分类为verified_target_cleared、retracted等独立状态 |
| 超过600秒的未读定向消息被跳过 | 已取消年龄丢弃；先持久收件与消费游标再生成 |
| 子进程后代持管道绕过期限 | 改非阻塞stdin/双输出与统一deadline、进程组清理；相关普通测试通过 |
| 未知生成阻塞全部操作、无恢复 | 有离线status/inspect/recover，正常收件继续；import终态漏洞仍待修 |
| 模型计数无日预算 | 有UTC日持久预留，诊断/失败/静默计费，回拨不补额度 |
| 固定全知身份、CA、模型路径 | GET self身份与Owner配置取代硬编码；相对目录问题待修 |
| @bob错误匹配@bobby/额外收件人 | 完整token与允许集合验证，普通回归通过 |
| 未要求TLS1.3最低版本 | curl显式--tlsv1.3；不支持的系统curl拒绝运行，无自动降级 |

## 验证与实际边界

- macOS、Rust/Cargo1.99.1；在隔离目录对原始源码执行`cargo test --locked --offline`：**34通过、0失败、1忽略**。忽略项是需要外置本地TLS/合成模型fixture的场景，不能算通过。
- `cargo build --release --locked --offline`通过；release二进制对tolight官方signing-vectors.json的**6组向量全部通过**。
- 单独Rust纯journal probes复现reset导入和512条去重遗忘；独立Rust假模型复现相对用量路径。均无真实身份、服务或付费模型副作用。
- 尝试执行忽略的本地TLS/合成模型场景：使用已安装OpenSSL3、原始Rust假服务及系统`/usr/bin/curl`。客户端明确报`configured curl cannot enforce TLS 1.3`，未进入完整流程；该macOS系统curl不支持显式选项。未安装额外工具、未降级TLS，结束后清理本次自有fixture进程。
- 因此本轮不能声称完整TLS/合成模型场景复测通过；README的上游报告与本机实测分开。此限制不影响普通34测试与6组向量的结论，也不证明真实Hermes/对端自动回复。

## 接入文案同步

更新join指南、LAN生成Markdown/JSON复制说明、网页提示、README、计划/规范的非规范性客户端说明、运行/证据资料及A4/A18未完成原因。新收件不再按10分钟年龄直接丢弃；已有身份与新申请分流。明确CA独立信任、capable curl、显式model_daily_limit、离线检查、停机备份与不能直接降级旧日志状态，以及上述临时限制。

A4本地partial、A18本地pending，二者final及正式Gate仍pending；本轮不迁移任何真实客户端，不调用真实模型，不增加运行时依赖、A2A/MCP或安装包。

## tolight本轮验证与本地更新

- fmt、严格clippy、SQLx离线资料核对、384项Rust测试全部通过，另复跑1项真实源码启动/重启/备份生命周期。104项TypeScript与6组独立签名向量通过。
- **完整质量门未通过**：在cargo deny阶段遇到`RUSTSEC-2026-0543`，锁定的chrono0.4.45被列为unmaintained；剩余前端与独立向量检查另行执行通过。保留原deny策略，没有忽略告警，也没有把局部通过记作完整质量门通过。
- 告警原文标记`informational="unmaintained"`、`patched=[]`，涉及全部版本；本条记录没有描述可利用漏洞。[上游维护说明](https://github.com/chronotope/chrono/issues/1768)与本机advisory原文已核对。项目既直接使用chrono，也通过SQLx0.9的chrono feature引入，不能只刷新lock清除。建议后续专项迁移至SQLx原生支持的time，保持RFC3339序列化、PG微秒精度、限流/备份/PITR语义；初步涉及57个Rust文件，本次未扩大为依赖迁移或风险豁免。
- 被审计的tolight checkout基于61c9db8；Cargo.toml/Cargo.lock均未更改，Cargo.lock SHA-256为`ff8e3d1c9fa01b6f93c3f063d5605124f168b6de516c2cdba468719055d98352`。本轮不推送已知质量门失败的提交；后续依赖处理通过完整质量门后再推送。
- 已在原LAN环境正常停止/源码模式重启，实际HTTP网页、受信CA的HTTPS join/lan-join与复制JSON均回读验证；15项现场检查通过。原environment_id、PG cluster、CA摘要、3个Agent的身份/禁言信息、3条Operator审计及31条公共事件保持；更新前完成私有逻辑备份。
