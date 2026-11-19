# chrono依赖迁移（2026-11-19）

Owner要求修复tolight的chrono依赖问题，保持Rust + TypeScript，完整质量门通过后推送GitHub；不修改tolink源码。

## 依赖与变更

此前cargo deny在基于61c9db8的checkout发现`RUSTSEC-2026-0543`：chrono0.4.45停止维护，advisory标记`informational="unmaintained"`、`patched=[]`，没有可通过升级chrono消除告警的版本。该记录描述维护状态，不是已复现的可利用漏洞。项目既有业务直接依赖，也有SQLx0.9 chrono feature，因此必须一并移除。

- workspace及各crate统一采用time0.3.55，SQLx保留0.9.0并切为time feature。使用原生OffsetDateTime/Duration，不保留chrono兼容包装或旧时间库。
- Cargo.lock移除chrono、iana-time-zone及相关平台附属包；新lock SHA-256为`35391ecc7b1e7eeb04585c544892db0bb50aedc08f675205e83f28a18ef53561`。原lock摘要为`ff8e3d1c9fa01b6f93c3f063d5605124f168b6de516c2cdba468719055d98352`。deny.toml及质量门规则保持。
- 序列化使用core共享适配器，保留原来的UTC RFC3339字符串、0/3/6/9位自动精度、秒/微秒/纳秒显式格式和Operator显示；Option保留缺失/null。公开事件、管理接口、审计、控制指纹、恢复快照和备份不变成time默认元组。
- SQLx继续读写TIMESTAMPTZ，不更改数据库schema。保留PG微秒截断、纳秒限流边界、负时差及Retry-After向上取整。参考客户端预算与沙盒场次明确按UTC日计算。
- HTTP Date仍输出GMT，PITR继续接受PG的空格分隔及`+HH`/`+HHMM`/`+HH:MM`偏移。原配置、自动运维状态及参考客户端整数秒/纳秒持久状态可继续读取。

## 解析边界

time的RFC3339解析器接受的分隔符比旧实现宽，且会把`:60`闰秒改成前一纳秒。共享入口保留T/t/空格分隔符限制；闰秒明确报错，避免静默改变Operator/PITR目标。带时区的9999年边界若转UTC越界，返回解析错误而非panic。项目生成的普通UTC历史数据保持兼容；非规范宽松人工日期及无法无损表示的闰秒不再被隐式归一化。

## 验证

完整质量门`scripts/quality-gate.sh`最终退出0：

- 392项Rust测试全部通过，另复跑1项真实源码启动/重启/双备份生命周期，共393次执行。新增8项回归覆盖共享时间格式与解析、旧Agent/控制快照JSON、非UTC时钟的日预算及PG恢复目标；真实逻辑恢复、WAL/PITR及控制状态保留均通过。
- fmt、workspace/all-targets/all-features严格clippy、SQLx元数据核对及cargo deny原规则全部通过；advisories/bans/licenses/sources均为ok，依赖图不含chrono/iana-time-zone。104项TypeScript与6组独立签名向量全部通过。
- 第一轮发现两处恢复测试把原生time值直接序列化成数组预期；修为明确RFC3339字符串后继续逐字比较，生产输出与断言强度保持。最终日志为私有`.local/logs/chrono-migration-quality-r2-2026-11-19.log`，没有跳过或忽略失败项。

现场已完成原LAN环境的正常停止/源码模式启动，15项检查通过，HTTPS指南与源文件一致。原environment_id、PG cluster、CA摘要、3个Agent身份/禁言信息、3条Operator审计及31条公共事件保持；管理接口时间字段仍为RFC3339字符串，升级前生成私有逻辑备份。未调用真实模型或切换真实客户端。

GitHub普通main推送以完整本地质量门通过为前提；远端结果以该提交对应的[Quality gate](https://github.com/RaynorZhong/tolight/actions/workflows/ci.yml)实际记录为准，不把本地通过冒充远端通过。上一轮已完成但因chrono阻断暂未推送的tolink接入文案/审计记录随本次源码一并版本化，不修改tolink仓库。

tolink源码、真实客户端身份/游标与模型调用没有更改；没有新增A2A/MCP、安装包、系统服务或依赖告警豁免。正式生产/真实外部Agent等验收Gate保持原状。
