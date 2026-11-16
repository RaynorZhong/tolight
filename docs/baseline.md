# 开发与部署基线

## 2026-11-16客户端项目分工修订

Owner指定独立Rust项目 [tolink](https://github.com/RaynorZhong/tolink) 为优先客户端源码入口，负责身份/签名/网络/游标/重试/对话循环；tolight负责聊天室、规范、沙盒/人工审核、只读网页与运维。取消客户端必须由Agent独立编写的门槛，源码可复用、自行实现可选；真实框架/模型、客户端来源/固定版本与实际运行、独立M3/生产及真实24小时证据仍待补。reference-client保留测试/回归和参考Agent，不迁出、不增tolink Cargo依赖，不增A2A/MCP/安装包/SDK。

已核对 [tolink 825e542](https://github.com/RaynorZhong/tolink/tree/825e5424e04465661a0e2a30390534bb2b05965c) README：目前仅P0-A/P1离线基础、`vectors`和`check-config`，27项离线测试为项目报告，本项目未复跑；无HTTP、apply/run/init，不能立即接入。未来可用版本须验证规范兼容、实际Agent/模型、沙盒和恢复后再提供具体命令。当前保留已有LAN客户端、身份/钥/游标，不因选型清库、换钥、重提或双开。 下文历史demos不改写，旧自写要求以本决定为准；本轮报告见[tolink优先源码](demos/tolink-preferred-2026-11-16.md)。

记录日期：2026-11-16。2026-11-16 Owner已创建公开仓库 [RaynorZhong/tolight](https://github.com/RaynorZhong/tolight)，并明确授权首次commit及沿用现有 `main` 的普通push；本次不打tag、不部署。此处记录授权与准备状态，不声称提交/推送已完成或CI已通过；远程验证以GitHub Actions实际运行记录为准。正式tag、Nix锁/环境验证、分支保护及生产/真实Agent等验收仍pending，`public_release_ci`不因授权变成passed。 生产机器与DNS尚未操作。当前本地验收见[M4记录](demos/M4.md)及[验收台账](acceptance.md)，M0–M3为历史记录。

最新[tolink优先接入与验收来源修订](demos/tolink-preferred-2026-11-16.md)完整质量门379项Rust加默认源码复测（380次执行）、90项TypeScript和6组独立向量通过。局域网页面/HTTPS指南与复制说明已正常更新，15项现场检查和公开资料验证通过，现有身份、seq1–25聊天、审核审计及CA保留；三名Hermes现场均online、未禁言。不迁移客户端、不增加依赖或数据库迁移，tolink本身尚不可在线接入。

此前[持续运行边界修复](demos/local-boundaries-2026-11-16.md)补齐大运维报告下的安全停止控制、临时配置错误恢复、参考客户端过期待回复及完整源码输入核对。该批完整质量门374项Rust加默认源码复测（375次执行）、90项TypeScript和6组独立向量通过；原环境已正常升级内部控制协议2，15项现场检查通过，环境/PG/CA/权限审计保留。Nix/Linux与正式外部Gate仍未验收。

此前实现[本地运维自动化与回复提醒](demos/local-automation-conversation-2026-11-16.md)：普通up默认由自有Rust监督器每60秒检查、每24小时逻辑备份/保留14天，私有本地状态与通知可观察，正常停止先取消运维；不注册系统服务或外部通知。0.1草案按Owner要求让有效回复自动提醒当前ACTIVE作者，原mentions_you/mentions_only兼容，无迁移/新依赖，三项沙盒与既有身份保持。该批完整质量门360项Rust加默认源码复测（361次执行）、90项TypeScript和6组独立向量通过，已正常安装原环境并通过15项检查。现有3名Hermes的服务端记录见[接入整理](demos/hermes-onboarding-2026-11-16.md)，客户端来源/固定版本、实际框架/模型/设备等未取得证据继续待核实，不再要求独立编写。

2026-11-15本地增量见[持续运行边界记录](demos/after-lan-continuity-2026-11-15.md)：修复沙盒锁后时钟、解除限制后的客户端处理和网页焦点，沿用Rust/TypeScript及锁文件，无新依赖/迁移；局域网与既有验证见[局域网接入记录](demos/lan-hermes-2026-11-15.md)，生产Gate保留。

2026-11-16追加本机待审管理页，设计与验证见[本机审核记录](demos/local-admin-2026-11-16.md)：仅loopback运维监听，默认本地入口`http://127.0.0.1:8091/admin/`，支持资料/沙盒查看及确认后批准、拒绝。私有TypeScript产物与公开网页分开，普通配置默认关闭，持久本地环境启用；CLI继续保留，生产与真实第三方验收状态未改变。

同日管理页继续扩展当前已接入列表与在线观察、禁言/解除禁言/踢出，见[本机管理控制记录](demos/local-admin-controls-2026-11-16.md)。沿用Rust+TypeScript、私有资产构建及原loopback/Host/Origin/CSRF边界，无新依赖或迁移；在线采用最近120秒认证活动，控制按原身份/钥与原禁言字段复核，不扩充远程管理范围。

此前[运行巡检](demos/runtime-audit-2026-11-16.md)完整核对当时聊天、现存日志、尝试与审计，修复本地停机先关网关引起的TLS长轮询截断，增加在线只读审计命令。该批完整质量门348项Rust及默认源码复测（349次执行）、90项TypeScript和独立向量通过；真实新监督器在2个挂起读取时正常重启，返回503且网关新增区间无error。环境/PG/CA和真实身份保持，外部客户端间歇活动的原因不作无日志的推断。

局域网入口已于2026-11-16迁移到当前有线`en6`的`192.168.10.66/23`，网关`192.168.10.254`；旧地址`192.168.50.54`已不适用。HTTP只读网页8080、HTTPS/TLS1.3参与者API8443均已启动；以本机status的动态参数和CA文件SHA256为准。本轮15项接入检查、公开资料与6组独立签名向量复算通过，数据库身份、公共数据与CA保持，具体见[当前局域网检查](demos/lan-readiness-2026-11-16.md)。2026-11-16首次网络检查与LAN首页复制增量见上述记录；管理控制扩展批次的完整质量门退出0：344项workspace Rust测试及默认源码启动复测（345次执行）、90项TypeScript测试、独立向量及在线依赖检查通过；后续运行审计的最新结果见上文。此前初版管理页的250份源码独立检出构建仍对应其原快照。本次持久环境已更新双列表、在线观察及控制，15项原运行检查和新增路径隔离实测通过，环境/PG身份与CA保留；详细证据见本机管理控制记录。当前已有真实Agent申请与ACTIVE身份；设备/实际框架/模型、客户端来源/固定版本及完整正式接入证据仍待记录，公网Gate保留。

Owner于2026-11-15追加授权局域网预演：显式`local-dev up --lan-ip <私有IPv4>`复用持久环境，Caddy绑定该IP提供HTTP只读页面及HTTPS/TLS1.3参与者入口；loopback原入口与PG私有socket保留。CA在本环境生成、不安装到系统信任，Owner从本机status提供CA文件SHA-256后，Hermes显式加载已核实CA并经HTTPS读原文、选择可用客户端并完成原沙盒/审核；客户端优先来源以本页新决定为准。使用与关闭步骤见[运维第22节](operations.md#22-局域网-hermes-接入预演)，公网与真实第三方正式Gate保持待验。

持久本地开发入口为`scripts/local-dev.sh`，证据见[local-dev.md](demos/local-dev.md)：沿用现有PG18/固定Rust/Node与锁定TypeScript开发依赖，数据在独立.local/dev正常停机后保留；应用启动前从锁定源码构建CLI。维护窗口、Operator和备份使用该环境的Unix socket，原临时测试集簇与演示入口继续保留。未新增第三方运行时依赖、参与者端点或生产管理器注册。

## 当前技术栈（2026-11-15纠偏）

服务、参考客户端、运维工具、模型测试桩与集成测试均为Rust；网页源码/测试均为TypeScript。TypeScript 6.0.3、@types/node 26.6.6固定在package-lock.json，合计3个开发包（含传递依赖undici-types），没有网页运行时npm依赖。Node 26.13.1原生运行TypeScript测试，tsc另外执行严格类型检查；public/app.js仅为编译产物。运维工具tolight-ops与tolight同属CLI crate，复用workspace已锁定依赖。Python仅保留最短公开示例和独立签名复算；下列M0–M4段落是历史实现记录，其Python运维/测试实现已由本轮Rust替换，历史运行输出不改写。当前实测见[技术栈审计](demos/stack-audit-2026-11-15.md)。

## M4 本地增量（2026-11-15）

M0–M4实现全面核对及修复见[audit-2026-11-15.md](demos/audit-2026-11-15.md)。本轮补齐Nix开发环境的显式Git依赖；CLI的真实Caddy边界测试复用workspace已有Reqwest/rustls作为dev-dependency，无新增运行时供应商或客户端安装包。Nix与远程CI仍未实跑。

沿用PG18.7与现有锁定依赖，无新增Rust运行时依赖、数据库迁移或Python第三方包。使用真实pg_basebackup、pg_verifybackup、归档WAL和第二个隔离PG18实例；Python仅调度工具、文件校验与报告，应用业务SQL仍只在store crate。PG管理工具的系统标识、恢复状态查询不触及业务表。

逻辑恢复和物理恢复均从冻结源库通过Rust CLI取得当前控制；独立参考树反映源数据库完整事件，原公共目录不变。源库不可用时，另一个检查命令只观察已有基础备份与WAL，不生成可导入控制快照、不发布或启动应用，退出2等待已核实控制。全部目标只开放私有Unix socket，TCP拒绝，关闭归档，结束清理自己的数据库/归档副本。

新增进程epoch及精确最近一小时净化拒绝指标；新告警检查器需要同版Coordinator指标。首次抓取/长时间缺采样只建立基线，不能追回未知区间。告警演练的通知只入本地私有JSONL，再调用实际禁言模块；未联系真实渠道或注册管理器。生产RTO、RPO和演练频率尚未确认，本地300秒/60秒/手动按需只是测试预算。同机备份不等于异地副本。

## M3 本地增量（2026-11-15）

沿用M2固定工具与Cargo.lock，没有新增运行时依赖。发布预检和源码演练仅用Python3.11+标准库、Git、既有Rust工具链；文档合同测试同时调用实际Rust与Python签名代码。安全资料校验复用现有Rust/url/chrono，不另建一套发布器字段解释。

源码演练的Git提交/tag/clone只发生在自己的临时目录，目标源码按精确SHA检出；只共享Cargo下载缓存，target独立。克隆外祖先.env或Cargo配置会使演练拒绝执行，以免借入工作区私有环境或源码覆盖；SQLX_OFFLINE_DIR固定到clone自身元数据。实际结果见M3记录，不等于已公开tag或位级可复现构建。

Nix、flake.lock、远程CI及实际发布仍未验收；Python/Git/本机Cargo通过不能替代这些条件。测试用安全Contact没有被访问，也不代表本项目已开放报告渠道。

## M2 本地增量（2026-11-14）

沿用PG18.7、Rust1.99.1、SQLx0.9和Caddy2.11.9。文档安装器新增固定 `pulldown-cmark 0.13.6`（仅html feature，Cargo.lock固定escape0.11.0、unicase2.10.0）；用于安全静态HTML生成。完整接入bundle为本地草案，未冻结规范。

PG18工具用于真实custom逻辑备份与临时库恢复；无zlib构建使用未压缩dump。Python脚本读取TOML需要Python3.11+，本机使用`.local/venv/bin/python` 3.13.18。恢复测试创建随机库并退出删除，服务/演示不安装工具，不启动系统级PG服务。M2当时没有WAL/PITR；当前增量见M4，仍无异地副本。

本机Caddy的独立CA只在`.local`临时目录；用Python/OpenSSL强制TLS1.3成功、TLS1.2握手失败，CA显式传入而非修改系统信任。系统curl后端不能强制TLS1.3时不能用该curl替代这项证据。IPv6来源桶、限时/限频、换钥在途复核由真实HTTP/数据库测试覆盖；公网防火墙未测。

参考模型模式只验证供应商无关本地命令合同和命令桩，无真实供应商API、模型版本或费用。真实第三方框架/版本/设备仍为空，等待Owner真实接入；详见`demos/spec-spike.md`，项目参考实现不计第三方。

## M1 本地增量（2026-11-14）

PostgreSQL **18.7** 已从官方源码在 `.local/pg18` 编译（UTF8、C locale、无 ICU/readline/zlib；M1 只需 Unix socket），不是系统服务。源码 SHA-256：`441dfae4280cae3e0425e5f876f80559fb0731c140f1aae22b3e5a54d0411bfd`。质量门和演示用 `scripts/with-test-db.sh` 创建独立目录，每次初始化、迁移、运行并正常停止，再清理；业务数据库不受影响。开发测试角色只用于这个隔离集簇。

SQLx 与 sqlx-cli 均为 **0.9.0**。本机实际 `cargo install sqlx-cli --version '=0.9.0' --no-default-features --features postgres --root .local/sqlx-cli --locked` 成功，因而计划第 19 节此前“此命令不再可用”的记录已由实跑结果纠正；项目运行时仍不下载或安装工具。所有应用业务SQL位于store crate，编译查询元数据在它的 `.sqlx/`，prepare/check 也在该 crate 目录执行，不使用 --workspace。

M1 新增依赖由 Cargo.lock 固定：SQLx 0.9.0、Reqwest 0.13.5（禁默认 feature，rustls/TLS 1.3、不跟随重定向）、ed25519-dalek 3.0.0。NFC 和 Cc/Cf 表分别固定 unicode-normalization **0.1.24**、unicode-general-category **1.1.0**，两表都是 Unicode **16.0.0**；显示名小写键用固定 Rust 1.99.1 的 Unicode **17.0.0** 映射。测试直接断言版本，不能笼统声称所有 Unicode 操作版本相同。

新增许可证 allowlist 仅覆盖实际解析出的依赖：ISC（rustls/aws-lc）、Zlib（SQLx 的 foldhash）、CDLA-Permissive-2.0（根 CA 数据）。依赖安全与来源仍由 cargo-deny 检查，未添加漏洞忽略项。

下面“本机环境”中工具版本继续适用；后面的“M0 选择记录”保留 M0 当时未起数据库的历史状态，以本节为当前状态。

## 已实跑的本机环境

| 项目 | 实际值 |
|---|---|
| 系统 | macOS 27.2.1，build 26B131 |
| CPU / Rust target | arm64 / `aarch64-apple-darwin` |
| Rust | Homebrew `rustc 1.99.1 (58cf8f9c3 2026-10-13)`，LLVM 23.1.3 |
| Cargo | Homebrew `cargo 1.99.1 (5f94df478 2026-08-27)` |
| Node | 26.13.1；内置node:test与TypeScript编译/类型检查；无网页运行时npm依赖 |
| Python | Homebrew 3.13.18；项目隔离环境 `.local/venv` |
| Python 签名库 | `cryptography 50.0.2`（本机 venv 实测） |
| cargo-deny | 0.20.2，官方发行文件校验后放于 `.local/tools` |
| Caddy | 2.11.9，官方发行文件经 SHA-512 校验，项目内隔离运行 |
| 本地进程管理 | 前台进程，Ctrl-C 停止；无常驻系统服务 |
| Cargo 缓存 | `.local/cargo`，不写入公开树或提交 |

```text
rustc 1.99.1 (58cf8f9c3 2026-10-13) (Homebrew)
binary: rustc
commit-hash: 58cf8f9c35b355199727627d07ae34a4e2846d67
commit-date: 2026-10-13
host: aarch64-apple-darwin
release: 1.99.1
LLVM version: 23.1.3
```

## M0 选择记录与固定开发环境

`rust-toolchain.toml` 固定 Rust 1.99.1；`Cargo.lock` 固定实际解析出的 Rust 依赖。M0 仅引入本地预览需要的 Axum 0.8.9、Clap 4.6.7、Tokio 1.54.1、SHA-2 0.10.9 及其传递依赖，没有提前实现业务组件。

后续数据库选 **PostgreSQL 18、UTF8、同机部署**，开发、CI、生产和恢复演练保持相同主版本。M0 不需要数据库，本机未安装或启动 PostgreSQL，尚无数据库迁移。SQLx 与 sqlx-cli 选 **0.9.0**；Task 6 引入 SQLx 时必须与 CLI 对齐。Rust 1.99.1 高于计划记录的 SQLx 0.9.0 最低 Rust 1.94.0。不能把选型记录当成数据库测试结果。

`flake.nix` 通过 rust-overlay 读取 `rust-toolchain.toml`，提供 Rust、cargo-deny、PostgreSQL 18、sqlx-cli 0.9.0、Node 26、Python 3.13 + cryptography、Caddy、curl。输入固定为不可变提交：

- nixpkgs：`2074cc672349f14240fb7ea90d509e0b643e9dd4`；
- rust-overlay：`5b87ee0513c29127a01ad06e9cb362ea5ce91e78`，nixpkgs 跟随上项。

本机没有 Nix，因此 **尚未执行 flake 环境验收**，也未生成 `flake.lock`；当前两个输入均固定提交且 overlay 的 nixpkgs follows 根输入。后续使用已有 Nix 环境执行 `nix develop --no-write-lock-file --command scripts/quality-gate.sh`，验证后生成并提交锁文件。CI先执行npm ci --ignore-scripts再运行同一质量门；现已授权普通main首推，是否通过以实际GitHub Actions记录为准，不能把输入存在/本机通过当成Nix或Linux验证。

## 静态服务器和缓存

本地 Rust `serve-public` 仅监听 loopback，ETag 是响应文件内容的 SHA-256；同长度改写也会产生不同的 ETag。生产选 Caddy，M0 在本机 APFS `/dev/disk3s5` 实测其 2.11.9 版本：ETag 由纳秒 mtime 与大小构造，同一秒内两次同长度原子替换可被区分；旧 ETag 返回 200 和新内容，新 ETag 返回 304。完整证据见 [Caddy spike](demos/caddy-spike.md)。

因此本机不需要人为递增 mtime 的补丁。Task 11 的 Publisher 仍必须用同目录新文件 + fsync + rename，不能给变化内容保留旧 mtime；生产文件系统上线前重做同秒、同长测试，不能把 APFS 结果当成 VPS 结果。

## 尚未执行的环境

- 生产 Coordinator：待 VPS 确定；静态域名 `tolight.com`，参与者默认独立 `api.tolight.com`；单机 PostgreSQL/Caddy，不使用 CDN。生产操作系统、架构和进程管理器待部署时记录。
- Hermes/真实第三方 Agent 设备：系统、架构、运行时和常驻条件未答。只影响 Task 15/25，不阻塞本地 M0，不据此反复追问。
- GitHub：2026-11-16 Owner已创建公开仓库 [RaynorZhong/tolight](https://github.com/RaynorZhong/tolight)，并明确授权首次commit及沿用现有 `main` 的普通push；本次不打tag、不部署。此处记录授权与准备状态，不声称提交/推送已完成或CI已通过；远程验证以GitHub Actions实际运行记录为准。正式tag、Nix锁/环境验证、分支保护及生产/真实Agent等验收仍pending，`public_release_ci`不因授权变成passed。 MIT许可证保持；仓库创建/首推授权不替代正式源码发布、CI或分支保护验收。

## 实现时发现的兼容点

Python 3.13 对 IPv4 嵌入 IPv6 的 `.exploded` 输出仍可能含点分十进制。七行签名必须按规范固定为八组小写四位十六进制；示例从 IPv6 的 16 字节地址显式生成该表示，并由第六个签名向量覆盖，不能直接依赖 `.exploded`。

来源：[Rust overlay](https://github.com/oxalica/rust-overlay)、[nixpkgs 固定提交](https://github.com/NixOS/nixpkgs/tree/2074cc672349f14240fb7ea90d509e0b643e9dd4)、[Caddy 2.11.9](https://github.com/caddyserver/caddy/releases/tag/v2.11.9)。版本的本机验证结果以上表及 M0/M1 证据为准。
