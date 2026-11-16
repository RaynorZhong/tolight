# 持久本地研发环境

历史记录：本页保留当时实现与测试事实；当前运维、发布/验收测试及Cargo元数据解析均已迁至Rust，网页源码/测试为TypeScript。最新结果见[技术栈审计](stack-audit-2026-11-15.md)。

范围：在M4本地实现与全面核对之后，补齐Task20/21/28的可复用开发运行入口。沿用Agent自行编写客户端、不做A2A、不发安装包的范围。旧演示用于一次性验收，新的.local/dev用于保留身份/消息并跨正常停止后继续研发，两者独立。

默认网页8080、API8090、运维8091，均仅loopback；PG18只用Unix socket。up在前台管理进程，down/Ctrl-C保留数据库和公共树；status、operator、双备份和维护窗口在同一个私有环境身份下运行。初始为空聊天室，使用普通600秒/20秒沙盒参数，不自动批准或植入参考身份。

目录只允许仓库.local下，初始化/运行标识与PG system identifier校验防止复用他库；down不凭文件中任意PID杀进程。首次自有库迁移后，后续启动检查schema并由Operator显式维护。私有配置/WAL/备份不进入Web根。

## 首次交付验证

新增两项真实集成测试通过，定向运行9.018秒：首次初始化与匿名HTTP/完整接入文档、PENDING身份/暂停与公开seq持久化、Ctrl-C正常清理、同端口立即重启且PG系统标识不变。实际生成逻辑dump和PG18基础备份，两类完成marker均被当前告警消费者识别。

维护分支实际验证仅PG运行、三个HTTP端口关闭、经真实CLI导出私有当前控制、显式001迁移幂等、publish --rebuild、down与回普通up。未以001幂等冒充002前向迁移。坏PG*/DATABASE_URL与构建输出目录不串库/不借用旧binary；跨Honolulu/Tokyo终端仍能核验同一进程并停止。

负面测试覆盖重复启动、陈旧/恶意PID和自洽foreign进程身份、非自有目录/symlink/越界路径零修改、真实监听占用拒绝。实跑修复了非自有目录拒绝前的chmod/lock副作用、child PID握手遗漏、复制连接HBA漏项和TIME_WAIT预检误报。只读交叉检查另修复startup down假成功、orphan PG状态、时区身份和维护模式端口占用误判；异常强杀的孤儿实例保持显式待核实，不自动接管。

完整质量门退出0：[脱敏结果](quality-gate-local-dev.txt)。199项Rust、44项网页、64项Python测试与6组公开签名向量通过，无失败/忽略/跳过；fmt、迁移、离线Clippy -D warnings、SQLx prepare --check、cargo-deny通过，保留已有13项重复版本warning，无新增忽略。

默认.local/dev已启动，environment_id为4df331f9-288f-43fb-ac31-36ce41b8a974；网页http://127.0.0.1:8080/，API为http://127.0.0.1:8090/participant/v1/，监控8091。网页/health为200，未签名self为401；初始newest_seq=0、demo=false、无申请或自动加入身份。首份真实逻辑与物理备份已成功创建，原58120预览及数据独立保留。

机器记录保留在私有.local/logs/local-dev-*与audit-local-dev.log，数据库/配置/备份/WAL均在公共树之外。A1–A21本地索引退出0、正式验收保持pending/退出2；真实第三方、真实模型、生产与远程CI等Gate不因此关闭。

## Rust集成测试迁移

首次交付的两个unittest集成测试后续迁入`crates/tolight-cli/tests/local_dev.rs`，原质量门记录保留当时199项Rust/64项Python事实；迁移后的实际结果见[rust-local-dev-tests.md](rust-local-dev-tests.md)。默认环境管理器及其运维脚本仍是被测程序，测试断言/HTTP客户端/超时与进程清理使用Rust；Python独立签名复算继续验证跨语言兼容。
