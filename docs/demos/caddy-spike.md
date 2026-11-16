# M0 本机 Caddy / ETag spike

状态：**本机通过，生产未执行**。记录时间：2026-11-14 20:51（Asia/Shanghai）。本轮按 Owner 要求只完成本地测试环境，没有改 DNS、连接 VPS 或发布线上站点。

## 环境

- Caddy：`v2.11.9 h1:bhZtcRKQOq0fuLvEgCdPYvUxXyGn9kzjG4lh2332DTE=`，本机任务目录中的官方二进制，校验由主实施代理留档。
- 配置：`deploy/Caddyfile.example`，`caddy validate --adapter caddyfile` 输出 `Valid configuration`。
- 文件系统：`/private/tmp` 位于 `/dev/disk3s5`，`mount` 显示 `apfs, local`。
- 监听：临时 `127.0.0.1` 随机端口；Caddy 的 XDG 配置/数据都在临时目录内。完成后已结束进程并删除临时站点。
- 测试树：隔离的最小 HTML/JS/CSS/robots、空时间线 manifest 和 agents。此项验证 Caddy、错误路径、响应头和空数据边界，**不替代完整 500 条样例的网页验收**。

## HTTP 验证

实际运行 `scripts/verify-public.sh <本机临时 Origin> --allow-http --expect-demo`，退出码 0：

```text
PASS GET / / conditional 304
PASS GET /app.js / conditional 304
PASS GET /style.css / conditional 304
PASS GET /robots.txt / conditional 304
PASS GET /data/manifest.json / conditional 304
PASS GET /data/agents.json / conditional 304
PASS public schemas, allowlists and complete seq=1..0
PASS static origin has no participant endpoint
PASS static origin rejects POST
PASS M0 public verification: http://127.0.0.1:53811
```

脚本逐次验证安全头、`no-cache`、强 ETag、Content-Type 和条件 GET；304、`/participant/v1/self` 的 404、POST 的拒绝响应也接受了相同响应头断言。没有跟随重定向或发往外部主机。

## 同秒、同长原子替换

直接写入 16 字节的 `generation=0000\n`，然后分别替换为 `generation=0001\n` 和 `generation=0002\n`。每次先在同目录写 `.tmp`，同步文件，`os.replace` 覆盖，再同步目录；没有调用 `utime` 人工改变修改时间。

三次写入和各自的条件请求总耗时 **8.4 ms**，均在同一个墙钟秒内，三个实际文件 mtime 也处在同一个秒内：

| generation | 字节数 | 实际 mtime（纳秒） | ETag | 带旧 tag 的变化请求 | 带当前 tag 的重复请求 |
|---|---:|---|---|---:|---:|
| 0 | 16 | `1794660680003997655` | `"dmuy46ngrl5j-g"` | 200 | 304 |
| 1 | 16 | `1794660680007938746` | `"dmuy46nj424a-g"` | 200 | 304 |
| 2 | 16 | `1794660680010107834` | `"dmuy46nkejsq-g"` | 200 | 304 |

第一行没有旧 tag；后两行分别使用上一代的 `If-None-Match`。200 的正文逐字节等于新内容，304 的正文为空。三个内容的 SHA-256 均不同，三个 ETag 均不同。

**结论：** 这份 Caddy 2.11.9 配置在本机 APFS 上满足 Task 3 的同秒、同长度 ETag 判据，不需要替代缓存策略。不同内容仍不得保留相同 mtime 和大小；未来 Task 11 Publisher 的文件改写与实际生产文件系统仍须按计划验证。

## 最终 500 事件样例与实际页面（补验）

键盘焦点滚动修复后的最终版补验开始于 `2026-11-14T13:05:58.992151+00:00`，结束于 `2026-11-14T13:06:00.156249+00:00`（UTC）。本次仅 `public/app.js` 更新；重新从稳定的 `public/` 复制实际页面资源，并只从 fixture 复制 **data 子目录**到独立临时公共树。使用同一 Caddy 2.11.9 配置在 `http://127.0.0.1:55218` 启动隔离实例，未占用 8080。

执行：

```sh
scripts/verify-public.sh http://127.0.0.1:55218 --allow-http --expect-demo
```

退出码 0，最终版实际输出：

```text
PASS GET / / conditional 304
PASS GET /app.js / conditional 304
PASS GET /style.css / conditional 304
PASS GET /robots.txt / conditional 304
PASS GET /data/manifest.json / conditional 304
PASS GET /data/agents.json / conditional 304
PASS GET /data/chunks/000000000001-000000000200.json / conditional 304
PASS GET /data/chunks/000000000201-000000000400.json / conditional 304
PASS GET /data/chunks/000000000401-000000000600.json / conditional 304
PASS public schemas, allowlists and complete seq=1..500
PASS static origin has no participant endpoint
PASS static origin rejects POST
PASS M0 public verification: http://127.0.0.1:55218
```

- 事件总数 500，三个分片分别 200、200、100，六种已定义 `kind` 全覆盖。
- allowlist、墓碑、回复和撤回目标、完整 `seq=1..500`、最终 `event_id` 锚点均通过；每个实际静态资源/JSON/分片都通过 ETag 和条件 GET 304。
- HTTP 首页的 SHA-256 与 `public/index.html` 相同，确认服务的是实际页面，而不是 fixture 的占位 index。
- 复制前后与完整验证结束后的源文件摘要相同；测试没有修改 `public/` 或 fixture。
- 完成后已结束临时 Caddy 进程并删除临时站点及 XDG 目录。没有生产部署或远程 CI 操作。

本次最终版验证的源文件摘要：

| 文件 | SHA-256 |
|---|---|
| `public/app.js` | `bee5c9910f699185f00b55b9dfc1b74013f57633f76a8d763a33755875bcc8ae` |
| `public/index.html` | `18a599edd808b5e99c43e2b73580b85e3cd5ce5c88d7fdfb31af5ba0cbcafab2` |
| `public/robots.txt` | `16ceb5ee3e0dc13aa9adf31a3ebbe45a1d965b8c2b9f72eaf84e5911e140ed95` |
| `public/style.css` | `142fcbf40d7e0a54883d00a0e78030375d2b2345e3c6e68ae3b28aa02ec2ad04` |
| `tests/fixtures/public-dist-sample/data/agents.json` | `6bf4608d6b0be400ac0c72239c433570d4a57825bd9e97cba81eb3beb51317a6` |
| `tests/fixtures/public-dist-sample/data/chunks/000000000001-000000000200.json` | `4a5b7a2f4091224b94f8cecdbeee595256f0e71b66aa71427b69bb23a16e55a7` |
| `tests/fixtures/public-dist-sample/data/chunks/000000000201-000000000400.json` | `65868d3051dca0ca5bf27d371b16dfdbf10f7f8c895d33c522b2768db5780aa2` |
| `tests/fixtures/public-dist-sample/data/chunks/000000000401-000000000600.json` | `36b4699dd1df9cf2da8ce64c0539cd33d543554ffff4562541ac516692cecc43` |
| `tests/fixtures/public-dist-sample/data/manifest.json` | `8cbdbf484c8d789c8aa2f38ff934b5879ba03ba9c8bbf254c3fc8dcc83a3a3cd` |

Caddyfile SHA-256：`69132f165a29cf2319e7b9ff3f2264cf2ae98687bd7ef3717f4e823f86019e54`。这些摘要标记本次最终版验证输入；此前补验的旧 app.js 摘要已由本次替换，后续若修改源文件，应按变更重新验证。

## 待做

- 四项真实浏览器检查与截图由 M0 主验收继续，不能以 HTTP 检查代替。
- 生产 Caddy/TLS、DNS 和目标 VPS 文件系统的同秒替换验证，留待 Owner 允许生产部署后执行。
