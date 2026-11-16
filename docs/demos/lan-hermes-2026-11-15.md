# Task15/16/20 局域网 Hermes 接入准备（2026-11-15）

Owner本轮明确授权开放本地局域网，供另一台设备上的Hermes自行编写客户端接入。项目继续Rust+TypeScript，无新依赖、数据库迁移、参与者端点、A2A、SDK或安装包；生产及真实第三方正式验收Gate保留。

- `local-dev up --lan-ip <私有IPv4>`显式启用并持久保存局域网配置，默认HTTP网页8080、HTTPS参与者API8443；原loopback网页/API/运维和私有Unix socket PG继续保留。只绑定指定RFC1918地址（loopback供隔离测试），拒绝通配/公网地址；端口可覆盖，`--no-lan`关闭并保留数据库和CA，维护模式不启动网关。
- 复用已有Caddy，独立私有XDG/CA存储、禁管理API与访问/请求错误日志，不安装系统信任或修改防火墙。HTTP只读网页且拒绝参与者请求，HTTPS仅代理参与者API与接入原文固定GET/HEAD清单，管理/health/metrics/私钥路径不暴露。保持32KiB请求上限、真实来源IP、原Host签名和TLS1.3。
- 实测发现按IP无SNI时Caddy会生成额外的TLS1.2默认策略。网关改用限定bind的无SNI策略、显式IP自动证书和Host拒绝；回归要求所有TLS策略最低1.3，真实TLS1.2-only连接失败、TLS1.3验证IP证书成功。
- 动态`lan-join.md`与公开`local-ca.pem`提供真实地址及PEM文件字节SHA256。Owner从本机status独立给出hash；Hermes先核对下载CA，再以该CA的TLS连接取指南、规范、签名向量和示例。HTTP页面自己的hash不足以建立信任；不关闭证书验证、不改系统信任。Hermes仍自写客户端、本机保管密钥、申请、完成三项沙盒并等待人工审核。
- 为保留原loopback直连，新增`network.allow_direct_loopback`，默认false；仅LAN配置启用且backend必须监听loopback。已受信loopback且完全没有XFF时才回退peer，坏值/重复头仍拒绝，远端与未受信伪头忽略。原本机HTTP API的80/443默认端口与外部HTTPS签名规范化有冲突，LAN模式明确拒绝这两个backend端口，默认8090正常；LAN自身HTTPS443不受此限制。
- `local-dev check`在原8项之外为LAN增加7项：页面、manifest、agents、TLS未认证401、可信HTTPS指南、CA字节摘要和HTTP参与者入口拒绝。停机/维护/身份无法确认时仍跳过全部HTTP，不自动修复或批准身份。

定向验证已通过：helper真实Caddy适配/自有CA、TLS1.3与TLS1.2拒绝、日志不含签名和正文等4项测试；核心配置与来源规则新增2项回归。新增3项真实PG/TLS集成覆盖默认客户端不信任自有CA、可信TLS1.3/IP证书、无SNI TLS1.2拒绝、HTTPS接入资料与CA、Host和请求体上限、本机HTTP签名直连、真实申请/三项沙盒/20秒429等待/待审隔离/人工批准后公开发言，以及配置/CA/数据跨重启保留、维护不监听、显式关闭/重开、10组非法参数及占用端口不接管。全部使用自有隔离目录/PG/Caddy，内部客户端不冒充Hermes独立接入。

local_dev完整8项已通过。实际完整仓库隔离克隆构建通过，29.678秒，见[脱敏结果](lan-source-rehearsal.json)。237份源码组成私有fixture提交`dc2dcf80c82be5e4def3baa9667e8e4cb5c21f85`，精确检出后真实离线npm ci/TypeScript编译、独立target离线Cargo workspace构建与4个bin的help全部通过；包含最终Clippy断言及系统curl指南兼容修正，未复制生成JS或node_modules。原源码逐文件内容/owner执行位/链接及真实.git摘要不变，构建前后检出干净，未发布。源码快照保留当时文档及前次结果状态，后续证据不反写快照。

质量门所列必需检查已全部通过，见[完整分步结果](quality-gate-lan-hermes.txt)：324项workspace Rust测试、1次默认源码构建生命周期复测（325次主流程Rust执行）、51项TypeScript网页测试和6组独立签名向量，无失败/忽略/跳过；类型/构建、fmt、离线Clippy、隔离迁移、SQLx prepare通过。主质量门在RustSec在线拉取时遇GitHub DNS失败，退出1；DNS恢复后只重跑cargo-deny及后续网页/向量步骤，均退出0，在线依赖审计通过，13项既有重复版本warning保留。最后的指南文字修正另复测helper4项、定向Clippy/fmt与上述最终源码构建。本轮新增9项Rust回归。

原持久环境已正常down/up，在`192.168.50.54`开放HTTP网页8080与TLS1.3参与者API8443；最终`check`15项全部通过。实际IP上TLS1.2-only连接以protocol-version alert失败，系统curl默认协商并校验CA/IP可成功下载HTTPS指南；原loopback页面/完整接入资料及下载向量复算通过。LAN地址的8090/8091/5432均无监听，HTTPS的healthz/metrics/私钥路径均404，原58120预览仍为200。未修改系统防火墙或信任库。

环境ID`4df331f9-288f-43fb-ac31-36ce41b8a974`及PG系统标识`7708293698505551965`未变，public/data全树前后SHA256一致；没有清库或制造Hermes测试身份。最终指南兼容修正再次正常重启后，同一CA文件SHA256仍为`ccc2c7d741f1ac5dbcd2509d56abe2cfee3154858a32d9492bc2c33a9b0a2c49`。这是本机向自身LAN地址的真实验证；另一台Hermes的独立编写/申请/沙盒/审核与聊天记录仍待实际设备执行。

验收账本已原生重新渲染且结构/证据检查通过，本地仍为14 verified / 5 partial / 2 pending，21项最终验收仍pending。正式验收及生产发布预检均退出2，errors为空；同机备份的既定范围也已与计划对齐，未追加异机备份任务。LAN预演没有关闭公网、真实第三方、真实模型24小时、公开联系渠道或正式源码发布Gate。
