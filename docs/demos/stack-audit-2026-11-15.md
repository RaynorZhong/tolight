# Rust / TypeScript 技术栈审计（2026-11-15）

Owner明确要求保持Rust技术栈，并将JavaScript源码改为TypeScript。此前把Python扩展到测试/运维的决定属于实现偏离，已撤回。

- 服务、参考客户端、运维工具与集成测试：Rust。同一workspace中的tolight-ops承担runtime/storage/delivery/demo；Shell仅保留已有工具编排和参数转发。
- 网页源码与网页测试：frontend/app.ts、tests/web/app.test.ts。tsc严格检查，生成public/app.js；产物不作为源码提交。TypeScript与Node类型是锁定开发依赖，无浏览器运行时npm依赖。
- Python例外仅为examples/python/minimal_client.py和check_vectors.py：计划Task2/规范6.11要求的最短示例及独立签名互操作复算。外部Agent客户端仍可自行选择语言。
- SQL迁移、Nix环境声明、HTML/CSS和Shell启动包装保留各自用途，不引入应用运行时框架。

当前源码边界：15个Python运维/演示实现及5个unittest文件迁入Rust；前端手写JS与.mjs测试改为两个TypeScript文件。SQL迁移和配置schema没有改变。参考客户端的模型/信号测试桩为Cargo构建的Rust子进程，不再生成Python程序。

迁移复核补回4处容易遗漏的旧断言：仅mtime变化不污染Git检查；production正例的真实CLI退出码、完整ready状态与Git只读性；签名向量已冻结而文档仍为草案的单独阻断；待验收ID精确等于A1–A21。外部观察来源、独立M3记录不可复用和真实模型整24小时要求继续保留。

发现并修复的构建/入口问题：隔离npm构建须使用两个不同的私有空配置文件，避免同一路径双重加载失败；两个Cargo包显式default-run保留既有启动命令；独立publish runner先构建Rust运维工具，再提取发布说明；探针时间参数恢复为正整数，拒绝NaN/inf/小数后才创建状态文件。

已完成的定向验证：TypeScript严格类型检查与编译、44项网页测试、生成浏览器ES模块加载；7项Rust模型/监督测试、3项探针参数与时钟分类、14项真实PG备份恢复、59项发布/验收/源码合同、1项真实Caddy适配器文档合同通过。原生运维另覆盖告警和本地持久生命周期；最终以完整质量门去重计数。

source合同中的Rust成功/失败编译使用极小合成workspace；TypeScript冷净构建使用真实锁定编译器。它们不能代替本仓库完整隔离克隆构建，后者单独记录。

完整仓库隔离克隆构建已实际通过，耗时35.28秒：[脱敏结果](stack-source-rehearsal.json)。221份源码构成私有fixture提交`74e3527ed9b0770cd98a31112d3b9a69db70f11e`，精确SHA检出后真实执行npm ci、TypeScript编译及独立target的离线Cargo构建；tolight、tolight-ops、reference-client、reference-fixture共4个bin的help通过。快照不含生成app.js或node_modules；原源码及.git摘要不变，checkout构建前后干净。这是本地未发布fixture，后续追加的验收证据不在该快照中。

M1原生演示完成8条消息、真实429等待和恢复，使用真实旧构建`192f16d115012fc93a9cd8eab41c09f246aeb982895cd5631e37ee8550670135`回滚后再恢复本轮构建，全部data文件哈希不变。旧二进制使用只含其支持字段的兼容配置，数据库和public路径保持一致。M2原生演示启用明确标记的模型命令桩，15个步骤通过，含三项沙盒、配额/通知、控制/重启、换钥、恢复新增身份精确授权、Owner退出、公开边界/隐私扫描和清空演示数据；两个临时PG均已停止清理，原58120预览与latest指针保留。

完整质量门退出0：[脱敏结果](quality-gate-stack-audit.txt)。267项workspace Rust测试加1次Cargo退出后的默认源码构建生命周期复测，共268次Rust执行；44项TypeScript网页测试、6组独立签名向量通过，无失败、忽略或跳过。严格类型检查/编译、fmt、迁移、离线Clippy、SQLx prepare与cargo-deny通过；保留已有13项重复版本warning，没有新增忽略。历史M0–M4与local-dev日志保持当时事实。

现有持久主环境已正常停止旧监督者，并由原生tolight-ops以默认源码构建模式重新启动；网页8080/API8090/运维8091恢复。环境ID`4df331f9-288f-43fb-ac31-36ce41b8a974`、PG系统标识`7708293698505551965`保持不变，public/data全树前后SHA256逐项一致。原生status显示running/runtime_valid为真；实际编译网页、完整公开接入资料及下载签名样例验证通过，旧58120预览返回HTTP200。没有重建或清空该持久数据库。

生产部署、Nix实跑/远程CI、真实第三方自写客户端、真实模型24小时、授权公开渠道与生产RTO/RPO仍按原验收Gate等待实际证据。
