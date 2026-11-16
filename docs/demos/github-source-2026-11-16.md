# GitHub首次源码提交与推送（2026-11-16）

Owner创建公开仓库 [RaynorZhong/tolight](https://github.com/RaynorZhong/tolight) 后明确授权推送。沿用现有main创建首次源码提交 [578908b42c1f2012f7bb5c418ff30c17a07920d3](https://github.com/RaynorZhong/tolight/commit/578908b42c1f2012f7bb5c418ff30c17a07920d3)，通过现有SSH认证普通push，设置origin/main跟踪；随后经GitHub连接器回读heads/main，SHA与本机一致。没有强推、推tag、创建Release或部署生产。

## 文件范围与验证

首次提交包含271个源码、示例、固定锁文件及研发记录。按现有发布源码边界核对，工作区与暂存区无遗漏；提交后release-check的checks_passed=true、errors为空，acceptance-check结构通过。Rust/TypeScript代码沿用本轮完整质量门：379项Rust、一次默认源码生命周期复测、90项TypeScript及6组独立签名向量，见[tolink修订验证](quality-gate-tolink-preferred.txt)。此次首推只补源码托管状态和忽略规则，不声称重跑了tolink上游测试。

发布前发现根目录限定的`/.local/`会漏掉crate下的运行日志，已将运行缓存目录忽略改为任意层级，并排除真实public/data、私有配置、密钥、数据库和日志。实际候选中无这些私有路径，无真实token、私钥或Owner联系信息。保留明确标记的公开签名测试种子和demo=true的静态聊天fixture；既有公开Agent字段与脱敏运行事实不冒充私有来源或真实模型证明。

未配置本机全局Git身份；仅在本仓库缺少提交身份时使用`Codex (AI) <codex@localhost>`，未改全局配置。文档中的旧“无HEAD/无远端/未推送”记录保留首推前事实，当前源码版本化状态以本记录为准。本地服务、数据库、真实Agent客户端与权限没有因Git操作变更。

## CI与正式验收

首次push触发[Quality gate](https://github.com/RaynorZhong/tolight/actions/workflows/ci.yml)，首轮运行编号为[43894275706](https://github.com/RaynorZhong/tolight/actions/runs/43894275706)。运行结果以实际Actions及所检查的commit为准，不将触发成功视作测试通过；后续文档或修正提交会触发对应的新运行。

普通main源码公开不等于正式版本发布。代码仍为0.1.0、参与者规范仍为0.1草案；正式tag、flake.lock、分支保护、生产Origin、公开联系渠道、真实Agent与模型24小时等对应门槛保留。A2及public_release_ci继续pending，不因仓库可见或分支CI通过而自动关闭。
