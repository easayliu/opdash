# opdash

trace、日志、指标和云账单的查询页面。数据来自 [logpipe](../log) 写的 `logs.app_log`、[tracepipe](../trace)
写的 `logs.otel_trace`、[metricpipe](../metric) 写的 `logs.otel_metric`，以及 [goscan](../goscan) 按账期同步
进来的火山引擎 / 阿里云账单，都在同一个 ClickHouse 库里。给内部业务开发排障用：拿一个 trace id 看整条链路
和这条请求的全部日志；按服务 / 接口找慢请求、错请求；按关键字翻日志、看堆栈、看上下文；看某个服务的错误率
和 P95；按标签画指标曲线，从指标上的 exemplar 直接跳到那次请求；再顺带看这个月的云上花了多少钱、花在哪。
单二进制，只读，不需要别的服务。

前三个是「应用自己吐出来的」，goscan 是「去云厂商那儿拉回来的」——所以费用页的时间参数是**账期**
（`2026-09`），不是顶栏那个时间范围。

指标表和账单表都是**可选**的：没部署 metricpipe / goscan 的地方对应的表不存在，那个页签自动不显示，
其余的照常用。

```text
  浏览器 ──▶ /api/*  axum ── 参数化 SQL、readonly=2 ──▶ ClickHouse HTTP（单机或 Distributed）
     ▲          │
     └── /  内嵌 SPA（rust-embed，ui/dist）
  AI 助手 ──▶ /mcp   MCP 工具，在进程内复用同一套 /api/*（见「给 AI 用：MCP」）
```

## 页面

| 路径 | 干什么 |
| --- | --- |
| `/logs` | 日志检索：关键字 / 正则、级别、服务 / namespace / pod 等维度、logger、thread；直方图拖选缩小范围；展开看全文；上下文；跟随（SSE 推送，秒级；可切终端模式正序打印、自动滚到底）；导出 CSV / JSONL |
| `/traces` | 链路检索：服务、接口、span 类型、只看错误、耗时区间、属性 `key=value`；耗时 × 时间散点图 |
| `/traces/:trace_id` | 链路详情：瀑布图、span 属性 / 资源 / 事件（异常堆栈）/ 链接、这条 trace 的日志 |
| `/metrics` | 指标，两个页签：**服务看板**（选一个服务，按 OTel 语义约定自动拼出 HTTP / JVM / 连接池 / Kafka / Go 几套面板；顶上四个数；Top 接口 / 下游 / topic 表**点一行整页按它过滤**；进程重启和新 pod 启动标成虚线；粘性分区目录）和**全部指标**（233 个指标名平铺，自己选算法、分组、过滤；图上的圆点是 exemplar，点开就是那次请求的链路） |
| `/errors` | **错误分组**：把出错的 span 按「同一种报错」归堆——异常类 + 消息 + 哪个接口，一行一种，带次数 / 影响多少条链路 / 最后一次什么时候；展开就是堆栈、样本链路和三个去处 |
| `/services`（首页） | 服务总览：每个服务一张卡——请求量 / 错误率 / P95 各带「和上一个同样长的时间窗比」、一条迷你趋势；错误率 ≥1% / P95 涨 1.5 倍以上的标黄、≥5% / 3 倍标红并排到最前面；可切成表格 |
| `/services/:name` | 单个服务：**和对比时段比，是哪些接口变了**（变化榜 + 每一列都带变化的接口表），请求量与错误、延迟分位趋势（都叠着对比时段） |
| `/cost` | **云账单**（goscan 同步的火山引擎 + 阿里云），两个视图：**账单**看按账期的花费和环比、按天的曲线、按产品 / 计费项 / 地域 / 账号 / 实例 / 项目排行（点一行就加成筛选条件）、明细表（表头可按维度筛选，同一维度可多选）和 CSV 导出；**分析**按归属规则把费用摊到业务线，给出日均、预付费摊销与月度预估（见下面「成本归属」）。两朵云合在一张图上，金额口径可切「应付 / 现金 / 原价」。配了 `--goscan-url` 时右上角还有「同步账单」，可现场补一段账期。**本页按账期查询，顶栏的时间范围在此隐藏** |

### 首页 · 服务总览（照着 Cloudflare 的 Zone Overview 做的）

打开 opdash 第一眼回答的是「现在谁不对」。先总后分、注意力分层：

* **顶上一行全站数字**：全站请求量 / 错误率（各带变化）、异常服务数、进程重启次数、全站请求趋势
  （叠着对比时段的灰影）。
* **异常的放大，正常的压扁**：错误率 ≥ 1% 或 P95 比对比时段高 1.5 倍以上的服务用大卡放在最上面，
  卡上多一行**主因**（「主要是 `GET /x`：P95 80ms → 2.1s」，只对异常服务查一次接口表，挑法和
  详情页的变化榜共用）和**重启标记**
  （`↻ 2 次重启`，hover 看时刻和 pod）；其余 80 多个服务一行一个，名字 · 请求量 · 错误率 · P95 · 小趋势，
  一屏看完全站。hover 任何一行出三个小图标直接跳日志 / 链路 / 指标。
* **对比基线可选，默认昨天同时段**：和「上一小时」比的话，白天永远在涨、晚上永远在跌，早高峰的
  自然爬坡会被当成 +60%。`/api/services?compare=day|week|prev`，对比窗口整体平移，sparkline 的灰影
  也来自它。
* **按比例判 P95 之前要有足够样本**：两个窗口都 ≥ 300 次请求才比（`MIN_SAMPLES`）。低流量服务
  几条慢请求就能把 P95 顶上去，那是统计噪声不是事故——加这条之前首页 6 个「异常」清一色是每小时
  几百到几千次的服务，加了之后剩 1 个，而且是真的（对比时段零错误、现在开始出错）。
* **无意义的 P95 不显示**：入口 span 几乎零耗时（消息确认类，P95 < 1ms）的服务，延迟一列写「—」
  并解释，不要显示成 `4.1µs −5%` 像 bug。

每个服务卡上的三样东西：

* **三个数各带对比**：请求量 / 错误率 / P95，旁边是和对比时段比的变化（`+12%`、`3.1×`、错误从 0
  变成有写「新增」）。`/api/services` 一次返回当前窗和对比窗（`prev`），后端四条查询并发
  （当前、对比窗、两边的迷你趋势），线上 84 个服务一次往返 1 s 左右。
* **迷你趋势**：30 格的请求量小柱图，错误叠在顶上用红，对比时段同一格垫成灰影——形状一比就知道
  是「今天这个时段本来就该这样」还是「今天不一样」；最后一格不完整就不画（同图表那条规则）。
* **健康色**（`ui/src/lib/health.ts`，全站同一套阈值）：错误率 ≥ 5% 红、≥ 1% 黄；P95 ≥ 200ms 且
  是上一周期的 3 倍红、1.5 倍黄；上一周期零错误、这周期开始出错也标黄。坏的排最前，顶上
  「异常 N」一点只看它们。

### 单个服务 · 到底是哪些接口变了

总览页回答「谁不对」，服务详情页要回答的是下一句：**是哪个接口不对**。「这个服务比昨天慢 3 倍」
没法动手，「`POST /order/submit` 的 P95 从 120ms 变成 2.1s，这一小时多耗了 34 秒」才能。所以接口表
当前窗和对比窗**各查一次**（`/api/services/{name}/operations?compare=day|week|prev|none`，两条并发，
都锁定了 `service_name` 走排序键前缀，加一条的代价和第一条差不多），按 `(span_name, span_kind)`
对齐成一行。每个服务封顶 200 个接口（`LIMIT 200 BY service_name`，切在排序之后，留下的是量最大的
那些）：`span_name` 的基数是不可控的——把 SQL 语句、表名拼进 span 名的服务，`kind=client` 下一小时
上千个名字，一次问 24 个服务再乘两个窗口就是几万行 JSON 和同样多组 `quantilesTDigest` 状态。切过的
服务响应里 `truncated=true`，页面标「接口已截断」，「接口没了」那一档也会跳过它（排不进前 200 不等
于接口没了）：

* **变化榜**（`ui/src/lib/compare.ts`）：一屏之内告诉你「变了的是这几个」。**按影响面排，不按百分比排**——
  一小时 20 次的接口从 10ms 变 40ms 是 +300%，会排在「5 万次的接口从 80ms 变 120ms」前面，但用户
  在喊的是后者。所以先按性质分档（接口没了 > 错误变多 > 变慢 > 新出现 > 流量变化），档内各按自己的
  影响面：错误看多出来多少次失败，延迟看多耗的总时间（ΔP95 × 次数），流量看多出来多少次请求。
  阈值和总览页同一套思路（两边各 ≥ 100 次才比 P95，倍数和绝对值都要够），低流量接口的统计噪声不上榜。
  点一行，上面两张图只看这个接口。
* **接口表每一列都带变化**：次数 / 错误 / 错误率 / P50 / P95 / P99 下面一行是和对比时段比的
  `+12%`、`3.1×`。表头默认按值排，切到「按变化排」就按这一列的变化排——找退化的接口时，
  「P95 最高的」和「P95 涨得最多的」常常不是同一批：前者是本来就慢的那几个，后者才是今天新坏的。
* **接口的出现和消失**：只比两边都有的接口会漏掉最硬的两种变化。对比窗有、当前窗一次都没有的
  接口补成 0 次的一行标「没了」，反过来标「新」（多半是刚发的版本改了路由或 span 名）。
* **两张图也叠对比时段**：请求量柱图后面垫一层灰影，延迟图上多一条虚线的「P95 · 昨天同时段」。
  选了某个接口之后，这两条对比线也只属于这个接口——「它是什么时候开始和昨天不一样的」直接看出来。
* **对比基线跟着人走**：总览页选了「和上周同时段比」，点进服务详情看到的还是和上周比
  （`serviceHref` 带 `cmp`，见 `ui/src/lib/links.ts`）。

### 看板里的几个 Cloudflare 套路

* **Top 表 + 整页过滤**：按接口 / 下游地址 / topic 的量画成 Top 表（每行一根占比条）而不是十二色
  堆叠柱；**点一行，整页所有图（连顶上的数字、跳转链接）按它过滤**，顶上出现 chip，× 撤掉。
  过滤条件放在 URL 的 `attr` 里，和「全部指标」共用，切页签不丢。
* **重启 / 发布标记**（`/api/metrics/events`）：两个信号——累积 counter **掉回去**（`cur < prev`，
  只有进程重新起来才会这样，抓的是原地重启）和 **pod 在窗口里第一次出现**（滚动发布是新 pod
  从 0 开始、老 pod 消失，任何一条线都没有下降，只能靠这个抓）。用 `jvm.cpu.time` 这类没标签的
  counter 最干净，按 `(service, k8s.pod.name)` 分区。标成虚线竖线，顶上一行列出时刻和 pod。
  线上验过：一个服务 24 小时里两次重启，P99 在第二条虚线前冲到 10 秒——先劣化再被重启，一眼对上。
* **粘性分区目录**：HTTP 服务端 / 客户端 / JVM / 连接池 / Kafka 一条横向目录粘在顶上，滚到哪一节
  高亮，点了直接滚过去。

### 四个页面互相怎么跳

三个信号加服务概览，两两之间都能跳，**跳过去看到的是同一个服务、同一段时间**（地址拼装都在
`ui/src/lib/links.ts`，散在各页手写迟早有一处忘了带 `from` / `to`）：

| 从 | 到 | 入口 |
| --- | --- | --- |
| 日志行 | 链路详情 | 行尾的 trace id；16 位的 span id 跳「这个 span 的全部日志」 |
| 日志页（筛了服务） | 指标看板 / 最慢的链路 / **错误分组** / 出错的链路 / 服务概览 | 筛选栏下面那一排 |
| 链路列表（筛了服务） | 指标看板 / 错误日志 / **错误分组** / 服务概览 | 筛选栏下面那一排 |
| 链路详情 | 服务指标 / 服务概览 / 这条链路的全部日志 | 顶部；指标的时间窗以这条 trace 为中心前后各 15 分钟 |
| 链路详情 · 某个 span | 这个服务的指标 / 只看这个 span 的日志 | 右侧 span 面板 |
| 指标看板 | 最慢的链路 / **错误分组** / 出错的链路 / 错误日志 / 服务概览 | 顶部；图上拖一段之后带的就是拖出来的窗口 |
| **指标图上的一个点** | **这一格的链路 / ≥ 这个值的链路 / 这一格的报错 / 这一格的错误日志** | 点图上任意一点，见下面「点选下钻」 |
| 指标图上的 exemplar | 那一次请求的链路详情 | 图上的圆点 |
| 服务概览 | 三个信号 | 顶部 |
| **首页异常卡** | **这个服务在报的那句错** | 卡上「⚠ 异常类: 消息 N 次」那一行，点了直接展开对应的错误分组 |
| 错误分组的一行 | 样本链路（落地就选中报错的 span）/ 这个接口的全部错误链路 / 这个服务的错误日志 | 展开之后 |
| 服务详情 | 错误分组 | 顶部「错误分组」，在「出错的链路」前面；选中了某个接口就只看它的错（顶上一个可以摘掉的 chip） |

**怎么回去**：链路详情是四条路的共同终点（错误分组、链路检索、日志行的 trace id、指标图上的
exemplar），所以返回目标不能写死。跳过去时用 react-router 的 `state` 带上来处，详情页左上角就是
「← 错误」/「← 日志」——**返回目标不进 URL**：它是这一次导航的上下文，不是视图状态，放进 URL 会
跟着被复制给同事，别人点了会跳到一个他从没去过的列表。这也刚好给出正确的显示条件：直接粘 URL
进来的没有来处，就不显示返回，总比给一个猜出来的目标强。

另一条规矩：**展开、选中这类瞬态状态改 URL 时一律 `replace`，只有「换了在看的东西」才 `push`**。
错误分组的展开以前是 push 的，扫一遍列表展开五组，back 就要按五次才出得去。

**点选下钻**：光带服务和时间等于到了新页面还得自己再筛一遍，而面板本来就知道更多。点图上一个点，
弹层里的链接会把三样东西一起带过去：

* **这一格的时间窗**（那一分钟，不是整个页面的时间范围）；
* **点中那条线的标签**，按语义约定翻译成目标页认得的筛选——`http.route` / `http.response.status_code` /
  `server.address` 这些在 span 上是同名属性，直接变成链路页的 `attr=k=v`；`span.name` 变 `span_name`；
  spanmetrics 的 `status.code=STATUS_CODE_ERROR` 变「只看错误」；`res:k8s.pod.name` 变日志页的 `pod`
  维度筛选（对照表在 `ui/src/lib/links.ts`）；
* **那个点的值**：延迟类指标（单位是 `s` / `ms` / `us` / `ns`）多给一条「≥ 2.27s 的链路」，
  也就是 `min_ms`——从「P95 在这一分钟是 2.27 秒」直接跳到「这一分钟里比 2.27 秒还慢的那些请求」。

**弹层打开时会顺手查一次这一格的报错**（锁定服务的一分钟窗口，线上 49 ms / 0.2 MB），有就多一条
「这一格的报错 · N 种」（hover 看前三种是什么），没有就直说「这一格没有报错——尖的是耗时不是错误」。
**这句话本身就是答案**：告诉你这个尖峰是慢不是错，不用再去翻错误页。

之所以要查、而不是无脑给一条链接过去：错误太稀，给了多半是个空页面。线上量过，对**有过错误的**
服务，随便点中一分钟能命中错误的概率只有 10.9%。也不能靠放宽窗口蒙混——前后各放宽 30 分钟也
只到 39.8%（错误本来就稀且成簇），却会毁掉这个弹层「只看这一格」的承诺，其余几条链接都是精确到
那一分钟的。

从「一个时刻」（一条日志、一个 span、一个 exemplar）跳到按时间段看的页面时，前后各放宽
15 分钟（`WINDOW_AROUND_MS`）——只给那一毫秒的话指标图上一个点都没有，放宽了才看得出尖峰
是从什么时候开始的。按 trace id / span id 查日志则**不带**时间范围，理由见下面「查询是怎么写的」。

顶栏：时间范围（相对 / 绝对）、直达框（粘一个 trace id 直接开链路，16 位 hex 当 span id，其它当关键字搜日志）、
书签（收藏当前查询，见下面「收藏查询」）、深浅色。**所有筛选条件都在 URL 里**，链接复制给同事就是同一个视图；
相对范围（`range=1h`）打开时按当时的时间算，绝对范围（`from=&to=`）永远是那一段。

### 收藏查询

常用的那几条——「order 服务最近 24 小时的 ERROR」「payment 最慢的入口链路」——设好条件之后点顶栏的书签
图标收藏，起个名字（默认按条件拼一句），下次在任何一页打开书签点一下就到。能收藏的是日志 / 链路 / 错误 /
指标 / 服务总览 / 服务详情这些**列表和看板页**；链路详情是一条具体的 trace，30 天后就没了，不算查询。

收藏的就是页面地址（路径 + 查询串）：页面状态本来全在 URL 里，不用给每种筛选另写一套序列化，新加一个筛选
参数也自动能收藏。存的时候去掉翻页位置和**时间范围**（`range` / `from` / `to`）：收藏的是查询条件，时间范围
跟顶栏走——顶栏的范围本来就是跨页面共享、跟着人走的，打开一条收藏用的就是顶栏当前的范围。点的正是当前
这条时地址不会变，按「刷新」处理，重新查一遍。

**按账号区分**：收藏归在登录账号名下（OIDC 的 `preferred_username` / Basic 的用户名，和 API key 同一套），
列表只列本人的，换台机器登录还在；拿本人的 API key 也能读写（key 代表这个人）。没开认证的部署没有「用户」，
所有人共用一份。同一个人同一个地址只存一条（再收藏是 409），每人最多 200 条。存在 `--saved-query-file`
（一个几 KB 的 JSON），为什么是文件不是 ClickHouse、多副本怎么办和 API key 文件一样，见下面「API key」。

```text
GET    /api/saved        我的收藏，新的在前
POST   /api/saved        {"name": "订单超时", "path": "/logs", "query": "q=timeout&level=ERROR"}；name 可省
PUT    /api/saved/{id}   改名 {"name"}，或换地址 {"path", "query"} 一起给
DELETE /api/saved/{id}   删掉；别人的和不存在的一样是 404
```

按 trace id / span id 查日志**一样要带时间范围**（2026-09-21 改，之前是反过来的）。日志表上
**`span_id` 没有任何索引**，`trace_id` 的 `idx_trace_id` 是 `bloom_filter`、默认误判率 2.5%，摊到 30 天的
分区上也只剪掉九成七——两条路不带时间范围都是几十 GB 的全表扫描。关掉 query condition cache 实测：

| 查询 | 读行数 | 读量 | 耗时 |
| --- | --- | --- | --- |
| `span_id=…`，不带时间范围 | 313.5 亿 | 37.9 GiB | 8.8 s |
| `span_id=…` + 1 小时窗口 | 830 万 | 158 MB | 0.18 s |
| `trace_id=…`，不带时间范围 | 10.4 亿 | 5.4 GB | 14 s |
| `trace_id=…` + 样本时刻前后 1 小时 | 21 万 | 15.5 MB | 0.16 s |

所以拼链接的地方（`ui/src/lib/links.ts` 的 `logsHref`）一律把时间窗带上：手上有确定时刻的传
`around(ts)`（错误分组的 `last_ms` 和 `sample_trace` 来自同一个 `argMax`，链路详情有 trace 自己的跨度，
日志行有 `ts_ms`），只有一个 id 的传页面当前范围。日志页按 id 查也照页面当前范围裁，**相对范围
（`range=1h`）也算数**——以前只认字面写着的 `from` / `to`，于是 `logs?span_id=…&range=1h` 每打开一次
就是一次 38 GB 的扫描。窗口没套住时空状态上有「不限时间再找一次」兜底，那一下才是上表第一行的代价。

时间范围无论在哪个排序键下都走主键的通用排除搜索，只读范围内的 granule（线上现在还是
`(timestamp, level, trace_id)`，见下面「排序键」一节）。

## 启动

```bash
# 前端
cd ui && pnpm install && pnpm build && cd ..
# 后端（前端产物会编进二进制）
cargo run --release -- --clickhouse-url http://127.0.0.1:8123 --clickhouse-user default
# 打开 http://127.0.0.1:4880
```

没构建前端也能 `cargo build`（`ui/dist/.gitkeep` 占位），只是首页会提示去构建；API 不受影响。

前端开发：`cd ui && pnpm dev`，vite 把 `/api` 代理到本地 4880 的后端。

### 配置

所有参数都有对应的 `OPDASH_*` 环境变量，容器里直接用 env：

| 参数 | 环境变量 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--bind` | `OPDASH_BIND` | `0.0.0.0:4880` | 监听地址 |
| `--clickhouse-url` | `OPDASH_CLICKHOUSE_URL` | `http://127.0.0.1:8123` | ClickHouse HTTP 地址 |
| `--clickhouse-user` / `--clickhouse-password` | `OPDASH_CLICKHOUSE_USER` / `OPDASH_CLICKHOUSE_PASSWORD` | `default` / 空 | 建议给 opdash 建一个只读账号，profile 里钉住 `readonly=2`、`max_execution_time` |
| `--database` / `--log-table` / `--trace-table` | `OPDASH_DATABASE` / `OPDASH_LOG_TABLE` / `OPDASH_TRACE_TABLE` | `logs` / `app_log` / `otel_trace` | 和采集端 sink 配置一致；集群上填 Distributed 表名 |
| `--metric-table` | `OPDASH_METRIC_TABLE` | `otel_metric` | metricpipe 的表。**可以不存在**——那样指标页不显示，启动日志里说一句原因 |
| `--volcengine-bill-table` / `--alicloud-monthly-table` / `--alicloud-daily-table` | `OPDASH_VOLCENGINE_BILL_TABLE` / `OPDASH_ALICLOUD_MONTHLY_TABLE` / `OPDASH_ALICLOUD_DAILY_TABLE` | `volcengine_bill` / `alicloud_bill_monthly` / `alicloud_bill_daily` | goscan 的三张账单表，**各自可以不存在**（只接了一朵云是常态），一张都没有就不显示费用页。**一般不用配**：表名按「基础名 → `<名字>_distributed` →（火山那张还找）`volcengine_bill_details` → `volcengine_bill_details_distributed`」依次找，goscan 两轮改名建出来的表都认得 |
| `--goscan-url` | `OPDASH_GOSCAN_URL` | 不配 | goscan 的地址（`http://goscan.logging.svc.cluster.local:8080`）。配了费用页上才有「同步账单」按钮，见下面「手动同步账单」。**这是 opdash 唯一一处会向外发出「改变状态」请求的功能**，对 ClickHouse 依旧只读 |
| `--goscan-timeout` | `OPDASH_GOSCAN_TIMEOUT` | `10s` | 调 goscan 接口的超时。触发同步只是登记一个后台任务，很快返回；真正的拉取在 goscan 侧进行，与此超时无关 |
| `--bill-dedupe` | `OPDASH_BILL_DEDUPE` | `group` | 账单查询怎么去重：`group` 按建表排序键分组（默认，跨分片也对）、`final` 给表加 `FINAL`、`off` 不去重。见下面「账单表：为什么要在查询里再去重一次」 |
| `--bill-alloc` | `OPDASH_BILL_ALLOC` | 不配 | 成本归属规则文件（TOML），费用页的「分析」视图据此把费用摊到业务线、把预付费按服务期摊到各月。不配也能用，只是少了业务线这一层。示例与写法见 `examples/bill-alloc.toml` 和下面「成本归属」。**文件有误时进程直接退出**，不会静默降级 |
| `--timezone` | `OPDASH_TIMEZONE` | `Asia/Shanghai` | 直方图分桶对齐的时区，和两张表 `timestamp` 列的时区一致 |
| `--query-timeout` | `OPDASH_QUERY_TIMEOUT` | `30s` | 传给 ClickHouse 的 `max_execution_time` |
| `--max-range` | `OPDASH_MAX_RANGE` | `31d` | 允许查询的最大时间跨度 |
| `--max-rows` / `--max-offset` | `OPDASH_MAX_ROWS` / `OPDASH_MAX_OFFSET` | `1000` / `10000` | 日志一页最多几行；最多翻到第几条 |
| `--export-max-rows` | `OPDASH_EXPORT_MAX_ROWS` | `50000` | 导出上限 |
| `--max-message-chars` | `OPDASH_MAX_MESSAGE_CHARS` | `16384` | 列表 / 上下文 / 跟随里每条日志的 `message` 最多取多少字符，**导出不受限**。见下面「一条日志能有多大」 |
| `--max-trace-spans` | `OPDASH_MAX_TRACE_SPANS` | `5000` | 一条 trace 最多取多少 span，超过标记截断 |
| `--max-read-bytes` / `--max-read-rows` | `OPDASH_MAX_READ_BYTES` / `OPDASH_MAX_READ_ROWS` | `0`（不限） | 单条查询的读量护栏（`max_bytes_to_read` / `max_rows_to_read`），超过立刻报错让用户缩小范围，比等超时体验好。**按整条查询的总读量算**（发起端汇总各分片的进度一起检查），不是按分片；要按分片得用 `max_bytes_to_read_leaf` |
| `--max-concurrent-queries` | `OPDASH_MAX_CONCURRENT_QUERIES` | `16` | 同时最多几条查询在库上跑，排队 10 秒没名额回 503 |
| `--schema-refresh` | `OPDASH_SCHEMA_REFRESH` | `5m` | 多久重读一次 `system.columns` |
| `--tail-interval` | `OPDASH_TAIL_INTERVAL` | `1s` | 日志跟随时服务端多久查一次增量；每条跟随连接就是这个频率的一条轻量查询 |
| `--max-tail-streams` | `OPDASH_MAX_TAIL_STREAMS` | `8` | 同时最多几条跟随连接，满了回 503 |
| `--basic-auth` | `OPDASH_BASIC_AUTH` | 不认证 | `user:password`，配了就要求浏览器登录；`/api/health` 不认证。可和 OIDC 同时开，给脚本 / curl 用 |
| `--oidc-issuer` / `--oidc-client-id` | `OPDASH_OIDC_ISSUER` / `OPDASH_OIDC_CLIENT_ID` | 不认证 | Keycloak realm 地址（`https://sso.example.com/realms/ops`）和 client ID，两个一起配就走浏览器跳 Keycloak 登录，见下面「登录」 |
| `--oidc-client-secret` | `OPDASH_OIDC_CLIENT_SECRET` | 无 | client 密钥；public client 不用配（有 PKCE） |
| `--oidc-required-role` | `OPDASH_OIDC_REQUIRED_ROLE` | 无 | 要求用户带这个角色（realm 角色或本 client 的角色）才放行；不配 = 登录了就行 |
| `--oidc-scopes` | `OPDASH_OIDC_SCOPES` | `openid profile email` | 授权请求的 scope |
| `--public-url` | `OPDASH_PUBLIC_URL` | 按请求头推 | 浏览器访问 opdash 的地址，拼 OIDC 回调用；在 Ingress 后面按 `X-Forwarded-Proto` / `Host` 推一般是对的，本地 vite 开发配 `http://localhost:5173` |
| `--session-ttl` | `OPDASH_SESSION_TTL` | `12h` | 登录会话多久失效，到期重新跳一次 Keycloak |
| `--session-secret` | `OPDASH_SESSION_SECRET` | 随机 | 会话 cookie 的签名密钥。不配则每次启动随机生成（重启后要重新登录）；多副本必须配同一个 |
| `--api-key-file` | `OPDASH_API_KEY_FILE` | `api-keys.json` | 用户自己生成的 API key（给 MCP / 脚本用）存在哪个文件，只存哈希。**容器里把所在目录挂成卷**（镜像的工作目录是 `/var/lib/opdash`），不然重启就没了；多副本共享同一个文件 |
| `--api-key-ttl` | `OPDASH_API_KEY_TTL` | `90d` | API key 最长有效多久，生成时可以选更短的 |
| `--saved-query-file` | `OPDASH_SAVED_QUERY_FILE` | `saved-queries.json` | 用户收藏的查询存在哪个文件（按账号区分），和 API key 文件一样挂成卷、多副本共享，见上面「收藏查询」 |

`RUST_LOG=opdash=debug` 能看到每条 SQL 和绑定的参数。

### 登录：对接 Keycloak

这个页面能翻全部线上日志，对外暴露一定要认证。两种方式：`--basic-auth` 一组共享密码（内网、临时够用），
或者 OIDC 跳 Keycloak 登录（推荐，谁登录过日志里有名字，离职回收账号即可）。两个可以同时开，Basic 留给
脚本 / curl。

Keycloak 里建 client（realm 随意，下面以 `ops` 为例）：

1. Clients → Create client：类型 OpenID Connect，Client ID 填 `opdash`。
2. Capability config：Client authentication **On**（拿到 client secret；不开也行，opdash 带 PKCE），
   Standard flow 勾上，其它 flow 都不用。
3. Login settings：Valid redirect URIs 填 `https://opdash.example.com/api/auth/callback`；
   Valid post logout redirect URIs 填 `https://opdash.example.com/*`（退出后跳回来用）。
4. 可选：要限制只有某些人能看，建一个 realm 角色（比如 `opdash-viewer`）分给相应的人 / 组，
   opdash 配 `--oidc-required-role opdash-viewer`。client 角色也认（在 client 的 Roles 里建，
   token 里是 `resource_access.opdash.roles`）。角色 opdash 会同时从 id_token 和 access_token 里找，
   不用改 Keycloak 默认的映射器。

然后给 opdash 配：

```bash
OPDASH_OIDC_ISSUER=https://sso.example.com/realms/ops   # 和 Keycloak 签在 token 里的 iss 一字不差
OPDASH_OIDC_CLIENT_ID=opdash
OPDASH_OIDC_CLIENT_SECRET=xxxx                           # Credentials 页签里的 Client secret
OPDASH_OIDC_REQUIRED_ROLE=opdash-viewer                  # 可选
OPDASH_SESSION_SECRET=$(openssl rand -hex 32)            # 可选；多副本必须配
```

流程是标准的授权码 + PKCE，全部在后端完成：浏览器打开任何页面 → 没会话就 302 到 Keycloak → 登录后回
`/api/auth/callback` → opdash 用 code 换 token，校验 id_token 的 `iss` / `aud` / `exp` / `nonce`，把
用户名和邮箱签进一个 HttpOnly cookie（HMAC，服务端不存会话）→ 跳回原来要看的页面。id_token 不验签：
它是 opdash 自己走 TLS 直连 token 端点拿的，链路已经证明了签发方（OIDC Core 3.1.3.7 允许这么做），
所以 **issuer 必须是 https**。顶栏右侧显示用户名，退出会顺带结束 Keycloak 那边的 SSO 会话。

页面上显示的名字优先取 `name` claim —— Keycloak 里填了 First / Last name 的话它就是**中文姓名**，
比 `preferred_username`（登录用的拼音账号）好认；没有 `name` 就拿 `given_name` + `family_name`
自己拼，再没有才退回账号名、邮箱。Keycloak 是用空格把两栏拼成 `name` 的（中文习惯是 First name
填姓、Last name 填名，拼出来中间就多一个空格），**全是汉字时那个空格会去掉**，英文名
（`Jane Doe`）保持原样。要让这几个 claim 进 id_token，client 的 scope 留着 `profile` 就行（默认就有）；
**两栏都没填的用户仍然显示账号名**。

**认人的始终是账号名**（`preferred_username`）：API key 归在它名下、日志里记的也是它，所以
IdP 那边改个显示名，谁的 key 都不会突然「不见了」。名字是登录那一刻写进会话 cookie 的，
改完要**退出重新登录**（或等 `--session-ttl` 到期）才会变。

```text
GET  /api/auth/me        登录方式和当前用户；没登录也 200（前端据此跳登录）
GET  /api/auth/login     ?next=/logs   生成登录票，跳 Keycloak
GET  /api/auth/callback  Keycloak 跳回来的地址
GET  /api/auth/logout    清会话，跳 Keycloak 登出再回首页
POST /api/auth/keys      登录用户给自己签一把 API key，见下面「API key」
```

### API key：把登录「拿出来」给 MCP 客户端和脚本

Claude Code 这类 MCP 客户端和 curl 不会跳浏览器登录，所以登录用户可以在页面右上角的钥匙图标里
**给自己生成 API key、看自己有哪些、随时吊销**。之后带 `Authorization: Bearer opdash_…` 访问任何接口，
包括 `/mcp`。key 代表签发它的这个人，权限和他登录后一样（本来也只有「能看」一种权限），
`/api/auth/me` 会告诉你这个请求是 `session` / `basic` / `api_key` 哪种身份进来的，日志里记着
key 的 id 和名字（不记 key 本身）。

```text
GET    /api/auth/keys        我的 key：名字、前缀、创建 / 到期 / 最近使用时间；没有 key 本身
POST   /api/auth/keys        签一把，body 可选 {"name": "claude-code", "ttl": "30d"}；key 只在响应里给这一次
DELETE /api/auth/keys/{id}   吊销，立刻失效
```

key 长这样：`opdash_<12 位 hex id>.<32 位随机串>`。**服务端只存随机串的 SHA-256**（`--api-key-file`，
一个几 KB 的 JSON），认证时按 id 找到那一行、常量时间比哈希、看有没有过期；文件泄露了也签不出 key，
页面上列出来的也只有前半段。`last_used_at` 每分钟落一次盘，不会让每个 MCP 请求都写磁盘。

为什么是文件不是 ClickHouse：opdash 对库是只读的（每条查询 `readonly=2`，推荐给它只读账号），
为了一张几行的 key 表加一条写库路径、再处理集群上的建表，不值。文件用「写临时文件再 rename」，
掉电不会留半个文件；**容器里要把它所在目录挂成卷**，镜像的工作目录是 `/var/lib/opdash`，
`docker-compose.yml` 已经挂了一个 named volume，k8s 给它一个几 MB 的 PVC 就行。多副本共享一个卷
也可以：每次用到都先看文件的 mtime，别的副本改了就重读。

几条规矩：

* **只能管自己的**：列表只列本人的，吊销别人的和吊销不存在的一样是 404，不给探测 id 的机会。
* **key 不能管 key**：拿 API key 调这三个接口都是 403，一把泄露的 key 不能给自己续命、也不能删别的。
* **有效期**：默认最长 90 天（`--api-key-ttl`），页面上可选 7 / 30 / 90 天；过期的还会在列表里显示
  7 天（标「已过期」），然后从文件里清掉。
* 不用审批：谁登录了谁就能给自己签。要限制谁能进 opdash，用 `--oidc-required-role`。
* 没开认证的部署没有 key 这回事（什么都不用带），按钮不显示，接口回 400 说明原因。

## 给 AI 用：MCP

同一个二进制还开着一个 [MCP](https://modelcontextprotocol.io)（Model Context Protocol）端点 `POST /mcp`，
Claude Code / Codex / Cursor 这类 AI 助手接上之后，「昨天下午 order 服务为什么慢」这种问题它自己
会去查：先看服务总览谁不对，再看是哪个接口，拉错误分组，拿样本链路看瀑布图和异常堆栈，翻对应的日志。
人只用问问题。

先在页面右上角的钥匙图标里给自己生成一把 API key（见上面「API key」），页面会把接入命令拼好：

```bash
# Claude Code：Streamable HTTP 传输，API key 放在请求头里
# add 碰上同名的直接报 already exists，所以前面带一条 remove：没装过它只在 stderr 说句找不到，
# 不挡后面那条；装过就是换成新 key
claude mcp remove opdash 2>/dev/null
claude mcp add --transport http opdash https://opdash.example.com/mcp \
  --header "Authorization: Bearer opdash_…"

# 试一下握手（不需要客户端）
curl -s -H "Authorization: Bearer opdash_…" https://opdash.example.com/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
```

Codex 不用命令行加远程服务，写进 `~/.codex/config.toml`（项目级是 `.codex/config.toml`）；
已经有 `[mcp_servers.opdash]` 的把整段换掉：

```toml
[mcp_servers.opdash]
url = "https://opdash.example.com/mcp"
http_headers = { Authorization = "Bearer opdash_…" }
# 不想把 key 写进配置文件（比如它会进版本库）就改成从环境变量取：
# bearer_token_env_var = "OPDASH_API_KEY"
```

别的客户端（Cursor、Claude Desktop、自己写的）填地址 `https://opdash.example.com/mcp`、请求头
`Authorization: Bearer <key>` 就行——服务端是标准的 Streamable HTTP，没有任何客户端专属的东西。
换了 key（吊销重签、到期重签）就是把这一条里的 key 改掉，地址和其它都不用动。
只支持 stdio 的老客户端用 `npx mcp-remote https://opdash.example.com/mcp --header "Authorization: Bearer opdash_…"` 桥一下。

### 有哪些工具

| 工具 | 干什么 | 对应的页面 / 接口 |
| --- | --- | --- |
| `get_meta` | 版本、时区、当前时间、三张表各有哪些可筛的维度列、指标表是否启用 | `/api/meta` |
| `list_services` | 时间范围内有 span 的服务名 | `/api/traces/values` |
| `service_overview` | 每个服务的请求量 / 错误率 / P95 和对比时段的变化，`health` 字段按首页同一套阈值给出 red / yellow / ok；坏的排前面 | 首页 |
| `service_operations` | 一个服务的接口表，每列带对比时段的变化，`gone` / `new` 标出消失和新出现的接口；可按 P95 涨幅、错误增量排 | `/services/:name` |
| `service_timeseries` | 请求量 / 错误 / P50 / P95 / P99 随时间的曲线，叠对比时段 | `/services/:name` |
| `error_groups` | 出错的 span 按「同一种报错」归堆，带样本链路 | `/errors` |
| `search_traces` | 按服务 / 接口 / span 类型 / 只看错误 / 耗时区间 / 属性筛链路 | `/traces` |
| `get_trace` | 一条链路的全部 span，带层级 `depth`、相对开始的 `offset_ms`；太多时保留全部出错的和最慢的；`include_logs` 顺带取日志 | `/traces/:trace_id` |
| `get_span` | 一个 span 的属性 / resource / events（异常堆栈）/ links | 详情页右侧面板 |
| `search_logs` | 关键字 / 正则 / 级别 / 维度列 / trace_id / span_id 检索日志（默认不数总数，要总数用 `log_histogram` 或 `count=true`） | `/logs` |
| `log_histogram` | 日志条数按时间、按级别的分布——「错误从几点开始的」 | 日志页直方图 |
| `log_facets` | 几个维度列各自最常见的取值——「报错集中在哪个 pod」 | 日志页下拉框 |
| `log_context` | 某条日志前后几行 | 日志页上下文 |
| `list_attrs` | span / 指标上有哪些属性名、某个属性名有哪些取值——`attr` / `by` 该写什么，列一遍而不是猜 | 链路页 / 指标页的属性下拉 |
| `list_metrics` / `query_metric` | 指标目录；按 agg / field / by / 过滤查一个指标的时间序列。不给 agg / field 时按指标类型自动挑 | `/metrics` 全部指标 |
| `metric_exemplars` | 指标点上挂的 trace id：P99 尖峰直接换成一条链路，拿去 `get_trace` | 图上点一个点 |
| `metric_events` | 进程重启 / pod 新启动的时刻 | 看板上的虚线 |
| `cost_summary` | 云账单按账期（或按天）的花费，分云给——「这几个月花了多少」「这个月比上个月涨了吗」 | `/cost` 顶上的图 |
| `cost_breakdown` | 按产品 / 地域 / 账号 / 实例排行，两朵云合在一起排——「多出来的钱是哪个产品」 | `/cost` 排行 |
| `cost_detail` | 账单明细，一行一个计费项 | `/cost` 明细表 |
| `cost_allocation` | 按归属规则（`--bill-alloc`）把账单摊到业务线：各线金额、日均、月度预估、产品构成与未归属金额；未配规则时给出按产品的日均与预估——「哪条业务线最贵」「下个月大约花多少」 | `/cost` 分析视图 |
| `cost_compare` | 按产品比较两段等长日期的花费（与前一日、与上周同日、近 N 天与前 N 天），按变化额排序——「昨天为什么贵了」 | `/cost` 产品费用对比 |

工具都标了 `readOnlyHint`（全是只读查询），客户端据此可以免掉每次调用的确认。没配指标表的部署
不列指标那几个工具，没接 goscan 的不列 `cost_*`——列出来模型也只会换回一句「未启用」，白占上下文。
费用工具的时间参数是**账期**（`from` / `to` 写 `2026-09`，或者用 `months` 说「最近几个月」），
和别的工具那套 `from` / `to` / `range` 不一样。`cost_allocation` 默认只统计当前一个账期；跨账期且未指定
`days` 时改读阿里云的月度账单（行数约为日度账单的几十分之一），此时不提供日均与预估。`cost_compare`
只向前读取比较所需的天数（「与前一日比」读 9 天），而非页面固定的 62 天。单次工具结果超过 64 KB 会先砍列表、再截长文本，并在 `notes` 里说清楚砍了
什么——模型的上下文不该被一次 `search_logs` 灌满。参数名写错（`service` 写成 `service_name`）会
直接报「不认识的参数」并列出认识的，不会被静默忽略后拿一份全站的数当答案。

工具不直接碰查询层：每个工具把参数翻译成 `/api/*` 的查询串，在进程内走一遍同一个 axum Router，
拿到 JSON 再整理成给模型看的形状。所以参数校验、错误提示、读量护栏和页面是同一套——模型传了
不存在的列名，看到的是和页面一样的那句「不认识的筛选列 x；可用的筛选列: …」，照着改就行。
整理只做减法：时间戳一律转成 `--timezone` 的本地时间（模型不用自己算毫秒），空字段省掉，
`stats` / sparkline / 空桶这类页面装饰不给，长 message 和堆栈按参数截断并注明原长，
大列表的默认上限比页面小（日志 50 行、链路 20 条、错误 30 组）。

时间参数宽松：`from` / `to` / `at` 认 RFC3339、不带时区的本地时间（按 `--timezone`）、unix 秒或毫秒、
`now-30m` 这类相对写法；`range` 是跨度（`15m` / `1h` / `24h`），不给 `from` 时 `from = to - range`。
`initialize` 的 `instructions` 里写了排障套路、这些写法、当前时间和表结构里实际可筛的列名，
模型接上就知道该怎么用。

### 传输和认证

* **Streamable HTTP、无状态**：一次 POST 一个 JSON-RPC 请求（旧协议的批量数组也认），
  回一个 JSON；不发 `Mcp-Session-Id`，`GET /mcp`（服务端推送流）和 `DELETE /mcp`（结束会话）回 405。
  没有会话就没有要清理的东西，多副本部署也不用粘连接。
* **认证和页面同一套**：`/mcp` 在认证中间件里面，认会话 cookie、Basic 和 API key 三种身份。
  MCP 客户端不会跳浏览器登录，所以走 API key：每个人用自己的 key，日志里能看出是谁在查；
  不要把 `--basic-auth` 的共享密码发给大家。没开认证的部署 `/mcp` 也不认证。
* **Origin 校验**：请求带了 `Origin` 头就要和 `Host` 是同一个主机，否则 403（协议要求的防
  DNS rebinding）。命令行客户端不带 Origin，不受影响。
* 工具调用的失败（参数不对、查询超时、读量超限）作为**工具结果**回去（`isError: true`），
  模型看得到原因、能自己改参数重试；只有「没有这个工具」「方法不存在」这类协议层的问题才是
  JSON-RPC 错误。

## 表结构：程序知道什么、不知道什么

固定列是程序写死的（logpipe 的 9 列、tracepipe 的 22 列、metricpipe 的 27 列），启动时读
`system.columns` 校验，日志表 / span 表缺列直接在 `/api/health` 里报出来。**固定列之外的字符串列自动变成筛选维度**：k8s 元数据（`service_name` /
`namespace` / `pod` / `container` / `stream`）、采集端配置里 `fields` 加的静态列（`cluster` / `env` ……）
都不用改 opdash，`/api/meta` 的 `dimensions` 里有什么页面就显示什么筛选项。新加了列过 5 分钟自动认到。

span 表的四个属性列必须是 ClickHouse 的 `JSON` 类型（tracepipe v0.2.0 起，ClickHouse 25.3+）。
v0.1 的 `Map` 表不兼容，`/api/health` 会点名哪一列是 Map，按 tracepipe README 重建即可。

指标表和这两张不一样，**它缺了不算错**：表不存在、缺 metricpipe 的固定列、或者属性列不是 JSON，
都只是让 `/api/meta` 的 `metrics` 变成 `null`（`metrics_note` 里是原因，启动日志里也有一句），
指标页签不显示，日志和链路一切照旧。硬要求三张表齐全的话，一个还没上指标的环境连日志都打不开了。

### 账单表：为什么要在查询里再去重一次

goscan 的三张账单表（`volcengine_bill` / `alicloud_bill_monthly` / `alicloud_bill_daily`）和另外三张不同：
它们是 `ReplacingMergeTree`，靠「同一个账期重复拉不会翻倍」来保证幂等。**在 2026-09 的表结构调整之前，
这个保证在集群上并不成立**——下面先说清楚为什么，再说改了什么。

2026-09-22 查线上（`logs` 库、`log` 集群 3 分片）确认的事实：

```sql
SELECT name, engine_full FROM system.tables WHERE database='logs' AND name LIKE '%bill%';
-- alicloud_bill_daily_distributed   Distributed('log', 'logs', 'alicloud_bill_daily_local', rand())
-- volcengine_bill_details_local     ReplacingMergeTree
--     ORDER BY (BillPeriod, ExpenseDate, InstanceNo, ExpenseBeginTime, Product, ElementCode, PayableAmount)
```

分片键是 **`rand()`**：同一行重复写一次，两份会落到**不同分片**上。而 `ReplacingMergeTree` 的去重只发生在
分片内的 merge 里，`FINAL` 同理，goscan 同步完跑的那句 `OPTIMIZE TABLE ... ON CLUSTER FINAL` 也一样——
跨分片的那一份无论哪种方式都无法收敛。goscan 的日调度每天会把当月重拉一遍（`sync_mode` 无论取 `standard` 还是 `sync-optimal`，都不是
「先删后插」），一个月下来同一行最多可能存在三份，账单金额随之翻倍。

所以 opdash 默认（`--bill-dedupe=group`）在查询里再去重一次。**但去重键本身并不唯一**（2026-09-23 对账时发现）：
阿里云会把一笔尾差调整单独出成一行，维度与正常账单一模一样，只有金额不同；同一台机器同月先包月、再转包年，
也会出成两行同键的账单：

```text
billing_date  product    instance_id               pretax_amount  pretax_gross_amount
2026-06-01    短信服务    <账号>:<短信签名>         409.41               409.41
2026-06-01    短信服务    <账号>:<短信签名>         -0.005                    0
```

早先按排序键分组、其余列一律 `any()`，就会在这两行之间随手挑一个——2026-08 的百炼因此少算了 34,837.57，
阿里云 8 月按量合计少算 3,677.03。现在的去重分两步：

```sql
SELECT _period, _amount FROM (
  SELECT any(billing_cycle) AS _period, any(toFloat64(pretax_amount)) AS _amount,
         max(updated_at) AS __version,
         max(max(updated_at)) OVER (PARTITION BY <排序键>) AS __latest
  FROM logs.alicloud_bill_monthly
  WHERE ...
  GROUP BY <排序键>, pretax_amount, payment_amount, pretax_gross_amount   -- ① 金额也进分组
)
WHERE __version >= __latest - INTERVAL 60 SECOND                           -- ② 只留最近一次同步
```

1. **金额也进分组**：完全相同的副本（重拉留下的、写入重试留下的）合为一行；金额不同的并列行各自保留，之后相加。
2. **按排序键只留最近一次同步写入的那几行**：金额进了分组，重拉之前的旧版本（云厂商月中调过金额）就不会再与
   新版本合一，因此按 `updated_at`（也是 `ReplacingMergeTree` 的版本列）把它们筛掉。同一次同步里并列的几行
   相隔不过数秒（线上 2026-06 至 08 月最多 2.2 秒），同一账期的两次同步则至少相隔数分钟，60 秒的窗口把两者分得很开。

这正是 `ReplacingMergeTree(updated_at)` 合并之后该剩下的样子，只是引擎会把并列行也吞掉，这里不会。
按这套去重，opdash 对 2026-07、08 两个月的阿里云后付费与手工台账逐条业务线核对，差额均在一分钱以内。
代价是要把这几个月的数据读出来聚合一次——账单一个月不过几万到几十万行，比日志表小四个数量级，可以忽略。

**`--bill-dedupe=final` 同样会丢并列行**：`FINAL` 就是引擎的去重语义，同键只留版本最新的一行。在 goscan
让每一行账单都有唯一的键之前（见下文「账单表的分片键」一节末尾），不要切到 `final`。

两个实现上的细节：

* **去重键从 `system.tables.sorting_key` 读**，不是写死的（读不到才退回 goscan 当前 DDL 的那套，并打一条
  warn）。goscan 改了 ORDER BY 而 opdash 没跟的话，按旧键去重会**悄无声息地少算金额**——这类错误不会有人察觉。
  Distributed 表自己没有排序键，所以问的是它底下的 `_local`。
* **子查询里的别名一律加下划线前缀**（`AS _instance_id`）。别名和真实列名撞上时，ClickHouse 的分析器会把
  WHERE 里的那个列名解析成聚合结果，模糊搜直接报 `184: Aggregate function any(instance_id) is found in WHERE`。

#### 成本归属：把费用摊到业务线

账单只回答「哪个产品花了多少」。对账时真正要回答的却是「哪条业务线花了多少」，两者之间差着一层归属：
一台机器属于谁、一项共用服务按何比例分摊给几条业务线。**这层知识不在账单之中**，云厂商也无从得知，
只能由部署方给出，因此它是一份外部配置（`--bill-alloc rules.toml`），而非代码中的常量——业务线名称、
实例的内网地址都属于内部信息，不应随仓库分发。示例见 `examples/bill-alloc.toml`。

规则文件的模型只有四件事：

```toml
lines = ["业务线甲", "业务线乙", "公共资源"]   # 业务线，顺序即页面上的顺序
unmatched = "公共资源"                        # 未命中任何规则的费用归入哪条线，可省略

[[include]]                                   # 只统计命中其中任一条的账单行，可省略
subscription = ["PayAsYouGo", "按量计费"]

[prepaid]                                     # 预付费按服务期摊到各月，可省略
subscription = ["Subscription", "包年包月"]
lookback_months = 36                          # 往前回溯购买记录的月数，不截断服务期

[[rules]]                                     # 自上而下匹配，命中第一条即停
name = "业务线乙的专用机器"
product = ["云服务器 ECS"]                    # 跨云统一的维度，与排行下拉里的那几项同名同义
columns = [{ name = "intranet_ip", any_of = ["10.0.1.11"] }]   # 维度表达不了的，直接匹配原始列
to = "业务线乙"                               # 整笔归一条线

[[rules]]
name = "ECS 其余部分按机器数拆分"
product = ["云服务器 ECS"]
split = { "业务线甲" = 130, "业务线乙" = 400 }  # 或按权重摊给若干条，只论相对大小
```

几处值得说明：

* **命中即停，所以顺序有意义**。窄的规则（某几台机器属于谁）写在前，宽的规则（同一产品的其余部分按比例
  拆分）写在后。这既是为了让窄规则有机会命中，也是为了杜绝同一笔费用满足两条规则而被计两次——线上确实
  遇到过：弹性伸缩组释放的内网地址被另一批机器复用，若先按地址匹配，那部分费用会在两条业务线上各计一次。
* **分类在 ClickHouse 内完成**。规则被翻译成一条 `multiIf`，与去重、筛选在同一条 SQL 里；取值一律绑定为
  参数。账单一个月数万行，取回进程内再分类既慢又无必要。
* **要匹配的原始列在某张表上不存在时，该规则对这张表整体不生效**，而不会退化为「一律命中」。`intranet_ip`
  只有阿里云有，若把缺列的条件当作恒真，一条「某几台机器归业务线乙」的规则会把火山引擎的全部费用也计入
  业务线乙。
* **预付费（包年包月）单走摊销那条路**。这类账单在购买当月一次性出账，若按出账月计入，那个月会凭空鼓起
  一大块，日均与月度预估随之失真。配了 `[prepaid]` 之后，每一笔购买按它自己的 `service_period` 摊到各月：
  一台包年的机器在十二个月里各计十二分之一。命中 `[prepaid]` 的行会**自动从「按出账月计入」那条路里排除**，
  不会两头各计一次；归属仍走同一套 `[[rules]]`，规则里写上 `subscription = ["Subscription"]` 便可给预付费
  单独定归属（譬如包年的数据库属应用、按 ECS 比例拆，而后付费的数据库归基础保障）。
  三点需要留意：升降配只收退差价，其 `service_period` 记的是「剩余天数」而非整期，按同一套摊法处理即可，
  钱是真实发生的；火山引擎的账单没有服务期列，摊不动，那部分仍按出账月计入，**不会两头都不落**；阿里云
  接口只保留 18 个月账单，更早购买且仍在服役的机器不在库里，这部分成本看不到。
* **日均只算后付费，月度预估分两段相加**。预付费按月摊，除以天数没有意义，故不参与日均；月度预估因此是
  「后付费日均 × 目标月天数 + 该月的预付费摊销」。后一段不是估出来的——已经发生的购买摊到未来几个月的
  金额是已知的，接口会把区间之后十二个月的摊销一并给出，页面据此算下个月。
* **日均的分母是「有账单的天数」，不是自然月的天数**。当月账单尚未出齐，按 30 天摊薄只会低估日均，愈近
  月初偏差愈大。月度预估则反过来：日均 × 目标月的自然天数。页面上可将日均的窗口收窄到最近 7 / 14 / 30 天，
  以避开月初扩容等早期波动；窗口以**各表最后一日有账单的日期**为基准回溯，而非以今日为基准——账单滞后
  一两日出具，自今日倒推会平白少算几天，且两朵云的同步进度未必相同。
* **未命中规则的金额始终单列**。即便配置了 `unmatched` 把它并入某条业务线，页面仍会标出这一笔有多少、
  占比几何，以便知晓规则还有多少没覆盖到。
* **不配规则也能用**：分析视图照常给出按产品的日均与月度预估，只是没有业务线这一层。

#### 手动同步账单

账单不是推上来的：goscan 按自己的 cron 去云厂商的接口拉。刚接入、补历史账期、或者当天的调度还没到点时，
页面上就是空的——所以费用页右上角有一个「同步账单」，把这个动作转给 goscan：

```text
浏览器 ──▶ POST   /api/bills/sync                ──▶ POST   {goscan}/sync              登记后台任务，拿 task id
浏览器 ──▶ GET    /api/bills/sync/{id}/events    ──▶ GET    {goscan}/tasks/{id}/events 进度推送（SSE），done 之后刷新页面数据
浏览器 ──▶ GET    /api/bills/sync/{id}           ──▶ GET    {goscan}/tasks/{id}        推送不可用时退回每 2 秒轮询一次
浏览器 ──▶ DELETE /api/bills/sync/{id}           ──▶ DELETE {goscan}/tasks/{id}        停止：写完当前这一趟再停
浏览器 ──▶ GET    /api/bills/sync/running        ──▶ GET    {goscan}/tasks             这朵云眼下有没有同步在跑
```

对接口径以 goscan README 的「手动同步（给 opdash 对接）」一节为准（swagger 注解会与实际行为有出入）。
几点说明：

* **opdash 仍然不写库**。账单是 goscan 拉回来再写进 ClickHouse 的，opdash 只转发「拉一次」「停下」这两个指令，
  自己的每条查询照旧带 `readonly=2`。这也是 opdash 仅有的会向外发出改变状态的请求。
* **要经 opdash 转一手**，是因为 goscan 的 HTTP 接口没有认证（集群内服务），而 opdash 有登录。
  让页面直连 goscan 等于把它暴露给浏览器。
* **进度靠推送，不靠轮询**（goscan v0.5 起）。任务每变一次——受理、开跑、写完一批、换账期、结束——goscan 就推
  一帧，opdash 逐帧转给浏览器，并换成与轮询接口相同的 JSON；收到 `done` 后页面主动关闭连接，否则 `EventSource`
  会不停重连。老版本 goscan 没有这个接口，opdash 回 404，浏览器不再重连，页面随即改为每 2 秒轮询一次。
  进度的单位是「趟」（一个账期 × 一种粒度），一趟可能要跑好几分钟，其间以「本趟已写入 N / M 行」显示仍在推进。
* **同一朵云同时只能有一个同步**。打开「同步账单」时，若这朵云已有同步在跑（包括 cron 发起的），页面直接接上
  它的进度；点「开始拉取」撞上 409 时也一样，而不是只报一句「正在同步中」。关闭窗口不会中断任务，重新打开仍能看到。
* **可以中途停下，但停在两趟之间**。goscan 每一趟拉之前都会先清空那个账期，半路掐断会留下只写了一半的账期，
  所以它把手上这一趟写完才停，按下去到真正停下要等几秒到几分钟，其间按钮显示「正在停止…」。停下之后，没跑的
  那几趟（如 `2026-09 日度`）会列出来，这些账期的数据原样没动。已经结束的任务再点停止，goscan 回 409。
* goscan 拒绝触发时的语义原样透出：409 是「已有同步任务正在执行」，429 是「已达并发上限」。

没有配 `--goscan-url` 的部署不显示这个按钮，接口也会回 400 说明原因；账单仍可等 goscan 自己的 cron，
或用 `goscan --once config.yaml --provider alicloud --start 2026-01 --end 2026-06` 在集群里补。

**MCP 工具里没有这一条**：`cost_*` 三个工具和其余工具一样都标了 `readOnlyHint`，让模型去触发一次几分钟的
云厂商拉取不在只读的承诺之内；要补数据由人在页面上点。

#### 表名认哪几个

goscan 的表名动过两轮：集群上的 `_distributed` 后缀取消了（现在和 logpipe 一样，Distributed 表就叫基础名），
火山那张从 `volcengine_bill_details` 改成了 `volcengine_bill`。而两轮改名是「先按新口径重建表、后改配置」，
中途库里会有多个名字并存——2026-09-22 16:43 那次 DDL 之后，`logs` 库中同时存在 `volcengine_bill_details`
（刚建的 Distributed）、`volcengine_bill_details_distributed`（上一轮留下的）和 `volcengine_bill_details_local`
（真正存数据的）；到 16:57 重建为新名并清掉旧表，才收敛成现在的三张 `volcengine_bill` /
`alicloud_bill_monthly` / `alicloud_bill_daily`。

opdash 因此按一串候选依次查找，**零配置即可对上**：基础名 → `<基础名>_distributed` → 火山那张再加
`volcengine_bill_details` → `volcengine_bill_details_distributed`。`_local` 始终不在候选之列：它只是一个分片的
数据，查出来的金额只有三分之一。显式配置了 `--volcengine-bill-table` 且名字不同的部署不再回退到旧名。

#### 账单表的分片键：goscan 已经改了

上述去重是**规避**，而非**根治**。根治的四条落在 goscan 的 `pkg/ddl` 里，2026-09-22 已全部改完
（那三张表当时刚建好、尚无一行数据，改表结构无需迁移任何内容）：

| 改了什么 | 从 | 到 |
| --- | --- | --- |
| Distributed 分片键 | `rand()` | `cityHash64(<排序键>)` |
| 排序键 | 末位是金额（`PayableAmount` / `payment_amount`） | 只有业务身份：火山用 `BillDetailId`，阿里云用「账号 + 产品 + 实例 + 计费方式 + 拆分 / 调整记录」 |
| 引擎 | `ReplacingMergeTree` | `ReplacingMergeTree(updated_at)`，后拉到的那份胜出 |
| 火山金额列 | `String` | `Decimal(20, 8)` |
| 分区表达式 | `toDate(ExpenseDate)`、`parseDateTimeBestEffort(...)`，空值抛异常 | `parseDateTimeBestEffortOrZero(...)`，脏值落进 1970-01 分区而不是让整批 INSERT 失败 |

**这次调整不能原地升级**：引擎、排序键、分区键和列类型都是建表时定死的，`CREATE TABLE IF NOT EXISTS`
对已存在的表不起作用。老表要先 `DROP` 再按新 DDL 建，然后重新同步。

opdash 这边两处跟着变：**金额的转换函数按库里的真实列类型选**（`String` 用 `toFloat64OrZero`，
`Decimal` 用 `toFloat64`，混用会被 ClickHouse 以 43 拒掉），所以新旧两种表都查得了；去重键仍然从
`system.tables.sorting_key` 读，表一重建就自动跟上。~~分片键确定之后 `--bill-dedupe` 可以调成 `final`~~——
**这一条作废**，原因见下一小节：排序键不唯一，`final` 会丢掉并列行。`group` 那条路对任何分片键都正确。

以下是当初的四条建议原文，留作改动的依据：

1. **Distributed 的分片键不应使用 `rand()`**，应改为按去重键哈希，例如
   `Distributed('log', 'logs', 'volcengine_bill_local', cityHash64(BillPeriod, InstanceNo))`。同一行的多次写入
   从此落在同一分片，`ReplacingMergeTree` 与 `FINAL` 方才真正生效。账单数据量小，**单分片足矣**
   （`Distributed(..., 1)` 或干脆不建 Distributed 表）——分三片的唯一收益是并行扫描，而这几张表一个月的
   数据量尚不及日志表一分钟。调整之后，opdash 侧把 `--bill-dedupe` 改成 `final` 即可。
2. **排序键里不要放金额列**。现在 `PayableAmount` / `payment_amount` 是去重键的一部分：云厂商月中调整账单
   （退款、优惠重算、发票折扣）之后，同一计费项的金额发生变化，新旧两行的排序键随之不同，**两行都会保留**，
   而这恰恰是「重复拉取」最应当收敛的情形。排序键应该只放业务身份——火山那张表有现成的 `BillDetailId`，
   阿里云那两张可以用 `(billing_cycle/billing_date, bill_account_id, product_code, instance_id, subscription_type,
   split_item_id)`——再配合 `ReplacingMergeTree(updated_at)`，令后拉取的那一份胜出（`updated_at` 列已经存在）。
3. **火山那张表的金额不宜存成 `String`**。`PayableAmount` 这些列现在是 `String`，每次求和都要
   `toFloat64OrZero`，排序与跳数索引都用不上，压缩率也差。金额宜用 `Decimal(20, 8)`，不丢精度；若需保留 API 返回的
   原始文本以便核对，可另设一列存放。
4. **`PARTITION BY toYYYYMM(toDate(ExpenseDate))` 存在隐患**。`ExpenseDate` 是 `String`，`toDate('')` 会抛异常——
   云厂商只要返回一条 `ExpenseDate` 为空的账单，**整批 INSERT 都会失败**，且只有写入时才会暴露。应改为
   `toYYYYMM(toDate(parseDateTimeBestEffortOrNull(ExpenseDate)))`，或增设一列 `MATERIALIZED` 的日期列并按其分区。

若第 1、2 条不改，opdash 的 `group` 去重能挡住「重复拉取」，却挡不住第 2 条所述「金额被修正」的重复——
那种重复在任何去重键下都是两行不同的数据，唯有引擎带版本列方能判定孰新孰旧。

#### 还差一条：排序键要能唯一标识一行账单

2026-09-23 与手工台账对账时发现，阿里云账单里存在**排序键完全相同、只有金额不同**的两行：一笔正常费用加
一笔尾差调整（原价 0、应付 −0.005），或者同一台机器同月先包月、再转包年（1,612.29 与 13,768.2）。
这对 `ReplacingMergeTree(updated_at)` 是致命的——两行同键，按新的分片键必然落在同一分片，**合并时只留
`updated_at` 较大的一行，另一行被永久删除**。`updated_at` 相同（同一批写入，精确到毫秒也可能相同）时留哪一行
不确定，丢掉的可能是那笔 13,768.2。`logs.alicloud_bill_monthly` 里眼下就有一组尚未合并的：

```text
2026-08  大模型服务平台百炼  <账号>;<应用>;<模型>;input_token;0    34837.5724  03:04:47.538
2026-08  大模型服务平台百炼  <账号>;<应用>;<模型>;input_token;0       -0.0045  03:04:47.197
```

这一组碰巧大额那行晚了 0.34 秒，合并后会被保留；顺序反过来，这笔费用就没了。opdash 的查询期去重能把
并列行都算上（见上一节），但**存储层合并掉的行，查询时无从找回**。

**goscan v0.5 已经根治**：阿里云两张表的排序键加入了 `item`（订单 / 后付账单 / 退款 / 调账）与 `line_seq`
（同一次拉取中其余各列都相同的行，按拉到的顺序编号 0、1、2…），每一行账单从此都有唯一的键；重拉某个账期前
先按分区清空它，云厂商撤掉的行不会残留。opdash 的静态兜底去重键已跟着更新（通常用不上，去重键从
`system.tables.sorting_key` 读）。

**但表要重建才生效**：排序键是建表时定死的，只跑 goscan 的补列语句会把这两列加上，它们却不在键里，撞键的行
照样被合并掉。线上阿里云两张表需要按 goscan README「2026-09 的结构调整不能原地升级」一节先删后建，再重新同步。
重建之前，`--bill-dedupe` 继续用默认的 `group`。

### 一条日志能有多大

线上 `message` 的 p50 是 **129 字符**、p99 是 9.4 KB——但一小时 844 万条里有 **200 条超过 1 MB，
最大 49 MB**（业务把整个响应体打进了日志）。撞上一条，页面就完了：一条 52 行的链路日志响应
**63.8 MB / 12.6 秒**，浏览器主线程连续无响应 **近两分钟**。

所以列表、上下文、跟随这三条路都在 **SQL 里**截断 `message`（`--max-message-chars`，默认 16384 字符，
是 p99 的 1.7 倍，正常的堆栈和 SQL 一个字都不会少），并带回原始长度 `message_len`，
前端在文字断掉的那一点上标「· 已截断」，展开里说清楚完整有多少字、要全文去导出。
同一个请求改完是 **103.8 KB / 0.38 秒**，主线程最长卡 5 ms。

几个坑：

* **必须在 SQL 里截，不能拿回来再截**：那 12.6 秒里 ClickHouse 只占 1.7 秒，另外 11 秒全花在
  ClickHouse → opdash 这一程的传输上，在 Rust 里截省不掉。
* **单位是字符不是字节**（`substringUTF8` / `lengthUTF8`）：按字节切会把多字节字符劈成半个，
  ClickHouse 对非法 UTF-8 的行为是未定义的，吐出来的 JSON 可能直接解析不了。
* **原始长度要写成 `` `app_log`.message ``**：直接写 `lengthUTF8(message)` 会解析成上面那个截断后的
  别名 `message`，量出来永远等于 cap（线上就返回过 16384 而不是真实的 41149053）。和
  `export_columns` 里 `time` 别名踩的是同一个坑，单测钉住了写法。
* **导出不截**：那是拿全文的唯一一条路，截了就没意义了。

### 查询是怎么写的（排障时看这里）

* 所有用户输入都走 ClickHouse 查询参数 `{name:Type}`，SQL 文本里只有白名单里的列名。`String` 参数按
  TSV 规则转义（ClickHouse 那头按 TSV 解），正则里的 `\d+` 才能原样到达。
* 每个请求带：`readonly=2`、`cancel_http_readonly_queries_on_client_close=1`（关掉页面查询就停）、
  `max_execution_time`、`wait_end_of_query=1`（错误一定是干净的 5xx 而不是 200 + 半截 JSON）、
  `output_format_json_quote_64bit_integers=0`；span 表的查询另带 `optimize_skip_unused_shards=1`。
* 时间范围：`timestamp >= fromUnixTimestamp64Milli({from:Int64})`，参数代入后是常量，能裁剪分区、走主键。
* 日志检索 / 上下文 / 导出统一 `ORDER BY timestamp, level, trace_id`——**就是表排序键去掉服务的那一截**
  （锁定单个服务时正是该服务数据段的物理顺序），ClickHouse 才能纯按顺序倒着读、读够 LIMIT 就停。排序键以外的列一旦参与排序，计划里会多出
  `PartialSorting` + `FinishSorting`，为了定出这 200 行的先后要多读一大堆。线上 26.3 实测（1 小时窗口）：

  | | 排序键以外的列也参与排序 | 只按排序键 |
  |---|---|---|
  | 翻一页 200 行 | 3.03 GB / 4240 ms | **0.14 GB / 166 ms** |
  | 关键字 + 200 行 | 9.19 GB / 20.8 s | **6.00 GB / 4.7 s** |
  | 查看上下文 50 行 | 7.58 GB / 9075 ms | **0.47 GB / 137 ms** |

  代价：同一 `(timestamp, level, trace_id)` 的行之间先后不保证（线上 10 分钟里 38% 的行落在这种并列组
  里，最大一组 233 行），页边界正好切在一组中间时翻页可能重复或漏几行。要精确得换 keyset 翻页——
  游标带上这个元组、不用 OFFSET，顺带能解除 `max_offset` 的翻页上限，还没做。
* 关键字语法参考 Kibana / Datadog：空格 = AND，`a OR b`，`-词` / `NOT 词`，`"带 空格"`，`( )` 分组，
  优先级 NOT > AND > OR；`AND` / `OR` / `NOT` 全大写才是操作符。解析不报错，悬空的操作符和多余的括号
  直接忽略；词内配对的括号（`getUser(id)`）照字面搜。每个词是 `positionCaseInsensitiveUTF8(message, ...)`，
  全是词的 OR 合成一个 `multiSearchAnyCaseInsensitiveUTF8(message, [...])` 一趟扫完。message 没有索引，
  扫的是时间范围内的全部行。
* 关键字搜索择机走 **token 索引**。`app_log_local` 上有
  `INDEX idx_message_tokens lower(message) TYPE tokenbf_v1(131072, 3, 0) GRANULARITY 1`，
  词按 tokenizer 同样的规则（非字母数字的 ASCII 字符，下划线也算）切开，**够长的（≥16 位）token**
  才当 needle：

  * 纯 id（span id 16 位、trace id / msgId 32 位）：发 `hasToken(lower(message), 小写词)`，单独就是
    完整语义。线上实测一小时窗口查一个 msgId：**7.83 GB / 1063 ms → 0.030 GB / 140 ms，命中数一致**。
  * 键加 id（`msgId:AC10…`、`traceId=…`）：id 那个 token 发 `hasToken` 跳 granule，再 AND 上原来的
    子串条件保证键也对得上。2026-09-18 实测一小时窗 `msgId":"AC10…`（键加 32 位 id）：
    **10.24 GB / 1.2 s → 0.02 GB / 0.2 s**（读 630 万行 → 4661 行，剩下的是 bloom filter 的误判）。
  * `RESULT_CHANGE`、`im_enter_direct_msg` 这类切出来全是短词的复合标识符**不发**：2026-09-18 量过
    （1 小时窗、3 分片），`change` / `result` 各命中 359 / 364 个 granule，一个都跳不掉，多出来的两个
    `hasToken` 反而让墙钟多 10% ~ 40%（1320 → 1950 ms、1517 → 1655 ms）。常见词怎么组合都进不了索引，
    原因见下面「跳数索引的上限」；这类词 24 小时的子串检索就是要读 250 ~ 290 GB（实测 10 ~ 12 GB /
    小时），护栏按总量算，100 GiB 在十来个小时处必然触发。

  规则的边角，改之前先看清楚：

  * needle 必须是切好的纯字母数字 token——`hasToken` 遇到带分隔符的 needle 会**抛异常**而不是返回空。
  * 排除词（`-词` / `NOT 词`）一律保持子串语义：整词比子串窄，取反之后变宽，会漏掉本该排除的行；
    否定条件本来也用不上 bloom filter。
  * OR 组仍走 `multiSearchAnyCaseInsensitiveUTF8`，不进索引。
  * 语义确实收紧了：搜 id 的前半截不再命中。响应里的 `token_terms` 列出哪些词按整词匹配了，
    页面上标成「按整词匹配 · 已走索引」，要搜片段用正则模式。
  * 短词故意不走索引——搜 `health` 得能匹配 `healthcheck`。阈值在 `TOKEN_MIN_LEN`。
* **文本索引（26.2 GA 的 `TYPE text`）对我们的常见关键字也没用**，2026-09-10 实测过再下的结论。
  判断一个跳数索引的**上限**不用真建索引：直接数「有多少个 granule 至少命中一次」就行
  （`uniqExactIf((_part, intDiv(_part_offset, 8192)), 条件)`）。一小时窗口 740 个 granule：

  | 关键字 | 命中的 granule | 索引最多能省 |
  |---|---|---|
  | `青栀`（生僻中文） | 23 / 740 | 32× |
  | `发送私信事件监听器` | 740 / 740 | 0 |
  | `im_enter_direct_msg` | 740 / 740 | 0 |
  | `WX_RECOGNIZE_SHADOW` | 736 / 740 | 0 |
  | `sendWebHooksMsgId` | 740 / 740 | 0 |
  | `timeout` / `msgId` | ~740 / 740 | 0 |

  一个 granule 是 8192 行、约 5 秒的全量日志（1600 行/秒，所有服务混在一起）；只要这个词
  平均每几秒出现一次，它就在每个 granule 里，**任何**跳数索引都跳不掉。真正稀疏的是
  32 位 id 那类——那已经由 `idx_message_tokens` 覆盖了。（关键字取自 `system.query_log` 里
  近 7 天用户真实搜过的词，不是拍脑袋选的。）
* **日志表的排序键要换成 `(service_name, timestamp, level, trace_id)`，服务打头**（2026-09-18 的结论，
  之前是 `(timestamp, level, trace_id)`）。**线上还没换**：2026-09-21 查 `system.tables`，三个分片的
  `app_log_local` 仍然是 `(timestamp, level, trace_id)`，下面这些收益都还没拿到。当年不放服务打头的理由是「近 7 天 12823 次检索只有 3% 带服务筛选」，
  到 2026-09 用法变了：近 7 天 584 次检索里 67% 带服务，关键字检索里 74% 带服务，而「服务 + 关键字」
  是最疼的一类——排序键里没有服务时，服务筛选**完全不减少读量**（message 按 granule 整块读，每个 granule
  里都有这个服务），3 小时以上的这类检索 61 次挂了 41 次（撞 20 GiB 护栏）。建了一张新键的试验表、
  灌进同样的数据对比（缓存全关，正常量级的 3 小时窗、2144 万行）：

  | 查询 | 时间打头 | 服务打头 |
  |---|---|---|
  | 服务 + 关键字 列表 | 34.1 GB / 4.5 s | **2.0 GB / 0.76 s** |
  | 服务 + 关键字 直方图 | 34.0 GB / 3.8 s | **1.9 GB / 0.39 s** |
  | 服务 + 关键字（最大的服务，占 30% 字节） | 34.1 GB / 4.8 s | 7.9 GB / 1.3 s |
  | 只筛服务 最近 200 条 | 0.03 GB / 311 ms | 0.08 GB / 102 ms |
  | 无筛选 最近 200 条 | 0.02 GB / 302 ms | 0.39 GB / 207 ms |
  | 无筛选 直方图 | 0.19 GB / 237 ms | 0.19 GB / 157 ms |
  | 只有关键字（稀有词） | 34.1 GB / 6.5 s | 34.9 GB / 6.1 s |
  | trace id 查找 | 0.07 GB / 419 ms | 0.08 GB / 114 ms |
  | 筛选下拉 facet | 0.93 GB / 464 ms | 0.93 GB / 309 ms |

  「无筛选最近 N 条」不能再顺序读、读够就停，要把范围内的排序列读出来排一遍，读量涨 20 倍但绝对值
  很小（排序列都很窄），墙钟没变差；主键分析对第二列 `timestamp` 的范围条件仍然有效（generic exclusion
  search），不会读整个分区。「只有关键字」失去早停，常见词理论上会变慢，实测没量出差别。
  换表的做法：建 `app_log_v2`、按分区回灌、`EXCHANGE TABLES` 原子换名、再补灌换名前后的两个分区；
  Distributed 表和 logpipe 都按名字写，不用改。
* 手动写 `PREWHERE` 没有意义：`optimize_move_to_prewhere` 默认开着，实测把 `service_name`
  显式提到 PREWHERE 读量一字不差（39.6 GB → 40.5 GB，还略涨）。
* **`ngrambf_v1` 试过，无效，别再走这条路**：8192 行日志里就有 13 万个不同 trigram，几乎覆盖整个
  现实 trigram 空间。取 20 个 granule 对 6 个真实关键字（含 32 位十六进制 msgId）验证，一个都跳不掉。
  token 不一样是因为它的取值空间无穷大——一个 msgId 只落在真正含它的那一两个 granule 上。
* 用不上索引的关键字（带标点 / 中文的子串）仍要扫完时间范围内的 message：2026-09-10 复测
  **一小时约 10 GB、两小时约 40 GB**（未压缩，全集群；`message` 一列就占全表 911 GB 里的 761 GB，
  1740 字节/行）。分片之间不均，两小时的关键字检索单分片就能撞上 `--max-read-bytes` 的 20 GiB
  护栏——线上 24 小时里 14 次 307 全是这么来的，护栏本身是对的，要让两小时以上的关键字检索
  跑完只能把它调大（40 GiB 量级）或者接受「关键字检索限一小时左右」。这类查询的长尾成因没查出来——不是 Keeper（复制队列全 0）、不是后台合并
  （p90 与合并字节数相关系数 −0.12）、也不是读带宽限流。`OPDASH_QUERY_TIMEOUT` 因此设成 90s
  而不是默认 30s（见 `deploy/`）。
* **带关键字时检索和直方图串行发**（2026-09-10 改）。两条查询的 WHERE 一模一样，而 `message`
  没有索引、要扫完整个时间范围。并发发出去就是同一段数据扫两遍；错开之后第二条命中
  ClickHouse 26.x 的 **query condition cache**（`use_query_condition_cache`，服务端默认开，
  记的是「哪些 granule 不满足这个条件」）：线上实测第一条 **39.8 GB / 3.5 s**，紧接着同条件的
  第二条 **0 GB / 18 ms**。顺序是「检索在前、直方图在后」——列表是人盯着的那块，不能为了
  直方图让它变慢；关键字命中多的时候检索读够 200 行就停，那种情况下直方图自己扫，两种情况
  加起来集群大约只扫一遍。
* 「共 N 条」不单独跑 `count()`：直方图各桶之和就是总数（时间条件左闭右开、桶按同一原点切，每行都
  落在某个桶里），日志页给 `/logs/search` 传 `count=0` 关掉它，省下一条扫同样数据的查询。按 trace id
  查（没有直方图）时才回到 `count()`；关键字搜索那条路走 `exact_rows_before_limit=1`，一次扫描顺带出总数。
MCP 的 `search_logs` 同样默认 `count=0`（模型要总数用 `log_histogram`，顺带还给时间分布，或者显式 `count=true`）。
* 相对范围（`最近 N 分钟`）在前端解析成绝对毫秒后**固定到下次刷新**：翻页、改排序、切页面都不会让
  `to` 往前爬。否则每改一个参数 `from` / `to` 就变几毫秒，直方图和 facet 明明和翻页无关也得重扫一遍，
  而且第二页和第一页的窗口边界对不上、行会错位。点刷新（或重新选范围）才推进到当前时间。
* 链路检索两次往返：先 `SELECT trace_id ... ORDER BY timestamp DESC LIMIT 1 BY trace_id LIMIT n`
  拿候选 id，再 `WHERE trace_id IN {ids:Array(String)} GROUP BY trace_id` 聚合摘要。不用嵌套子查询，
  Distributed 表上 `distributed_product_mode=deny` 也没问题。「请求耗时」= 根 span 的耗时（根缺失时
  退回最早的 span）；「总跨度」= 最早 span 开始到最晚 span 结束，异步消费会让它比请求耗时长得多。
* **链路检索的两条查询都不扫整个时间窗**（2026-09-14 改）。改之前一次「最新 50 条」是
  5248 万行 / 2.1 GB / 4.4 秒，两条查询各贵一半，贵的原因不一样：

  **候选查询**：`timestamp` 不是排序键前缀，`ORDER BY timestamp DESC` 在 `EXPLAIN` 里是
  `Sorting (Sorting for ORDER BY)`，窗口内每行的 `trace_id + timestamp` 都得读出来重排。但最新
  的 50 条挤在窗口末尾——线上实测这 50 条的时间跨度只有 **16 毫秒**——所以先查最后 1 分钟，
  不够再 15 分钟，还不够才整窗。1 分钟窗 124 万行 vs 整窗 1 小时 3043 万行。最小一级取 1 分钟
  是因为时间是第三级排序键，每个 `(service_name, span_name)` 组合都得捞一段 granule，5 秒窗也要
  读 103 万行，再小没收益。按耗时排不能这么干（最慢的那条可能在窗口任何位置）。

  **摘要查询**：`trace_id` 上只有 bloom filter（GRANULARITY 4，2.5% 误报），50 个 id 一起 OR，
  一个索引块活下来的概率是 `1 - 0.975⁵⁰ ≈ 72%`——线上 `EXPLAIN indexes=1` 实测 8513/11480，
  和期望值对得上。也就是说这一步基本挡不住块，**唯一的杠杆是把主键能圈到的时间窗做小**。
  所以时间谓词锚到候选自己的跨度（±10 分钟），而不是整个搜索窗。

  收窄会切掉跑得久的链路（MQ 消费那种），用 **`root_count = 0`** 认出来再按完整窗口补一次：
  完整的链路一定有一个 `parent_span_id = ''` 的根 span，窗口里找不到就说明前面被切了。补捞不贵，
  id 一少 bloom filter 就重新管用。线上采样 10.7 万条链路，跨度超过 10 分钟的只占 0.02%，
  但它们 span 多、被「最新 50 条」抽中的概率也高，一页里能有 0~4 条。

  | 窗口 / 排序 | 扫描行 | 读量 | ClickHouse 耗时 |
  |---|---|---|---|
  | 1 小时 · 最新在前 | 5865 万 → **1250 万** | 2.35 → **0.51 GB** | 1451 → **466 ms** |
  | 6 小时 · 最新在前 | 8900 万 → **1278 万** | 3.58 → **0.53 GB** | 1677 → **539 ms** |
  | 6 小时 · 最慢在前 | 5572 万 → 5560 万 | 2.08 → 2.08 GB | 1549 → 1493 ms |

  12 个窗口 × 两种排序逐条对过：返回的 50 条 id 和每条摘要的每个字段**和改之前完全一致**。
  按耗时排这一轮基本没收益（候选散布在整个窗口，收窄不了）——它由下面两条接手。
* **`trace_id IN` 放 PREWHERE，按耗时排的候选不让库去重**（2026-09-14 改）。上面那轮之后
  「最慢在前」成了页面上最贵的一条，两处各挖了一刀：

  **摘要查询的 `trace_id IN` 挪进 PREWHERE**。自动 PREWHERE 不挑这一条（大数组 + String 列不符合
  它的启发式），留在 `WHERE` 里 ClickHouse 会把聚合要用的八列全读出来再过滤。挪过去之后每行只读
  `trace_id`，其余列只为命中的行读：1 小时高峰窗 **1257 MiB / 2524 ms CPU → 1042 MiB / 1899 ms
  CPU**（中位 / 5 次）。1042 MiB ÷ 3000 万行 = 34.7 字节，正好一个 `trace_id`，也就是到底了；
  再往下只能动 bloom filter，那是 tracepipe 的表。

  **按耗时排的候选去掉 `LIMIT 1 BY trace_id`**，改成多取 10 倍的行、在 Rust 里去重。带 `LIMIT 1 BY`
  时计划是老实的 `Sorting`，`trace_id` 得为窗口里每一行物化出来再排；去掉之后变成
  `Limit (preliminary LIMIT)` + `LazilyReadFromMergeTree`——只读排序列定位前 n 行，`trace_id`
  只为这 n 行读：**285 MiB / 1310 ms CPU / 927 ms → 189 MiB / 723 ms CPU / 136 ms**。

  这一刀**只对按耗时排有效**：最慢的 span 来自不同链路，实测 7 个窗口取 500 行能去重出
  83~179 个 trace，都够 50 有余。按时间排则不行——最新的 span 扎堆（同一条忙碌链路一毫秒内能写
  好几个 span），同样取 500 行只剩 46 个，不够 50；反正它已经靠探测把窗口缩到 1 分钟了。
  取满了 `limit × 10` 行却凑不够（一条链路占满了最慢的那几百个 span）就退回 `LIMIT 1 BY`。

  | 窗口 / 排序 | 读量 | ClickHouse 耗时 | 端到端 |
  |---|---|---|---|
  | 1 小时 · 最慢在前 | 1538 → **1235 MiB** | 1389 → **674 ms** | 1541 → **782 ms** |
  | 6 小时 · 最慢在前 | 2699 → **2140 MiB** | 2150 → **1114 ms** | 2388 → **1233 ms** |
  | 6 小时 · 最新在前 | 635 → **571 MiB** | 673 → **617 ms** | 869 → 889 ms |

  12 个窗口 × 两种排序对过，返回的 50 条 id、顺序和每条摘要的每个字段都一致。（「最新在前」
  偶尔顺序不同是同毫秒并列：这 50 条落在 17 个毫秒里，最大的一撮 12 条同毫秒，
  **同一条 SQL 自己连跑 5 次也会换顺序**，和改动无关。）
* 热力图那条 `GROUP BY bucket, lvl` 量过，**没有可改的**：`EXPLAIN` 里只有一个 `Aggregating`、
  没有 `Sorting`，1 小时高峰窗 1159 万行 / 116 MiB / 140 ms，是页面上最便宜的一条。按 CPU 时间
  （中位 / 5 次，墙钟在这台集群上能抖 2~3 倍）拆开：基础扫描 + `span_kind` 过滤占 42%，多读一列
  `duration_ns` 占 26%，每行一次 `log10` 只占 9%。试过收窄分组键类型、把 `(bucket, lvl)` 打包成
  一个 Int64、用 `roundDown` 常量阈值和 `log2` 换算代替 `log10`、去掉 `countIf`——全在 ±10% 的
  噪声里。
* **链路详情不取属性列**（2026-09-10 改）。四个 JSON 列（`resource_attributes` / `span_attributes` /
  `events.attributes` / `links.attributes`）是这条查询的**全部**成本，线上同一条查询实测：

  | | 读量 | 耗时 |
  |---|---|---|
  | 带这四列（原来） | 0.259 GB | 1.8 s（同形状的 p99 41.9 s、最慢 43.3 s） |
  | 不带（现在的瀑布图） | **0.002 GB** | **50 ms** |

  原因不是这几列的数据多（这条 trace 里它们只有几十 KB），是**主键前缀只能圈到秒级**：
  `(service_name, span_name, toDateTime(timestamp))`，一条 42 毫秒的 trace 会拖进 5 万行候选
  （毫秒精度的话只有 108 行），JSON 列又是按 granule 整块读的——每个路径一条流，路径一多，
  读一个 granule 的固定开销就压过了真正要的那几行。所以瀑布图只取轻列，属性等用户点开某个
  span 再按 `/api/traces/{id}/spans/{span_id}` 单独取（0.011 GB / 0.9 s）。那个接口收
  `service` / `name` / `ts` 三个主键前缀提示，页面上本来就有，带上就省掉再定位一次（0.11 GB → 0.011 GB）。
* 链路详情也两次往返：先 `WHERE trace_id = ?` 只读 `span_id, service_name, span_name, timestamp` 定位，
  再按 `service_name IN ... AND span_name IN ... AND timestamp BETWEEN ...` 走排序键前缀取 JSON 属性、
  events / links 这些重列。`trace_id` 只有 bloom filter（GRANULARITY 4，2.5% 误报），一天一亿多 span 时
  过了索引的块绝大多数是误报（线上 EXPLAIN：18371 个 granule 剩 476 个，和期望误报数正好对上），一步
  到位地读完整 JSON 就是几个 GB、30 秒超时；拆开后误报块只读几十字节一行，重列由主键精确圈到。
  第二步不传 span id 列表：参数都在 URL 里，5000 个 id 会超过 64 KB 的 URI 上限；两步排序相同、
  时间区间卡在第一步的首尾毫秒，取同样多的行就是同一批 span。
* **超上限的 trace：退回围着 `at` 的窄窗口，链接指名的那个 span 单独捞回来**（2026-09-16 改）。
  定位那一步是 `ORDER BY timestamp LIMIT --max-trace-spans`，超了就按时间从早往晚切。线上那条
  `1719ae…8dbc` 是典型的 **trace id 被复用**（常驻消费者一直用同一个）：21882 个 span、跨 2 小时
  13 分。按老办法取回来的是最早的 5000 个——15:52 开始的那一段，而用户是带着 `at=16:52` 从错误
  分组点进来的，要看的 span 在被切掉的后半段里，点开只剩一张不相干的瀑布图。现在：

  1. **哪一档窗口装不下，就退回第一档够用的**（够用 = 至少 `max/10` 个 span，免得 `at` 偏了几分钟
     时退回一张空图）。窄窗口虽然窄，但它围着 `at`、在自己这段时间里是完整的，而且便宜得多——
     同一条 trace 实测：

     | | span 数 | 取数读量 | 关联日志的窗口 |
     |---|---|---|---|
     | 不退（最早的 5000 个） | 5000 | 16.4 MB | 18 分钟，且要看的 span 不在图里 |
     | 退到 ±15 分钟 | 4738 | 34.3 MB | 39 分钟 / 1.6 GB |
     | **退到 ±1 分钟（现在）** | **701** | **3.8 MB** | **11 分钟 / 0.28 GB** |

     响应里 `narrowed` + `window_from_ms` / `window_to_ms` 告诉前端只显示了哪一段，徽标上写清楚。
  2. **不限时间那一档（全表扫，线上 5.1 GB / 39 s）在明知装不下时不发**：上一档已经装了半个上限
     还多，扫回来也只会被截断、再退回窄窗口。
  3. 详情接口收 `span=`：指名的那个仍然不在结果里（窗口外、或第一档就装不下只能按时间切），就再发
     一条 `WHERE trace_id = ? AND span_id = ? LIMIT 1`（同一套探测窗口，成本和定位那趟同量级），
     取回来钉在 `spans` 末尾、`pinned_span` 里报一声。它的父链不一定在图里，瀑布图上是「父缺失」。
     没被截断、窗口又已经探到头时不追到「不限时间」那一档：窗口里的 span 是全的，为一个抄错的 id
     扫全部分区不值。同样的定位也给 `/api/traces/{id}/spans/{span_id}`——不带主键前缀提示时它原来
     是定位整条 trace 再从里面挑，超上限的 trace 上会挑不着。
  4. **没带 `at` 时锚在「现在」往回探**（2026-09-20 加）：原来没有 `at` 就没有中心点，直接发不限时间
     那一档。线上近 24 小时走过兜底的 11 条 trace 里有 8 条**一趟带窗口的查询都没发过**（也就是根本
     没带 `at`），正好是最慢的那几条——86.9 s / 36.7 s / 28.8 s / 20.0 s / 19.6 s，它们一家吃掉这个
     接口 97% 的读取量。而手点进来的 trace 几乎都是刚发生的：那 8 条被查时 7 条在 75 分钟以内（最近
     的一条只隔了 36 秒），最老的一条 13.3 小时。所以改成锚在 `now()` 探 ±15 分钟 → ±2 小时 →
     ±26 小时，探不中才落到不限时间。窗口往后的那一半落在未来、分区还不存在，不花钱；前后对称是因为
     「探中」要过「没贴着窗口边」那一关，往后留少了刚发生的 trace 会贴着 `now` 判成没探中。线上实测
     （用一个不存在的 id，量的就是白跑的代价）：0.31 s / 8.4 MB、0.33 s / 59.7 MB、2.29 s / 574.6 MB
     ——三档全空也只多花 2.9 s，而命中一档是 0.3 s 对 25.8 s。

     根子在索引上，**2026-09-20 已经修掉**：`otel_trace` 原来只有一个 `idx_trace_id bloom_filter
     GRANULARITY 4`，用的是**默认 2.5% 误报率**——不带时间条件时线上 EXPLAIN 1289480 个 granule 只挡掉
     到 29384（2.28%，跟误报率一致，几乎全是误报），一条 126 个 span、只跨 192 ms 的 trace 要读
     1.22 亿行 / 4.0 GB / 25.8 s。现在加了第二个 `idx_trace_id_v2 trace_id TYPE bloom_filter(0.001)
     GRANULARITY 4`，**两个一起用**（两个 bloom 的误报率是相乘的）：

     | 用哪个索引 | 过索引的 granule | 读行数 | 读字节 |
     |---|---|---|---|
     | 只有老的（改之前） | 29384 / 1289480 | 1.22 亿 | 4.08 GB |
     | 只有新的 | 1264 / 1289508 | 506 万 | 169 MB |
     | **两个都用（现在）** | **112 / 1289480** | **41 万** | **13.7 MB** |

     所以老索引别删——它多占 788 MiB/分片，换的是再少读 12 倍。两个索引合计 2.2 GiB/分片，数据是
     112 GiB/分片。全表 `MATERIALIZE INDEX` 三个分片一起 85 秒跑完，part 数和磁盘几乎没动（官方文档说
     mutation「重写整个 part」，实测只写索引文件、其余列走硬链接）。新建表要一步到位的话，一个
     `bloom_filter(0.000025)` 跟这两个叠加等价、占用也一样（bit 数正比于 `-ln(p)`，`0.025 × 0.001`
     和 `0.000025` 的账是一样的）。

  整页实测（线上那条 trace，`at` + `span` 都带着）：详情 1.5 s + 关联日志 0.40 s + span 属性 0.81 s
  ≈ 3.0 s；改之前关联日志一趟就 1.42 s / 1.6 GB，而且那个 span 根本选不中。
* 日志检索只取**显示得了的列**：字符串 / 数字 / 时间列，也就是 `/api/meta` 里给前端的那些维度。
  线上的 `app_log` 物理上带着整套 span 列（`resource_attributes JSON`、`events.attributes Array(JSON)`
  ……，logpipe 不写，全是默认值），照单全收只是白读白传：一小时窗口取 200 行 0.129 GB → 0.098 GB。
* 属性过滤写成子列标识符 `` span_attributes.`http.route` ``：只读那一个子列（线上 10 分钟数据 12 MB、
  40 ms）。`getSubcolumn(col, {path:String})` 虽然能把路径当参数，但 MergeTree 上会把整个 JSON 列读出来
  （同一查询 5.9 GB、5 秒）。路径进 SQL 前按标识符规则校验（不含反引号 / 反斜杠 / 控制字符）。
* **指标：累积量的速率是查询时相减出来的。** metricpipe 按 OTLP 原样存，counter 是进程启动以来的累计
  值（`temporality = 'Cumulative'`）——当初选择不在采集端转 delta，是因为多副本路由下很难做对。所以
  `agg=rate` / `increase` 的 SQL 是：桶内取最后一个累计值 → `lagInFrame` 拿上一个桶的 → 相减。
  `cur < prev` 当成进程重启（计数器归零），按 Prometheus 的做法把当前值整个算成增量。`Delta` 的桶内
  求和就完事，两种 temporality 在同一条 SQL 里用 `if(temp = 'Cumulative', ...)` 分开，不用先查一次表
  才知道是哪种。速率除的是**两个点的真实间隔**而不是桶宽：上报周期 60s、步长 30s 时除桶宽会把速率
  砍一半。
* **相减必须按时间线分，而时间线包括 resource 属性。** 同一个服务的两个 pod 报的是两条独立的计数器，
  混在一起相减会得到一串负数（然后被当成重启）。表上没有 series_id 列，只能现算
  `cityHash64(service_name, scope_name, toString(resource_attributes), toString(attributes))`，代价是把两个
  JSON 属性列整列读出来。查询已经锁死一个 `metric_name`，读的行数有限，认了；只做 `avg` / `last` 这类
  不用相减的聚合时不算这一步。
* 直方图分位数：把各时间线的 `bucket_counts` **先按时间线相减、再逐元素相加**（`sumForEach`），
  最后在服务端从桶计数和 `explicit_bounds` 插值。分位数不能对多条时间线取平均——那是把 p95 又平均了
  一次，没有意义。桶边界不一样的时间线合不到一起，`explicit_bounds` 因此进了分组键：真出现两套边界
  就是两条线，而不是悄悄算错。
* **查之前先问一句这个指标是什么类型。** 五种类型共用一张表、用不上的列留默认值，所以直方图行的
  `value` 是 0：拿 gauge 那套「`avg` + `value`」去查 `jvm.gc.duration` 不会报错，只会安安静静地回一片 0
  ——线上照着这个结论说过「整个集群没有 GC」。`/api/metrics/query` 现在先发一条 `LIMIT 1`（和主查询
  同一个 WHERE 前缀，几乎不花钱）拿到 `metric_type` / `is_monotonic` / 有没有 `explicit_bounds`：
  `agg` / `field` 没给就按类型挑（和页面上 `aggOptions` 的第一项一致：带桶的直方图→分位数、
  指数直方图 / Summary→`mean` + `sum`、counter→`rate`、gauge→`avg`），给了但必然查空的组合
  （直方图 `field=value`、非直方图 `agg=quantile`、指数直方图算分位数）直接回 400 并写清楚该怎么查。
  拒在主查询之前，那一趟也省了。
* 时间线太多时只画最大的 N 条：聚合完之后 `dense_rank() OVER (ORDER BY total DESC)` 截断，`total` 是
  `sum(abs(v)) OVER (PARTITION BY keys)`。换成两次往返（先查 top N 的键、再查它们的点）反而要多扫一遍表。
  截断了响应里 `truncated = true`，页面提示加过滤条件。
* 指标看板不是写死的面板列表，是**按语义约定翻译出来的**：面板定义在 `ui/src/lib/dashboards.ts`，
  每个面板给一串候选指标名，取第一个这个服务真的在报的，一个都没有就整块不显示。线上同时跑着
  两代 SDK，同一件事有两个名字（`http.server.request.duration` 秒 / `http.server.duration` 毫秒、
  `jvm.*` / `process.runtime.jvm.*`），标签名也跟着变（`http.response.status_code` /
  `http.status_code`），候选列表就是用来吃掉这个差异的；非 Java 的服务再退回 collector 的
  spanmetrics（`calls` / `duration`）。实测 `ai-crm` 拼出 20 个面板、`eci`（老 SDK）16 个、
  `job-center`（只有 HTTP + JVM）11 个。
* 图怎么画跟着数据的性质走，和 Cloudflare 控制台一套观感：**计数 / 速率画堆叠柱**
  （按状态码、按接口、按 GC 名堆起来，构成一眼看得出，和服务详情页的「请求量与错误」一致），
  **水位和分位数画折线**，只有一条线时线下填一层 12% 的淡色。图例可以点，点一下把某条线摘掉
  ——按接口分组时十几条挤在一起，只想看其中一两条。
* **画出来的每条线都得认得出来**。调色板只有 8 个能分辨的颜色（`SERIES_SLOTS`），第 9 条起
  全落到同一个灰上，画了也分不清谁是谁。所以：**堆叠柱**多取几条，第 9 条往后加总成一条灰色的
  「其它 N 条」（堆叠本来就是在看构成，加总是诚实的）；**折线**不能加总（几条延迟曲线加起来
  没有意义），干脆只取前 8 条，图例上写一句「还有更多」，要全看点标题去「全部指标」里拆。
  图例默认只占两行（同一排卡片才对得齐），装不下的收成「+N 条」——那是个**按钮**，点开就把
  剩下的全列出来，这张卡片变高。图上画了却没名字的线是不行的。
* **一块看板共用一根十字线**：鼠标停在任意一张图上，同屏所有图都在同一时刻画竖线（气泡只出现在
  鼠标那张图上）。「GC 那一下和延迟尖峰是不是同一时刻」不用来回对 x 轴。
* **图上横向拖一段就是缩小时间范围**（和日志页直方图同一个交互）：拖完整块看板按新范围重查，
  顶上「最慢的链路 / 出错的链路 / 错误日志 / 服务概览」几个入口带的也是这一段——指标上看到一个
  尖峰，两步就能跳到那一分钟的链路和日志。服务详情页那边也有回到指标看板的入口。
* 图上**丢掉最后一个不完整的桶**：时间范围的右端就是「现在」，最后那一格往往才过了几秒，
  速率和计数只统计了一小截。不丢的话每张图末尾都往下掉一截，图例的读数（最后一个值）也跟着
  偏小——看图的人会以为量掉下去了。
* 看板一行只排两列（一行两张宽图比三张窄图好读），面板数是奇数时最后一张跨满整行，不留半行
  空白；图例固定两行高度，同一排的卡片才对得齐；图例上只写**有区分度**的那部分标签
  （按接口看 P95 时每条线都带 `quantile=p95`，写出来占地方又没信息量）。
* 看板的面板**滚进视口才发查询**：一屏二十个面板一起查，会把后端的查询名额
  （`--max-concurrent-queries`，默认 16）一次占满，别人就得排队；集群也白扫了没人翻到的那些面板。
  一个面板的查询很轻（单服务单指标一小时，直方图分位 0.015 GB / 0.3 s，gauge 0.001 GB / 0.05 s）。
* 指标的累积量相减有个 26.x 的坑：`UInt64 - UInt64` 出来是 **Int64**，和另一个分支的 UInt64
  拼不出公共类型，`if` 会给一个 `Variant(Int64, UInt64)`，外面的 `sumForEach` 直接报 43
  （`Illegal type Variant(Array(UInt64), Array(Variant(Int64, UInt64)))`）。`toUInt64()` 要套在
  **分支里面**（`if(c >= p, toUInt64(c - p), c)`）——套在 `if` 外面也不行，`toUInt64` 不吃 Variant。
* 指标目录（`/api/metrics`）只扫**最近 6 小时**，哪怕页面选的是 30 天：它要读整段范围里的
  `metric_name` / `service_name`，而「有哪些指标」看最近几小时就够了。真有只在凌晨报一次的指标，
  把时间范围整个挪过去就看得见——响应里的 `from_ms` 是实际扫的窗口，页面上写着。
* 指标表的排序键是 `(service_name, metric_name, toDateTime(timestamp))`，`metric_name` 上还有
  bloom filter，所以指标页不像链路页那样强制先选服务：不给服务时靠索引跳 granule。
* 指标的标签过滤和链路页一套写法：子列标识符 `` attributes.`http.route` ``，值一律 `toString(...)` 后比较
  （同一个 key 在不同服务里可能是整数也可能是字符串，直接比会 `NO_COMMON_TYPE`）。
* exemplar 先在源行上 `notEmpty(exemplars.trace_id)` 挡掉，再 `ARRAY JOIN` 展开：反过来是把每行的
  空数组也展开一遍。按值从大到小取，慢的那几次排在最前面。
* 服务概览：`quantilesTDigest` 而不是默认的 `quantiles`（后者是 8192 个样本的水塘抽样，尾部分位最不准）。
* 直方图分桶：`intDiv(toUnixTimestamp64Milli(timestamp) - origin, width)`，原点是范围起点那天的本地零点。
  不用 `toStartOfInterval(..., INTERVAL n SECOND)`：它按 UTC 取整，6 小时一桶时边界落在北京时间 02 / 08 点。
* 老分区没有索引：`ADD INDEX` 只对新写入的 part 生效，按 trace id 查历史日志慢的话在库上
  `ALTER TABLE logs.app_log_local ON CLUSTER log MATERIALIZE INDEX idx_trace_id`（`otel_trace_local` 同理）。
  哪些 part 没索引看 `system.parts` 的 `secondary_indices_compressed_bytes`，为 0 就是没有。

## 部署

镜像由 CI 构建推送到 GHCR（打 `v*` tag 触发，见下面「发布」）。

```bash
# docker：指向线上库
OPDASH_CLICKHOUSE_URL=http://ck:8123 OPDASH_CLICKHOUSE_USER=opdash OPDASH_CLICKHOUSE_PASSWORD=xxx \
  docker compose up -d

# k8s：Deployment + Service + Ingress 自己按集群写（deploy/ 目录不入库，里面有内部域名和密钥），
# 环境变量照上面的配置表；Secret 里放 ClickHouse 密码、Keycloak client 密钥、session secret
kubectl -n logging port-forward svc/opdash 4880:4880     # 没配 Ingress 先本地看
```

日志跟随走 SSE 长连接（`/api/logs/tail`）：响应带 `X-Accel-Buffering: no` 且不压缩，nginx / ingress 一般不用改；
每 15 秒有一次保活注释，闲着也不会被空闲超时掐掉。要是中间还有别的代理，确认它没开响应缓冲、读超时大于 15 秒。

k8s 上 `OPDASH_MAX_READ_BYTES` 建议给个 100 GiB 左右的护栏，按集群规模调。readiness 探针打
`/api/health`（不认证，会真的 ping ClickHouse），库挂了会摘流量；liveness 用 tcpSocket 就行，库挂了重启进程没用。对外暴露务必配 Keycloak 登录（`OPDASH_OIDC_*`，
见上面「登录」）或至少 `OPDASH_BASIC_AUTH`：这个页面能翻全部线上日志。

### 发布

```bash
# 1. 改 Cargo.toml 的 version，CI 会校验它和 tag 一致
git commit -am "release v0.1.0" && git push origin main
# 2. tag 单独推，和分支挤在同一条 git push 里不触发构建
git tag v0.1.0 && git push origin v0.1.0
```

产出 `ghcr.io/easayliu/opdash:v0.1.0` 和 `:latest`。`.github/workflows/ci.yml` 在 push / PR 上跑
`pnpm build` → `cargo fmt --check` → `clippy -D warnings` → `cargo test`；`docker.yml` 构建前复用它作为闸门。

## 测试

```bash
cargo test                                   # 单元 + 假 ClickHouse 集成测试（不需要库）
OPDASH_E2E_CLICKHOUSE_URL=http://host:8123 OPDASH_E2E_CLICKHOUSE_USER=x OPDASH_E2E_CLICKHOUSE_PASSWORD=y \
  cargo test --test e2e_clickhouse -- --nocapture   # 对着真实库把每个接口跑一遍，只读
```

假 ClickHouse（`tests/support`）回放预设响应并记下收到的请求，断言的是发出去的 SQL、`param_*`、设置和
认证头。它验不了 ClickHouse 怎么解参数、JSON 子列怎么读、Distributed 上 `LIMIT BY` 的行为——这些在 E2E 里。

## API

全部 GET，返回 JSON；错误是 `{"error": "...", "kind": "bad_request|timeout|too_heavy|unavailable|internal"}`，
`timeout` / `too_heavy` 前端会提示缩小范围。时间入参统一 unix 毫秒；出参日志 `ts_ms`、span `start_us`（微秒）
+ `duration_ns`。每个响应带 `stats`（扫描行数 / 字节 / 耗时），页面上显示出来，让人知道这次查询贵不贵。

```text
GET /api/meta                 表结构、维度列、上限
GET /api/health               ping ClickHouse + 表结构状态，不认证
GET /api/auth/*               登录相关，见上面「登录」，不认证
    /api/saved                收藏的查询（GET / POST / PUT / DELETE），按账号区分，见上面「收藏查询」
GET /api/logs/search          ?from&to&q&regex&level&logger&thread&host&trace_id&span_id&<维度列>&order&limit&offset
GET /api/logs/histogram       同 search 的筛选参数
GET /api/logs/facets          ?field=level|logger|host|<维度列>&limit
GET /api/logs/context         ?host&file&ts&before&after
GET /api/logs/export          同 search，&format=csv|jsonl
GET /api/traces/search        ?from&to&service&span_name&kind&error_only&min_ms&max_ms&attr=k=v&rattr=k=v&sort=time|duration&limit&trace_id
GET /api/traces/{trace_id}                       瀑布图用的轻列，不含属性
GET /api/traces/{trace_id}/spans/{span_id}       ?at&service&name&ts   这一个 span 的属性 / events / links
GET /api/traces/values        ?field=service|span_name&service&kind=entry|client|all
GET /api/traces/attr_keys     ?service&scope=span|resource
GET /api/traces/attr_values   ?key&service&scope
GET /api/metrics              ?from&to&service          指标目录（只扫最近 6 小时，见下）
GET /api/metrics/query        ?from&to&metric&service&agg&field&by&attr=k=v&rattr=k=v&q&step&limit
                              agg / field 不给就按指标类型挑（直方图→分位数、counter→rate、
                              gauge→avg）；按类型必然查空的组合（如直方图 field=value）回 400
GET /api/metrics/labels       ?metric&column=attributes|resource_attributes
GET /api/metrics/label_values ?metric&key&column
GET /api/metrics/exemplars    ?metric&service&attr&limit
GET /api/bills/periods        库里有哪些账期（费用页拿它定默认区间）
GET /api/bills/summary        ?from=2026-04&to=2026-09&amount=payable|paid|original&provider&<维度>&q
                              账期是 YYYY-MM，一次最多 36 个；不给就是最近 6 个
GET /api/bills/daily          同上，按天（只问有日粒度的表）
GET /api/bills/breakdown      ?by=product|item|region|zone|account|instance|project|subscription|currency&limit
GET /api/bills/allocation     ?days=7   按归属规则摊到业务线，给出日均、预付费摊销与月度预估的两段
GET /api/bills/detail         ?provider=volcengine|alicloud&granularity=monthly|daily&limit&offset
GET /api/bills/export         同 detail，&format=csv|jsonl
POST /api/bills/sync          {provider, from, to, granularity, force, mode}  手动拉一次，转给 goscan
GET /api/bills/sync/{task_id} 这次拉取跑到哪了（事件流不可用时页面每 2 秒问一次）
GET /api/bills/sync/{task_id}/events   同上，SSE 推送：event: task 为任务状态，event: done 表示已结束
DELETE /api/bills/sync/{task_id}       停止同步：goscan 把当前这一趟写完再停，已结束的回 409
GET /api/bills/sync/running   ?provider   这朵云正在进行的同步（手动或 cron 发起），没有则 task 为 null
GET /api/errors               ?from&to&kind=entry|client|all&service&span_name   错误分组，默认 entry
GET /api/services             ?from&to&compare=day|week|prev&<维度列>
GET /api/services/operations          ?service=a&service=b&...   一次最多 24 个服务
GET /api/services/{name}/operations   ?from&to&kind=entry|client&compare=day|week|prev|none
GET /api/services/{name}/timeseries   ?from&to&span_name&compare=day|week|prev|none
```

## 还没做

* 日志分页是 `OFFSET`，最多翻到第 10000 条（`--max-offset`）；再往后让用户缩小范围。keyset 分页需要一个
  行内唯一键，表里没有。
* 上下文按 `host + file` 取，容器重启换了文件（`0.log` → `1.log`）就断了；有 `pod` 列时可以在日志页按 pod 筛。
* 指标页只画单个指标，没有多指标运算（`a / b` 求成功率这种）、没有存下来的面板、没有 PromQL。
  真要表达式的话得先有一层解析，现在的 `agg + by + filter` 够看曲线。
* 指数直方图（`ExponentialHistogram`）不算分位数：它的桶是 `base^i` 编码的，没有 `explicit_bounds`，
  要另写一套换算。Summary 的分位数是采集端算好的，多条时间线合不起来，同样只看 count / sum。
* 没有告警、不写库、没有用户系统。
