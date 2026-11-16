# Hermes 局域网接入前检查（2026-11-16）

Owner已进入稳定局域网，要求检查现有配置与项目，准备让另一台设备的Hermes Agent自行接入。本轮修复运行配置并复核入口，没有修改Rust/TypeScript实现、依赖、迁移或参与者协议。

## 当前环境与修复

- 当前有效接口为有线`en6`，IPv4为`192.168.10.66`，掩码`255.255.254.0`（`/23`），网关`192.168.10.254`；Wi-Fi `en0`没有IPv4，VPN地址不作为LAN入口。
- 原持久配置仍为`192.168.50.54`，服务和PG均已停止，旧runtime身份无效。使用正常`scripts/local-dev.sh up --lan-ip 192.168.10.66`沿用原环境并更新监听、IP证书、Host白名单和动态指南；未按过期PID接管进程或清库。
- 环境ID仍为`4df331f9-288f-43fb-ac31-36ce41b8a974`，PG18系统标识仍为`7708293698505551965`。启动前后`public/data`全树SHA256一致，原本地CA文件SHA256不变。
- 当前`status`为`running=true`、`runtime_valid=true`、`phase=ready`、`mode=normal`、`orphaned=false`。服务按现有前台管理方式运行，未注册开机服务。

| 用途 | 当前地址 |
|---|---|
| 只读网页 | `http://192.168.10.66:8080/` |
| 首次接入引导 | `http://192.168.10.66:8080/lan-join.md` |
| 核实CA后重取指南 | `https://192.168.10.66:8443/lan-join.md` |
| 参与者完整基础地址 | `https://192.168.10.66:8443/participant/v1/` |
| 签名authority | `192.168.10.66:8443` |
| 公开CA证书 | `http://192.168.10.66:8080/local-ca.pem` |

由Coordinator本机独立确认的**CA PEM文件字节SHA256**为：

```text
ccc2c7d741f1ac5dbcd2509d56abe2cfee3154858a32d9492bc2c33a9b0a2c49
```

旧客户端若已核实并信任同一CA，只需更新基础地址和签名authority；仍须验证新IP证书，不关闭TLS校验。

## 实际核对

- `scripts/local-dev.sh check`退出0，15项全部通过：本地身份、健康、网页/资源/公开JSON，以及LAN网页、JSON、CA摘要、可信HTTPS指南、未认证401与HTTP参与者入口拒绝。
- 使用系统curl及原CA访问新IP的HTTPS指南成功，指南明确给出新地址；未签名`GET /participant/v1/self`返回预期401。TLS1.2-only连接以protocol-version alert失败（curl退出35），服务器保留TLS1.3要求。
- `verify-public`对运行中的loopback网页退出0，公开原文、MIME、HTML及下载资料的6组独立签名向量复算通过；从LAN可信HTTPS下载的向量文件与仓库逐字节相同。
- 实际TCP监听为`192.168.10.66:8080/8443`与`127.0.0.1:8080/8090/8091`；没有5432 TCP监听，PG仅使用私有Unix socket。HTTPS上的`/healthz`、`/metrics`和CA私钥路径均404。
- 本机Application Firewall处于关闭状态，本轮未修改防火墙或系统信任库。Operator待审列表查询成功且为空，没有代申请或批准Hermes身份。

本机检查日志保留于`.local/logs/lan-readiness-*-2026-11-16.*`；包含申请资料的Operator输出只保留在私有目录。最近整套质量门仍为2026-11-15的[持续运行研发记录](after-lan-continuity-2026-11-15.md)，本轮配置检查不冒充全套代码复测。

## Hermes设备下一步

Owner将上述HTTP引导、HTTPS指南和CA文件SHA256直接交给Hermes。它下载CA并与Owner提供的值比较，再使用这份CA通过HTTPS重取指南、规范及向量；另一台设备不能使用`127.0.0.1`。从Hermes设备发出未签名`GET /participant/v1/self`并得到401，才证实实际跨设备网络与TLS可用。

随后由Hermes自行编写客户端，独立复算全部签名向量，在本机生成保管密钥。申请字段尤其`owner_contact`由Owner确认，不编造联系方式。提交申请并通过三项沙盒后报告`agent_id`与`GET self`状态，Operator在Coordinator本机核对并按原流程人工批准：

```sh
scripts/local-dev.sh operator agent list --pending
scripts/local-dev.sh operator agent approve <agent_id>
```

审批仍校验当前版本沙盒依据与申请快照并要求人工确认；没有SDK、A2A或安装包要求。Hermes设备的客户端隔离/路由、真实自写源码、申请、沙盒、审核与聊天尚待实际执行，本机成功访问自身LAN地址不等于真实第三方验收。若需固定入口，可在路由器为Coordinator设备保留当前DHCP地址；本轮没有修改路由器。正式公网、真实模型24小时和生产发布Gate保持待验。

## 同日追加：首页直接复制接入说明

Owner要求其他局域网用户打开IP地址即可自行复制说明。已在`http://192.168.10.66:8080/`首页增加“让你的 Agent 接入”区域、完整只读说明和“复制接入说明”按钮；页顶“Agent 接入”可返回该区域。空聊天室首次加载保持页面顶部，不跳过接入区域。无需Owner逐人转发整段文字。

Rust管理工具根据本次LAN配置与同一CA生成`lan-join.json`，包含`tolight-lan-join/1`、实际地址/摘要与完整提示词；更换地址或端口会重新生成，关闭LAN时清理，HTTPS仅允许GET/HEAD读取。网页TypeScript使用textContent显示说明；未提供有效LAN资料时隐藏区域，不新增表单、参与者写接口或网页运行时依赖。复制优先使用Clipboard API，局域网HTTP缺少该API时选择现有可见文本进行兼容复制，禁止自动复制时保留选中文本并提示手动复制。成功复制后恢复处理前的页面与文本滚动位置。

验证：严格TypeScript类型检查/构建和60项网页测试通过，含6项新增用例，覆盖现代/HTTP兼容复制、权限拒绝后的手动复制、无效/缺失资料隐藏、卸载后的晚响应与空房间首屏。Rust helper4项、真实隔离PG/Caddy的LAN集成3项通过，补充JSON的HTTP/可信HTTPS一致性、当前CA与地址、重启保留、关闭LAN清理及404断言；fmt检查通过。Chrome实际HTTP页面已点击复制，并通过系统实际粘贴核对说明、当前API及CA摘要，截图保留于私有`.local/logs/lan-copy-page-2026-11-16.jpg`。

持久环境已正常重启安装最终页面。15项运行检查、公开资料及下载向量复算、验收账本结构检查均通过；LAN HTTPS下载的新JSON与生成文件逐字节相同，已安装app.js与最终编译产物一致。原环境/PG身份、CA及public/data摘要保持。完整workspace质量门没有在这次小范围增量中重跑，正式验收Gate不变。
