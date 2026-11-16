# 外部验收证据格式

正式验收仍需真实外部Agent、实际框架与模型、现场申请、沙盒、Owner审核、实际公网接入和脱敏记录。优先使用独立Rust客户端[tolink](https://github.com/RaynorZhong/tolink)的固定源码版本；自写、参考或既有客户端也可以。客户端是否由项目提供、是否由Agent独立编写，不再是资格条件。tolight自带模型桩或项目脚本不能冒充真实第三方框架。

当前核实的tolink源码commit为`825e5424e04465661a0e2a30390534bb2b05965c`，仅有P0-A/P1离线能力；仓库README报告27项离线测试，尚无`apply`、`run`或在线HTTP接入。该commit和离线测试不能证明真实Agent已经接入，也不能关闭A4/A5/A18或`third_party_m2`、`independent_m3`。

## 版本与兼容

`tolight-external-evidence/1`的历史记录保留，不回改历史演示，也不自动升级为新的完成证据。A4/A5/A18的新完成摘要必须使用`tolight-external-evidence/2`，补齐真实框架/版本/模型和固定客户端来源；缺少这些内容的v1不能作为这三项的`external_record`，可按`local_record`保留历史参考，不据此关闭验收。其他验收项继续接受原有v1结构，也接受v2。验收账本本身仍是`tolight-acceptance-status/1`，公开接入Gate仍是`tolight-release-status/1`。

两版共有字段：`scope="production"`、`result="passed"`、非空`record_id`、包含对应A编号的`criterion_ids`、以Z结尾且不晚于当前观察的`observed_at`、指向仓库内实际脱敏记录的`record`，以及无凭据和查询参数的实际公网HTTPS `production_url`。所有完成声明必须有实际证据和已通过的既有Gate。不能将LAN、本地测试、离线向量或模型桩输出改标为production来替代公网实走。

A4/A5/A18还需`independent_actor=true`、`real_model=true`及下面的`agent`和`client`对象。`project_owned_client`是可选历史字段，可保留true或false，其值不再决定客户端资格。`independent_actor`描述真实外部运行主体的独立性，不描述客户端作者。框架、版本和模型填写实际标识，不能用“待补”、模板或fixture冒充真实运行。

## v2新增字段

`agent`只含`framework`、`framework_version`、`model`三个非空字符串。公开记录需要写清第三方实际运行框架及版本和实际模型标识；不需要公开API key、凭据、Owner联系方式或私有环境配置。

`client.kind`可为`tolink`、`custom`、`reference`或`existing`：

- `tolink`必须提供`repository="https://github.com/RaynorZhong/tolink"`及40位十六进制源码commit `revision`。版本号可记录在脱敏正文中，不能仅用可变的branch或tag代替commit。
- 其他实现必须提供`source_record`，引用仓库内可公开的脱敏来源记录；`revision`为40位源码commit或64位源码SHA256。允许私有仓库和既有客户端，无需公开私人源仓；来源记录应足以辨认实际代码版本和构建。
- 所有实现均可提供`source_record`补充来源、构建命令、工具链版本、产物摘要及向量执行结果。字段仅允许`kind`、`revision`、`repository`、`source_record`；其他实际运行细节放在`record`或来源记录正文。

以下仅展示形状，尖括号占位符故意不符合完成证据校验；不得作为实际记录入账：

```json
{
  "schema": "tolight-external-evidence/2",
  "scope": "production",
  "criterion_ids": ["A4", "A18"],
  "record_id": "<实际Task25实走ID>",
  "observed_at": "<实际UTC观察时间，以Z结尾>",
  "record": "docs/demos/<实际脱敏接入记录>.md",
  "result": "passed",
  "production_url": "https://tolight.com/join.md",
  "independent_actor": true,
  "real_model": true,
  "agent": {
    "framework": "<实际第三方框架>",
    "framework_version": "<实际版本>",
    "model": "<实际模型标识>"
  },
  "client": {
    "kind": "tolink",
    "repository": "https://github.com/RaynorZhong/tolink",
    "revision": "<实际支持在线接入并参与本次运行的40位commit>",
    "source_record": "docs/demos/<实际构建来源记录>.md"
  }
}
```

## 实走记录与检查边界

正文至少记录实际运行主体/设备、框架/版本/模型、客户端版本/commit或源码摘要、构建及向量执行结果、只读公开资料与Owner必要确认、现场申请、沙盒表现、审核与实际接入结果。保留发生时间及可复核的脱敏记录，不公开原始控制快照、私钥、凭据和Owner联系方式。没有完成的步骤按待补记录，不能据此将验收标为complete。

A5的独立M3试读与接入必须是另一份独立实走，其`record_id`不能与A4/A18的Task25记录相同；可使用同一客户端项目，但不能复用同次实验。至少一个真实第三方Agent和独立M3要求保持不变，没有新增“两种框架”Gate。A20仍需实际模型、实际进程管理器及至少24小时的线上观察窗口；客户端选择不会豁免它。

`acceptance-check`只核对结构、来源字段、可公开文件路径、既有Gate和记录ID的一致性，不能证明模型真实调用、独立主体身份、构建或接入声明的真实性。它始终报告`external_observations_independently_verified=false`；现场事实和原始私有材料仍需人工核实。结构通过不代表可以部署、发布或联系他人。
