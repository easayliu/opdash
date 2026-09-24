# 云账单

费用页的数据由 [goscan](../../goscan) 按账期从火山引擎、阿里云拉取后写入 ClickHouse，共三张表：`volcengine_bill`、`alicloud_bill_monthly`、`alicloud_bill_daily`。本文说明 opdash 查询这几张表时的处理：为什么要在查询中再去重一次、表名如何识别、表结构经历了哪些调整，以及成本归属与手动同步的规则细节。费用页的使用方法见 [README](../README.md#费用页)。

## 查询时去重

### 为什么需要

这三张表是 `ReplacingMergeTree`，依靠「同一账期重复拉取不会翻倍」来保证幂等。**在 2026-09 的表结构调整之前，这一保证在集群上并不成立。** 2026-09-22 查询线上（`logs` 库，`log` 集群 3 分片）确认：

```sql
SELECT name, engine_full FROM system.tables WHERE database='logs' AND name LIKE '%bill%';
-- alicloud_bill_daily_distributed   Distributed('log', 'logs', 'alicloud_bill_daily_local', rand())
-- volcengine_bill_details_local     ReplacingMergeTree
--     ORDER BY (BillPeriod, ExpenseDate, InstanceNo, ExpenseBeginTime, Product, ElementCode, PayableAmount)
```

分片键是 **`rand()`**：同一行写入两次，两份会落在**不同的分片**上。而 `ReplacingMergeTree` 的去重只发生在分片内部的合并中，`FINAL` 同理，goscan 同步完成后执行的 `OPTIMIZE TABLE ... ON CLUSTER FINAL` 也一样，跨分片的副本无论如何都无法收敛。goscan 的日调度每天会重新拉取当月数据（`sync_mode` 无论取 `standard` 还是 `sync-optimal`，都不是「先删后插」），一个月下来同一行最多可能存在三份，账单金额随之翻倍。

因此 opdash 默认（`--bill-dedupe=group`）在查询中再去重一次。

### 去重键本身并不唯一

2026-09-23 对账时发现：阿里云会把一笔尾差调整单独列为一行，维度与正常账单完全相同，只有金额不同；同一台机器同月先包月、再转包年，也会出现两行同键的账单（如 1,612.29 与 13,768.2）：

```text
billing_date  product    instance_id               pretax_amount  pretax_gross_amount
2026-06-01    短信服务    <账号>:<短信签名>         409.41               409.41
2026-06-01    短信服务    <账号>:<短信签名>         -0.005                    0
```

早先的做法是按排序键分组、其余列一律取 `any()`，于是会在这两行之间任选一行：2026-08 的百炼因此少计 34,837.57，阿里云 8 月按量合计少计 3,677.03。现在的去重分两步：

```sql
SELECT _period, _amount FROM (
  SELECT any(billing_cycle) AS _period, any(toFloat64(pretax_amount)) AS _amount,
         max(updated_at) AS __version,
         max(max(updated_at)) OVER (PARTITION BY <排序键>) AS __latest
  FROM logs.alicloud_bill_monthly
  WHERE ...
  GROUP BY <排序键>, pretax_amount, payment_amount, pretax_gross_amount   -- ① 金额也进入分组
)
WHERE __version >= __latest - INTERVAL 60 SECOND                           -- ② 只保留最近一次同步
```

1. **金额也进入分组**：完全相同的副本（重复拉取、写入重试留下的）合并为一行；金额不同的并列行各自保留，随后相加。
2. **按排序键只保留最近一次同步写入的行**：金额进入分组之后，重新拉取之前的旧版本（云厂商月中调整过金额）就不会再与新版本合并，因此按 `updated_at`（也是 `ReplacingMergeTree` 的版本列）将其筛除。同一次同步中的并列行相隔不过数秒（线上 2026-06 至 08 月最多 2.2 秒），同一账期的两次同步则至少相隔数分钟，60 秒的窗口足以区分二者。

这正是 `ReplacingMergeTree(updated_at)` 合并之后应有的结果，区别在于引擎会把并列行也吞掉，而这里不会。按这套去重，opdash 对 2026-07、08 两个月阿里云后付费账单与手工台账逐条业务线核对，差额均在一分钱以内。代价是需要把这几个月的数据读出来聚合一次，而账单每月不过数万到数十万行，比日志表小四个数量级，可以忽略。

**`--bill-dedupe=final` 同样会丢失并列行**：`FINAL` 即引擎的去重语义，同键只保留版本最新的一行。在线上的表按下文「表结构的演进」重建、每一行账单都有唯一的键之前，不要切换到 `final`。`group` 对任何分片键都是正确的。

### 实现细节

* **去重键从 `system.tables.sorting_key` 读取**，而非写死（读取不到时才退回 goscan 当前 DDL 的定义，并记录一条 warn）。若 goscan 修改了 ORDER BY 而 opdash 没有跟进，按旧键去重会**悄无声息地少计金额**，这类错误很难被察觉。Distributed 表本身没有排序键，所以查询的是其下的 `_local` 表。
* **金额的转换函数按库中实际的列类型选择**：`String` 用 `toFloat64OrZero`，`Decimal` 用 `toFloat64`（混用会被 ClickHouse 以错误 43 拒绝），新旧两种表结构都能查询。
* **子查询中的别名一律加下划线前缀**（`AS _instance_id`）。别名与真实列名相同时，ClickHouse 的分析器会把 WHERE 中的列名解析为聚合结果，模糊搜索会直接报错 `184: Aggregate function any(instance_id) is found in WHERE`。

## 表名的识别

goscan 的表名改过两轮：集群上的 `_distributed` 后缀已取消（与 logpipe 一致，Distributed 表直接使用基础名），火山引擎的表从 `volcengine_bill_details` 改名为 `volcengine_bill`。两轮改名都是「先按新名称重建表、后改配置」，其间库中会有多个名称并存。2026-09-22 16:43 的那次 DDL 之后，`logs` 库中同时存在 `volcengine_bill_details`（刚建的 Distributed 表）、`volcengine_bill_details_distributed`（上一轮遗留）和 `volcengine_bill_details_local`（实际存放数据的表）；到 16:57 重建为新名称并清理旧表后，才收敛为现在的三张表。

因此 opdash 按一组候选名依次查找，**无须配置即可对应**：基础名 → `<基础名>_distributed` →（仅火山引擎）`volcengine_bill_details` → `volcengine_bill_details_distributed`。`_local` 始终不在候选之列：它只是一个分片的数据，查出的金额只有三分之一。显式配置了 `--volcengine-bill-table` 且名称不同的部署，不再回退到旧名称。

## 表结构的演进

查询时去重是**规避**，而非**根治**。根治需要修改 goscan 的建表语句（`pkg/ddl`），分两个阶段完成。

### 2026-09-22：分片键与排序键

| 修改内容 | 修改前 | 修改后 | 原因 |
| --- | --- | --- | --- |
| Distributed 分片键 | `rand()` | `cityHash64(<排序键>)` | 同一行的多次写入落在同一分片，`ReplacingMergeTree` 与 `FINAL` 才能生效 |
| 排序键 | 末位是金额（`PayableAmount` / `payment_amount`） | 只含业务身份：火山引擎用 `BillDetailId`，阿里云用「账号 + 产品 + 实例 + 计费方式 + 拆分 / 调整记录」 | 云厂商月中调整金额（退款、优惠重算、发票折扣）后，新旧两行排序键不同，两行都会保留，而这恰恰是最应当收敛的情形 |
| 引擎 | `ReplacingMergeTree` | `ReplacingMergeTree(updated_at)` | 后拉取的一份胜出；金额被修正的重复在任何去重键下都是两行不同的数据，只有带版本列的引擎才能判断新旧 |
| 火山引擎金额列 | `String` | `Decimal(20, 8)` | 求和不必再逐行转换，不丢精度，排序与跳数索引也可以使用 |
| 分区表达式 | `toDate(ExpenseDate)`、`parseDateTimeBestEffort(...)`，空值抛出异常 | `parseDateTimeBestEffortOrZero(...)` | 云厂商只要返回一条日期为空的账单，原写法会导致整批 INSERT 失败；现在脏值落入 1970-01 分区 |

当时三张表刚刚建立、尚无数据，修改表结构无须迁移。

### goscan v0.5：每一行账单都有唯一的键

2026-09-23 与手工台账对账时发现，阿里云账单中存在**排序键完全相同、只有金额不同**的两行（见上文「去重键本身并不唯一」）。这对 `ReplacingMergeTree(updated_at)` 是致命的：两行同键，按新的分片键必然落在同一分片，**合并时只保留 `updated_at` 较大的一行，另一行被永久删除**。`updated_at` 相同时（同一批写入，精确到毫秒也可能相同）保留哪一行不确定，丢掉的可能是金额较大的那一行。当时 `logs.alicloud_bill_monthly` 中就有一组尚未合并的：

```text
2026-08  大模型服务平台百炼  <账号>;<应用>;<模型>;input_token;0    34837.5724  03:04:47.538
2026-08  大模型服务平台百炼  <账号>;<应用>;<模型>;input_token;0       -0.0045  03:04:47.197
```

这一组恰好是大额的一行晚了 0.34 秒，合并后会被保留；顺序反过来，这笔费用就丢失了。opdash 的查询时去重能把并列行都计入，但**存储层合并掉的行，查询时无从找回**。

goscan v0.5 修复了这一问题：阿里云两张表的排序键加入了 `item`（订单 / 后付费账单 / 退款 / 调账）与 `line_seq`（同一次拉取中其余各列都相同的行，按拉取顺序编号 0、1、2……），每一行账单从此都有唯一的键；重新拉取某个账期之前先按分区清空，云厂商撤销的行不会残留。opdash 的静态兜底去重键已同步更新（通常用不到，去重键从 `system.tables.sorting_key` 读取）。

### 表必须重建才生效

引擎、排序键、分区键与列类型都在建表时确定，`CREATE TABLE IF NOT EXISTS` 对已存在的表不起作用；只执行 goscan 的补列语句，会把 `item`、`line_seq` 两列加上，但它们不在排序键中，同键的行仍会被合并。线上的阿里云两张表需要按 goscan README「2026-09 的结构调整不能原地升级」一节先删除再重建，然后重新同步。重建之前，`--bill-dedupe` 保持默认的 `group`。

## 成本归属

账单只回答「哪个产品花了多少」，而对账真正需要回答的是「哪条业务线花了多少」，二者之间隔着一层归属关系：一台机器属于谁，一项共用服务按什么比例分摊给哪几条业务线。**这层信息不在账单中**，云厂商也无从得知，只能由部署方提供，因此它是一份外部配置（`--bill-alloc rules.toml`），而非代码中的常量：业务线名称、实例的内网地址都属于内部信息，不应随仓库分发。

### 规则文件

规则文件只描述四件事，完整示例见 [`examples/bill-alloc.toml`](../examples/bill-alloc.toml)：

```toml
lines = ["业务线甲", "业务线乙", "公共资源"]   # 业务线，顺序即页面上的顺序
unmatched = "公共资源"                        # 未命中任何规则的费用归入哪条业务线，可省略

[[include]]                                   # 只统计命中其中任一条的账单行，可省略
subscription = ["PayAsYouGo", "按量计费"]

[prepaid]                                     # 预付费按服务期摊到各月，可省略
subscription = ["Subscription", "包年包月"]
lookback_months = 36                          # 向前回溯购买记录的月数，不截断服务期

[[rules]]                                     # 自上而下匹配，命中第一条即停止
name = "业务线乙的专用机器"
product = ["云服务器 ECS"]                    # 跨云统一的维度，与排行下拉框中的选项同名同义
columns = [{ name = "intranet_ip", any_of = ["10.0.1.11"] }]   # 维度无法表达的条件，直接匹配原始列
to = "业务线乙"                               # 整笔归入一条业务线

[[rules]]
name = "ECS 其余部分按机器数拆分"
product = ["云服务器 ECS"]
split = { "业务线甲" = 130, "业务线乙" = 400 }  # 或按权重分摊给多条业务线，只看相对大小
```

### 规则语义

* **命中即停止，所以顺序有意义。** 范围窄的规则（某几台机器属于谁）写在前，范围宽的规则（同一产品的其余部分按比例拆分）写在后。这既是为了让窄规则有机会命中，也是为了避免同一笔费用同时满足两条规则而被计入两次。线上确实遇到过：弹性伸缩组释放的内网地址被另一批机器复用，若先按地址匹配，那部分费用会在两条业务线上各计一次。
* **分类在 ClickHouse 中完成。** 规则被翻译为一条 `multiIf`，与去重、筛选在同一条 SQL 中执行，取值一律绑定为参数。账单每月数万行，取回进程内再分类既慢又无必要。
* **要匹配的原始列在某张表上不存在时，该规则对这张表整体不生效**，而不会退化为「一律命中」。`intranet_ip` 只有阿里云有，若把缺列的条件视为恒真，一条「某几台机器归业务线乙」的规则会把火山引擎的全部费用也计入业务线乙。
* **预付费（包年包月）单独走摊销。** 这类账单在购买当月一次性出账，若按出账月计入，那个月会凭空多出一大块，日均与月度预估随之失真。配置 `[prepaid]` 之后，每一笔购买按自身的 `service_period` 摊到各月：一台包年的机器在十二个月中各计十二分之一。命中 `[prepaid]` 的行会**自动从按出账月计入的路径中排除**，不会重复计算；归属仍使用同一套 `[[rules]]`，在规则中写上 `subscription = ["Subscription"]` 即可为预付费单独指定归属（例如包年的数据库归属应用、按 ECS 比例拆分，而后付费的数据库归基础保障）。需要注意三点：
  * 升降配只收取或退还差价，其 `service_period` 记录的是剩余天数而非整个服务期，按同一套方法摊销即可，这笔钱确实发生了；
  * 火山引擎的账单没有服务期列，无法摊销，这部分仍按出账月计入，**不会两边都不计**；
  * 阿里云接口只保留 18 个月的账单，更早购买且仍在服役的机器不在库中，这部分成本无法看到。
* **日均只计算后付费，月度预估分两段相加。** 预付费按月摊销，除以天数没有意义，所以不参与日均；月度预估因此为「后付费日均 × 目标月天数 + 该月的预付费摊销」。后一段不是估算：已发生的购买摊到未来几个月的金额是确定的，接口会一并给出区间之后十二个月的摊销，页面据此计算下个月的预估。
* **日均的分母是「有账单的天数」，而非自然月的天数。** 当月账单尚未出齐，按 30 天摊薄只会低估日均，越接近月初偏差越大。月度预估则相反，按日均 × 目标月的自然天数计算。页面上可将日均的窗口收窄到最近 7 / 14 / 30 天，以避开月初扩容等早期波动；窗口以**各表最后一个有账单的日期**为基准向前回溯，而不是以今天为基准：账单滞后一至两天出具，从今天倒推会平白少算几天，而且两朵云的同步进度未必一致。
* **未命中规则的金额始终单独列出。** 即使配置了 `unmatched` 将其并入某条业务线，页面仍会标出这部分金额及其占比，以便了解规则还有多少没有覆盖。
* **不配置规则也可以使用**：分析视图照常给出按产品的日均与月度预估，只是没有业务线这一层。

规则文件解析或校验失败时，opdash 启动即退出，而不会静默降级为「没有规则」。

## 手动同步的转发

账单不是推送过来的，而是由 goscan 按自己的 cron 从云厂商接口拉取。刚接入、补历史账期或当天的调度尚未执行时，页面上没有数据，所以费用页提供「同步账单」，把这一操作转交给 goscan：

```text
浏览器 ──▶ POST   /api/bills/sync                ──▶ POST   {goscan}/sync              登记后台任务，取得 task id
浏览器 ──▶ GET    /api/bills/sync/{id}/events    ──▶ GET    {goscan}/tasks/{id}/events 进度推送（SSE），done 之后刷新页面数据
浏览器 ──▶ GET    /api/bills/sync/{id}           ──▶ GET    {goscan}/tasks/{id}        推送不可用时退回每 2 秒轮询一次
浏览器 ──▶ DELETE /api/bills/sync/{id}           ──▶ DELETE {goscan}/tasks/{id}        停止：写完当前这一趟再停
浏览器 ──▶ GET    /api/bills/sync/running        ──▶ GET    {goscan}/tasks             这朵云当前是否有同步在进行
```

接口口径以 goscan README 的「手动同步（给 opdash 对接）」一节为准（swagger 注解与实际行为有出入）。

* **opdash 仍然不写库。** 账单由 goscan 拉取后写入 ClickHouse，opdash 只转发「拉取一次」「停止」两个指令，自身的每条查询照旧带 `readonly=2`。这也是 opdash 唯一会向外发出改变状态的请求。
* **之所以经由 opdash 转发**，是因为 goscan 的 HTTP 接口没有认证（仅供集群内访问），而 opdash 有登录。让页面直连 goscan 就等于把它暴露给浏览器。
* **进度依靠推送而非轮询**（goscan v0.5 起）。任务每发生一次变化（受理、开始、写完一批、切换账期、结束），goscan 推送一帧，opdash 逐帧转给浏览器，并转换为与轮询接口相同的 JSON；收到 `done` 后页面主动关闭连接，否则 `EventSource` 会不断重连。旧版 goscan 没有这个接口，opdash 返回 404，浏览器不再重连，页面随即改为每 2 秒轮询一次。进度以「趟」为单位（一个账期 × 一种粒度），一趟可能持续数分钟，其间以「本趟已写入 N / M 行」表明仍在推进。
* **同一朵云同一时刻只能有一个同步。** 打开「同步账单」时，若这朵云已有同步在进行（包括 cron 发起的），页面直接接续其进度；点击「开始拉取」遇到 409 时也是如此，而不是只提示「正在同步中」。关闭窗口不会中断任务，重新打开仍能看到进度。
* **可以中途停止，但停在两趟之间。** goscan 每一趟拉取之前都会先清空该账期，中途打断会留下只写了一半的账期，所以它会写完当前这一趟再停止，从按下到真正停止需要数秒至数分钟，其间按钮显示「正在停止…」。停止之后，尚未执行的几趟（如 `2026-09 日度`）会列出来，这些账期的数据保持原样。对已结束的任务再次停止，goscan 返回 409。
* goscan 拒绝触发时的语义原样透传：409 表示「已有同步任务正在执行」，429 表示「已达并发上限」。
* **MCP 中没有同步工具**：`cost_*` 工具与其余工具一样都标记了 `readOnlyHint`，让模型触发一次持续数分钟的云厂商拉取，不在只读承诺之内；补数据由人在页面上操作。
