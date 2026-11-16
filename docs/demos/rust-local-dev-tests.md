# 本地环境集成测试迁入Rust

历史记录：本页保留当时实现与测试事实；当前运维、发布/验收测试及Cargo元数据解析均已迁至Rust，网页源码/测试为TypeScript。最新结果见[技术栈审计](stack-audit-2026-11-15.md)。

范围：将首次持久环境交付中的两项Python集成测试迁入`crates/tolight-cli/tests/local_dev.rs`。保留真实PG18、CLI、HTTP、持久重启、维护和备份/负面路径；已有运维脚本仍是被测程序。独立Python签名向量复算保留，原有发布/验收/停源恢复的Python测试不在本轮迁移范围。

## 运行方式与构建锁

测试随cargo test --workspace --all-targets运行。Rust harness使用Cargo提供的CLI产物并放入测试自己的私有目录，再通过显式`--tolight`注入，避免在持有Cargo测试锁时再次cargo build。Cargo退出后，质量门根据Cargo JSON元数据直接执行同一Rust测试程序的默认构建变体，不传--tolight、保留坏Cargo/SQLx父环境与完整生命周期断言，真正构建当前源码。该复测不会重新进入cargo test父进程，保持无锁循环；不引入fake cargo或ignored test。

正常local-dev up不传--tolight时仍从当前锁定源码构建；测试模式开关只由Rust harness读取，产品脚本不据环境变量跳过构建。Python在质量脚本里只解析Cargo元数据定位程序，全部验证断言仍在Rust。

预构建路径须为绝对、普通、可执行文件，缺失/目录/非执行/symlink/相对路径在初始化前拒绝。环境与运行记录绑定所选binary，后续Operator、migrate、publish沿用；显式换另一条路径会被拒绝。旧无该字段的运行记录兼容默认源码路径，已运行的8080/58120不必为测试迁移停机。

## 覆盖对照

| 原有能力 | Rust验证方式 |
|---|---|
| 首次启动与HTTP文档 | 真实隔离PG、Cargo CLI、Reqwest与无签名401/网页200 |
| 身份/控制/事件持久化 | CLI登记PENDING与pause，正常停机/重启后核对申请ID、PG系统标识、公开seq/event_id与paused |
| 坏父环境与跨时区 | 注入错误PG/数据库/构建环境，跨Honolulu/Tokyo操作，不串库或借旧binary |
| 重复启动与PID污染 | 只持有自有Child，篡改supervisor/child及真实foreign指纹均拒绝，不影响无关进程 |
| 双备份 | 真实pg_dump/pg_basebackup/pg_verifybackup产物及SHA256，调用实际告警消费者核验完成marker |
| 维护窗口 | PG-only、HTTP端口全关、私有控制导出0600、001显式幂等迁移与公共树重建 |
| 正常清理与立即重启 | 有界等待、自有进程句柄清理；PG未停则保留诊断，不由TempDir自动删除活库 |
| 目录与端口拒绝 | 非自有/symlink/越界路径内容和权限不变；真实监听拒绝且不启动PG |

## 结果

Rust定向测试2项通过，最终一轮8.64秒；Clippy -D warnings通过。原Python两项已删除，其源码摘要留在私有日志中用于对照。peer逐项核对了原持久化、备份、维护、四种PID污染、跨时区和目录/端口断言，新增预构建路径拒绝与不同合法binary切换拒绝；测试中没有嵌入Python断言或动态import。

默认源码构建变体先独立实跑1项通过9.73秒，之后已纳入自动质量门；完整门中的同一变体1项通过8.09秒。实际执行cargo build，坏CARGO_TARGET_DIR/CARGO_BUILD_TARGET/SQLX_OFFLINE_DIR未影响所选产物/元数据；没有假Cargo或ignored test。全部自有PG/监听与临时目录清理。

完整质量门退出0：[脱敏结果](quality-gate-rust-local-dev.txt)。201项workspace Rust测试加默认构建变体1次，共202次Rust测试执行；44项网页、62项原有发布/验收/停源恢复Python测试及6组独立签名向量全部通过，无失败/忽略/跳过。fmt、迁移、离线Clippy、SQLx prepare及cargo-deny通过；保留既有13项重复版本warning，无新增忽略。

旧运行profile只读兼容核验成功：现有.local/dev的environment.json/runtime.json无需改写，新增选项仍可status/down；8080主环境与58120旧演示继续保持。最终机器索引local退出0、正式验收保持pending/退出2。迁移不涉及真实第三方/模型、生产发布或正式首版Gate。
