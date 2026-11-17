# Linux CI启动端口竞争修复（2026-11-17）

## 失败证据

品牌界面提交5aaf3c7的[GitHub CI](https://github.com/RaynorZhong/tolight/actions/runs/43986074195)在local_dev测试组失败：12项通过，`automation_retries_a_temporary_config_error_without_losing_checks_or_backups`在等待首次启动ready时失败。测试自有环境的serve.log为 `AddrInUse / Address already in use`，发生在配置故障注入之前。工作流只有一个quality job，所以邮件显示“All jobs failed”。

原分配器用 `bind(127.0.0.1:0)`同时取得若干空闲端口，返回编号时释放全部listener。随后初始化PG、迁移及发布期间，端口没有被保留；不同fixture可以再次取得相同编号，客户端也可能使用该临时端口区间。失败日志中的API、web和ops候选均位于Linux常用临时端口段；日志没有记录实际占用者，不将其归因于某个已确认的进程。

## 修复

只修改Rust测试的端口分配器：Linux读取实际 `/proc/sys/net/ipv4/ip_local_port_range`并排除它；其他Unix测试保守排除32768及以上，覆盖标准macOS默认范围。扫描有界的非特权候选，实际bind探测占用，同批探测socket保持到分配结束，并在整个测试进程记录已分配编号，之后不再分给其他fixture。

锁只保护分配，不串行化测试；没有增加ready期限、重试启动、忽略错误或移除外来端口占用拒绝。生产运行时、默认局域网环境、数据库及Agent身份不作更改。

## 验证

- 新增确定性回归：8个并行分配调用的48个号码互异且不在客户端临时端口段，之后分配不复用先前号码；旧bind(:0)方式在Linux会违反该范围断言。
- local_dev全部14项通过，包含原失败场景、真实LAN链路、已占用端口拒绝及新回归。
- 额外的真实源码构建、重启和逻辑/物理双备份生命周期回归通过；严格clippy、fmt与空白检查通过。私有日志为 `.local/logs/ci-port-source-lifecycle-2026-11-17.log`、`.local/logs/ci-port-clippy-2026-11-17.log`。
- 修复提交仍须完成Linux的完整[Quality gate](https://github.com/RaynorZhong/tolight/actions/workflows/ci.yml)，执行者须回读实际运行结果；本地通过不冒充远端通过。普通源码CI不关闭正式发行Gate。
