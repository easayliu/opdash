# opdash

opdash 是链路、日志、指标与云账单的查询平台，面向内部业务开发的故障排查。它是单个二进制文件，对数据库只读，不依赖其他服务。

* 输入一个 trace id，查看整条链路以及这次请求的全部日志；
* 按服务、接口查找慢请求与错误请求，按关键字检索日志、查看堆栈与上下文；
* 查看各服务的错误率与 P95，按标签绘制指标曲线，从指标上的 exemplar 直接跳到对应的那次请求；
* 查看云上每月的花费及其构成；
* 通过 MCP 接入 Claude Code 等 AI 助手，由助手自行完成上述排查。

所有数据都位于同一个 ClickHouse 库中：

| 数据 | 表 | 写入方 | 是否必需 |
| --- | --- | --- | --- |
| 日志 | `logs.app_log` | [logpipe](../log) | 必需 |
| 链路 | `logs.otel_trace` | [tracepipe](../trace) | 必需 |
| 指标 | `logs.otel_metric` | [metricpipe](../metric) | 可选，表不存在时不显示指标页 |
| 云账单（火山引擎、阿里云） | `volcengine_bill` 等三张 | [goscan](../goscan)，按账期同步 | 可选，表不存在时不显示费用页 |

前三类数据由应用自身产生，账单则由 goscan 从云厂商接口拉取，所以费用页按**账期**（如 `2026-09`）查询，而非顶栏的时间范围。

```text
  浏览器 ──▶ /api/*  axum ── 参数化 SQL、readonly=2 ──▶ ClickHouse HTTP（单机或 Distributed）
     ▲          │
     └── /  内嵌 SPA（rust-embed，ui/dist）
  AI 助手 ──▶ /mcp   MCP 工具，在进程内复用同一套 /api/*
```

设计与实现的细节另见：

* [docs/design.md](docs/design.md)：页面设计说明，包括首页如何判定异常、变化榜的排序依据、看板与图表的绘制规则、页面跳转的约定；
* [docs/queries.md](docs/queries.md)：查询实现与实测，包括日志、链路、指标各接口的 SQL 为何如此编写，以及对应的线上测量数据；
* [docs/bills.md](docs/bills.md)：云账单，包括查询时去重、表名识别、表结构演进、成本归属规则与手动同步。

## 快速开始

```bash
# 构建前端
cd ui && pnpm install && pnpm build && cd ..
# 构建并启动后端（前端产物会编译进二进制）
cargo run --release -- --clickhouse-url http://127.0.0.1:8123 --clickhouse-user default
# 浏览器打开 http://127.0.0.1:4880
```

未构建前端也可以 `cargo build`（`ui/dist/.gitkeep` 占位），此时首页会提示先构建前端，API 不受影响。

前端开发时执行 `cd ui && pnpm dev`，vite 会把 `/api` 代理到本机 4880 端口的后端。

## 页面

| 路径 | 用途 |
| --- | --- |
| `/services`（首页） | **服务总览**：每个服务一张卡片，展示请求量、错误率、P95 及其与对比时段（默认为昨天同时段）的变化，附迷你趋势。错误率 ≥ 1% 或 P95 达到 1.5 倍以上的标黄，≥ 5% 或 3 倍以上的标红并置顶。可切换为表格视图 |
| `/services/:name` | **服务详情**：与对比时段相比，哪些接口发生了变化（变化榜，以及每列都带变化的接口表）；请求量、错误与延迟分位数的趋势，均叠加对比时段 |
| `/logs` | **日志检索**：关键字或正则、级别、服务 / namespace / pod 等维度、logger、thread；在直方图上拖选以缩小范围；展开全文、查看上下文；实时跟随（SSE 推送，秒级延迟，可切换为终端模式正序输出并自动滚动）；导出 CSV / JSONL |
| `/traces` | **链路检索**：按服务、接口、span 类型、是否出错、耗时区间、属性 `key=value` 筛选；耗时 × 时间散点图 |
| `/traces/:trace_id` | **链路详情**：瀑布图；span 的属性、resource、事件（含异常堆栈）与 links；这条链路的日志 |
| `/errors` | **错误分组**：把出错的 span 按「同一种报错」（异常类 + 消息 + 接口）归组，每组显示次数、影响的链路数与最后一次出现的时间；展开后可查看堆栈与样本链路，并跳转到样本链路、该接口的全部错误链路或该服务的错误日志 |
| `/metrics` | **指标**，分两个页签。**服务看板**：选择一个服务后，按 OTel 语义约定自动生成 HTTP、JVM、连接池、Kafka、Go 等面板；点击 Top 接口 / 下游 / topic 表中的一行，整页按其过滤；进程重启与新 pod 启动以虚线标出。**全部指标**：列出全部指标名，自行选择聚合方式、分组与过滤；图上的圆点是 exemplar，点击即打开对应请求的链路 |
| `/cost` | **云账单**：见下文「费用页」 |

**所有筛选条件都保存在 URL 中**，把链接发给同事，对方看到的就是同一个视图。相对范围（`range=1h`）在打开时按当时的时间计算，绝对范围（`from=&to=`）始终指向同一段时间。

顶栏提供：时间范围（相对或绝对）、直达框（粘贴 trace id 直接打开链路，16 位十六进制视为 span id，其余内容作为关键字检索日志）、收藏、深浅色切换。

### 页面之间的跳转

各页面之间可以相互跳转，**跳转后看到的是同一个服务、同一段时间**：

| 从 | 到 | 入口 |
| --- | --- | --- |
| 日志行 | 链路详情 | 行尾的 trace id；16 位的 span id 跳转到「这个 span 的全部日志」 |
| 日志页（已筛选服务） | 指标看板 / 最慢的链路 / 错误分组 / 出错的链路 / 服务概览 | 筛选栏下方 |
| 链路列表（已筛选服务） | 指标看板 / 错误日志 / 错误分组 / 服务概览 | 筛选栏下方 |
| 链路详情 | 服务指标 / 服务概览 / 这条链路的全部日志 | 页面顶部；指标的时间窗以这条链路为中心前后各 15 分钟 |
| 链路详情中的某个 span | 该服务的指标 / 只看这个 span 的日志 | 右侧 span 面板 |
| 指标看板 | 最慢的链路 / 错误分组 / 出错的链路 / 错误日志 / 服务概览 | 页面顶部；在图上拖选之后，携带的是拖选出的时间窗 |
| 指标图上的一个点 | 这一格的链路 / 不低于该值的链路 / 这一格的报错 / 这一格的错误日志 | 点击图上任意一点 |
| 指标图上的 exemplar | 那一次请求的链路详情 | 图上的圆点 |
| 服务概览 | 日志 / 链路 / 指标 | 页面顶部 |
| 首页的异常卡片 | 该服务正在报的错误 | 卡片上「⚠ 异常类: 消息 N 次」一行，点击后展开对应的错误分组 |
| 错误分组中的一行 | 样本链路（打开时选中报错的 span）/ 该接口的全部错误链路 / 该服务的错误日志 | 展开之后 |
| 服务详情 | 错误分组 | 页面顶部；选中某个接口时只看该接口的错误 |

**点击指标图上的一个点**时，弹层中的链接会同时携带这一格的时间窗、所点曲线的标签（已翻译为目标页的筛选条件）以及该点的数值；弹层还会查询一次这一格有没有报错，没有则直接说明「尖峰来自耗时而非错误」。

从一个时刻（一条日志、一个 span、一个 exemplar）跳到按时间段查看的页面时，时间窗取该时刻前后各 15 分钟。从链路详情返回时，左上角的返回按钮指向实际的来源页（「← 错误」「← 日志」）；直接粘贴链接打开时没有来源，不显示返回按钮。设计考虑见 [docs/design.md](docs/design.md#页面之间的跳转)。

### 日志检索的关键字语法

语法参照 Kibana 与 Datadog：

* 空格表示 AND，`a OR b` 表示或，`-词` 或 `NOT 词` 表示排除，`"带 空格"` 表示短语，`( )` 用于分组；
* 优先级为 NOT > AND > OR，`AND` / `OR` / `NOT` 必须全部大写才是操作符；
* 不报语法错误：悬空的操作符与多余的括号直接忽略，词内成对的括号（如 `getUser(id)`）按字面搜索；
* 匹配不区分大小写，按子串匹配。例外是不少于 16 位的纯字母数字 token（trace id、msgId 等）：它们按整词匹配并使用 token 索引，速度快得多，但搜索 id 的前半段不再命中，页面会标注「按整词匹配 · 已走索引」。需要搜索片段时请使用正则模式。

不能使用索引的关键字需要扫描时间范围内的全部日志，一小时约 10 GB。时间范围较长时，请先筛选服务或缩小范围。实现细节见 [docs/queries.md](docs/queries.md#日志)。

### 收藏查询

在日志、链路、错误分组、指标、服务总览、服务详情这些列表与看板页面设好条件后，点击顶栏的书签图标即可收藏，名称可自定义（默认按条件生成）。收藏的是查询条件，**不含时间范围**：打开一条收藏时，使用的是顶栏当前的时间范围。链路详情对应一条具体的 trace，30 天后即过期，因此不能收藏。

**收藏按账号区分**：归在登录账号名下（OIDC 的 `preferred_username` 或 Basic 认证的用户名，与 API key 相同），列表只显示本人的收藏，换一台机器登录依然存在；使用本人的 API key 也能读写。未开启认证的部署没有「用户」，所有人共用一份。同一账号、同一地址只保存一条（重复收藏返回 409），每人最多 200 条。数据存放在 `--saved-query-file` 指定的 JSON 文件中，存储方式与 API key 文件相同，见下文「API key」。

```text
GET    /api/saved        我的收藏，新的在前
POST   /api/saved        {"name": "订单超时", "path": "/logs", "query": "q=timeout&level=ERROR"}；name 可省略
PUT    /api/saved/{id}   改名 {"name"}，或更换地址 {"path", "query"}（两者一起给出）
DELETE /api/saved/{id}   删除；他人的收藏与不存在的收藏一样返回 404
```

### 费用页

费用页展示 goscan 同步的火山引擎与阿里云账单，两朵云合并在同一张图上，金额口径可在「应付 / 现金 / 原价」之间切换。**本页按账期查询，顶栏的时间范围在此隐藏。**

* **账单视图**：按账期的花费及环比、按天的曲线；按产品、计费项、地域、账号、实例、项目排行（点击一行即加为筛选条件）；明细表（表头可按维度筛选，同一维度可多选）与 CSV 导出。
* **分析视图**：按成本归属规则把费用分摊到各业务线，给出日均、预付费摊销与月度预估。规则由 `--bill-alloc` 指定的 TOML 文件提供，示例见 [`examples/bill-alloc.toml`](examples/bill-alloc.toml)，规则语义见 [docs/bills.md](docs/bills.md#成本归属)。不配置规则时，分析视图仍给出按产品的日均与月度预估。
* **同步账单**：配置了 `--goscan-url` 时，右上角有「同步账单」按钮，可以当场补拉一段账期。同一朵云同一时刻只能有一个同步任务，已有任务在进行（包括 goscan 定时发起的）时，页面会直接接续显示其进度；关闭窗口不会中断任务。同步可以中途停止，但 goscan 会写完当前这一趟（一个账期 × 一种粒度）再停，通常需要数秒至数分钟。未配置 `--goscan-url` 时，可以等待 goscan 的定时任务，或在集群中执行 `goscan --once config.yaml --provider alicloud --start 2026-01 --end 2026-06` 补拉。

goscan 的账单表在集群上曾经会重复计入金额，opdash 默认在查询中再去重一次（`--bill-dedupe=group`）。原因与表结构的调整见 [docs/bills.md](docs/bills.md#查询时去重)。

## 配置

所有参数都有对应的 `OPDASH_*` 环境变量，在容器中直接使用环境变量即可。

| 参数 | 环境变量 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--bind` | `OPDASH_BIND` | `0.0.0.0:4880` | 监听地址 |
| `--clickhouse-url` | `OPDASH_CLICKHOUSE_URL` | `http://127.0.0.1:8123` | ClickHouse HTTP 地址 |
| `--clickhouse-user` / `--clickhouse-password` | `OPDASH_CLICKHOUSE_USER` / `OPDASH_CLICKHOUSE_PASSWORD` | `default` / 空 | 建议为 opdash 建立只读账号，并在 profile 中固定 `readonly=2` 与 `max_execution_time` |
| `--database` / `--log-table` / `--trace-table` | `OPDASH_DATABASE` / `OPDASH_LOG_TABLE` / `OPDASH_TRACE_TABLE` | `logs` / `app_log` / `otel_trace` | 与采集端的 sink 配置保持一致；集群上填写 Distributed 表名 |
| `--metric-table` | `OPDASH_METRIC_TABLE` | `otel_metric` | metricpipe 的表。**可以不存在**，此时不显示指标页，启动日志中会说明原因 |
| `--volcengine-bill-table` / `--alicloud-monthly-table` / `--alicloud-daily-table` | `OPDASH_VOLCENGINE_BILL_TABLE` / `OPDASH_ALICLOUD_MONTHLY_TABLE` / `OPDASH_ALICLOUD_DAILY_TABLE` | `volcengine_bill` / `alicloud_bill_monthly` / `alicloud_bill_daily` | goscan 的三张账单表，**各自可以不存在**（只接入一朵云是常见情形），一张都没有时不显示费用页。**通常无须配置**：goscan 历次改名留下的旧表名也能自动识别，见 [docs/bills.md](docs/bills.md#表名的识别) |
| `--bill-dedupe` | `OPDASH_BILL_DEDUPE` | `group` | 账单查询的去重方式：`group` 在查询中分组去重（默认，跨分片也正确）；`final` 给表加 `FINAL`，会丢失同键的并列行，在表按 goscan v0.5 的结构重建之前不要使用；`off` 不去重，重复拉取过的账期金额会翻倍 |
| `--bill-alloc` | `OPDASH_BILL_ALLOC` | 不配置 | 成本归属规则文件（TOML），见上文「费用页」。**文件有误时进程直接退出**，不会静默降级 |
| `--goscan-url` | `OPDASH_GOSCAN_URL` | 不配置 | goscan 的地址（如 `http://goscan.logging.svc.cluster.local:8080`）。配置后费用页才有「同步账单」按钮。**这是 opdash 唯一会向外发出改变状态的请求的功能**，对 ClickHouse 依旧只读 |
| `--goscan-timeout` | `OPDASH_GOSCAN_TIMEOUT` | `10s` | 调用 goscan 接口的超时。触发同步只是登记一个后台任务，很快返回；实际的拉取在 goscan 侧进行，与此超时无关 |
| `--datasources` | `OPDASH_DATASOURCES` | 不配置 | 业务数据源配置文件（TOML），供 MCP 排障时直连业务库，见下文「直连业务库」。不配置时 MCP 中没有 `db_*` 工具。**文件有误时进程直接退出** |
| `--env` | `OPDASH_ENV` | 不配置 | 这套 opdash 所属的环境（如 `生产`、`测试`），写入 MCP 握手的标题、使用说明的首句以及 `get_meta`、`db_sources` 的返回，同时接入多套 opdash 时模型据此区分数据来源。不配置时只说明访问所用的域名 |
| `--mcp-name` | `OPDASH_MCP_NAME` | 由 `--env` 推导 | 页面「API key」对话框中生成接入命令所用的 MCP 服务名（如 `claude mcp add … opdash-prod`），同时接入多套 opdash 时必须互不相同。未配置时，若 `--env` 只含字母、数字、下划线、短横线则取 `opdash-<env>`，否则（如 `生产`）取 `opdash` |
| `--timezone` | `OPDASH_TIMEZONE` | `Asia/Shanghai` | 直方图分桶对齐所用的时区，应与表中 `timestamp` 列的时区一致 |
| `--query-timeout` | `OPDASH_QUERY_TIMEOUT` | `30s` | 传给 ClickHouse 的 `max_execution_time` |
| `--max-range` | `OPDASH_MAX_RANGE` | `31d` | 允许查询的最大时间跨度 |
| `--max-rows` / `--max-offset` | `OPDASH_MAX_ROWS` / `OPDASH_MAX_OFFSET` | `1000` / `10000` | 日志每页的最大行数；最多能翻到第几条 |
| `--export-max-rows` | `OPDASH_EXPORT_MAX_ROWS` | `50000` | 导出的行数上限 |
| `--max-message-chars` | `OPDASH_MAX_MESSAGE_CHARS` | `16384` | 列表、上下文、跟随中每条日志 `message` 的最大字符数，超出部分截断并标注，**导出不受此限制**。原因见 [docs/queries.md](docs/queries.md#一条日志能有多大) |
| `--max-trace-spans` | `OPDASH_MAX_TRACE_SPANS` | `5000` | 一条链路最多取多少个 span，超出时标记为截断 |
| `--max-read-bytes` / `--max-read-rows` | `OPDASH_MAX_READ_BYTES` / `OPDASH_MAX_READ_ROWS` | `0`（不限） | 单条查询的读量护栏（`max_bytes_to_read` / `max_rows_to_read`），超出时立即报错并提示缩小范围，比等待超时体验更好。**按整条查询的总读量计算**（发起端汇总各分片的进度后检查），而非按分片；要按分片限制需使用 `max_bytes_to_read_leaf` |
| `--max-concurrent-queries` | `OPDASH_MAX_CONCURRENT_QUERIES` | `16` | 同时在库上执行的查询数上限，排队 10 秒仍无名额时返回 503 |
| `--schema-refresh` | `OPDASH_SCHEMA_REFRESH` | `5m` | 重新读取 `system.columns` 的间隔 |
| `--tail-interval` | `OPDASH_TAIL_INTERVAL` | `1s` | 日志跟随时服务端查询增量的间隔；每条跟随连接都以此频率执行一条轻量查询 |
| `--max-tail-streams` | `OPDASH_MAX_TAIL_STREAMS` | `8` | 同时存在的跟随连接数上限，已满时返回 503 |
| `--basic-auth` | `OPDASH_BASIC_AUTH` | 不认证 | `user:password`，配置后要求浏览器登录，`/api/health` 除外。可与 OIDC 同时开启，供脚本与 curl 使用 |
| `--oidc-issuer` / `--oidc-client-id` | `OPDASH_OIDC_ISSUER` / `OPDASH_OIDC_CLIENT_ID` | 不认证 | Keycloak realm 地址（如 `https://sso.example.com/realms/ops`）与 client ID，两者同时配置即启用 Keycloak 登录，见下文「登录」 |
| `--oidc-client-secret` | `OPDASH_OIDC_CLIENT_SECRET` | 无 | client 密钥；public client 无须配置（使用 PKCE） |
| `--oidc-required-role` | `OPDASH_OIDC_REQUIRED_ROLE` | 无 | 要求用户具有该角色（realm 角色或本 client 的角色）才允许访问；不配置则登录即可访问 |
| `--oidc-scopes` | `OPDASH_OIDC_SCOPES` | `openid profile email` | 授权请求的 scope |
| `--public-url` | `OPDASH_PUBLIC_URL` | 由请求头推导 | 浏览器访问 opdash 的地址，用于拼接 OIDC 回调地址。在 Ingress 之后按 `X-Forwarded-Proto` / `Host` 推导通常是正确的；本地 vite 开发时配置为 `http://localhost:5173` |
| `--session-ttl` | `OPDASH_SESSION_TTL` | `12h` | 登录会话的有效期，到期后重新跳转 Keycloak 登录 |
| `--session-secret` | `OPDASH_SESSION_SECRET` | 随机 | 会话 cookie 的签名密钥。不配置时每次启动随机生成（重启后需重新登录）；多副本部署必须配置同一个值 |
| `--api-key-file` | `OPDASH_API_KEY_FILE` | `api-keys.json` | 用户生成的 API key 的存储文件，只存哈希。**容器中须把所在目录挂载为卷**（镜像的工作目录是 `/var/lib/opdash`），否则重启后丢失；多副本共享同一个文件 |
| `--api-key-ttl` | `OPDASH_API_KEY_TTL` | `90d` | API key 的最长有效期，生成时可选择更短的期限 |
| `--saved-query-file` | `OPDASH_SAVED_QUERY_FILE` | `saved-queries.json` | 收藏查询的存储文件，与 API key 文件一样须挂载为卷、多副本共享 |

设置 `RUST_LOG=opdash=debug` 可以在日志中看到每条 SQL 及其绑定的参数。

### 表结构要求

* **固定列在启动时校验**：程序写死了 logpipe 的 9 列、tracepipe 的 22 列、metricpipe 的 27 列，启动时读取 `system.columns` 进行校验，日志表或 span 表缺列时在 `/api/health` 中报告。
* **固定列之外的字符串列自动成为筛选维度**：k8s 元数据（`service_name`、`namespace`、`pod`、`container`、`stream`）以及采集端 `fields` 配置中加入的静态列（`cluster`、`env` 等）无须修改 opdash，`/api/meta` 的 `dimensions` 中有什么，页面就显示什么筛选项。新增的列在 5 分钟内自动识别（`--schema-refresh`）。
* **span 表的四个属性列必须是 `JSON` 类型**（tracepipe v0.2.0 起，要求 ClickHouse 25.3+）。v0.1 的 `Map` 表不兼容，`/api/health` 会指出哪一列是 Map，请按 tracepipe README 重建该表。
* **指标表缺失不视为错误**：表不存在、缺少 metricpipe 的固定列或属性列不是 JSON 时，`/api/meta` 的 `metrics` 为 `null`（原因写在 `metrics_note` 中，启动日志中也有说明），指标页签不显示，日志与链路照常使用。若强制要求三张表齐全，尚未接入指标的环境将连日志都无法查看。

## 登录：对接 Keycloak

opdash 能查看全部线上日志，对外暴露时必须开启认证。有两种方式：`--basic-auth` 使用一组共享密码，适合内网或临时使用；OIDC 跳转 Keycloak 登录为推荐方式，登录者的身份会记录在日志中，人员离职时回收账号即可。两种方式可以同时开启，Basic 认证留给脚本与 curl 使用。

在 Keycloak 中创建 client（realm 不限，下文以 `ops` 为例）：

1. Clients → Create client：类型选 OpenID Connect，Client ID 填 `opdash`。
2. Capability config：Client authentication 选 **On**（以获得 client secret；不开启也可以，opdash 使用 PKCE），勾选 Standard flow，其余 flow 均不需要。
3. Login settings：Valid redirect URIs 填 `https://opdash.example.com/api/auth/callback`；Valid post logout redirect URIs 填 `https://opdash.example.com/*`（用于退出后跳回）。
4. 可选：如需限制访问人员，建立一个 realm 角色（如 `opdash-viewer`）并分配给相应的用户或组，opdash 配置 `--oidc-required-role opdash-viewer`。client 角色同样可用（在 client 的 Roles 中建立，token 中位于 `resource_access.opdash.roles`）。opdash 会同时从 id_token 与 access_token 中查找角色，无须修改 Keycloak 默认的映射器。

然后配置 opdash：

```bash
OPDASH_OIDC_ISSUER=https://sso.example.com/realms/ops   # 须与 Keycloak 签入 token 的 iss 完全一致
OPDASH_OIDC_CLIENT_ID=opdash
OPDASH_OIDC_CLIENT_SECRET=xxxx                           # Credentials 页签中的 Client secret
OPDASH_OIDC_REQUIRED_ROLE=opdash-viewer                  # 可选
OPDASH_SESSION_SECRET=$(openssl rand -hex 32)            # 可选；多副本部署必须配置
```

登录流程是标准的授权码 + PKCE，全部在后端完成：浏览器打开任意页面 → 没有会话时 302 跳转到 Keycloak → 登录后回到 `/api/auth/callback` → opdash 用 code 换取 token，校验 id_token 的 `iss`、`aud`、`exp`、`nonce`，把用户名与邮箱签入一个 HttpOnly cookie（HMAC 签名，服务端不保存会话）→ 跳回原先要访问的页面。id_token 不验签：它是 opdash 通过 TLS 直连 token 端点取得的，传输链路本身已证明了签发方（OIDC Core 3.1.3.7 允许这种做法），因此 **issuer 必须是 https**。顶栏右侧显示用户名，退出时会一并结束 Keycloak 的 SSO 会话。

**页面显示的名字**优先取 `name` claim：Keycloak 中填写了 First name / Last name 时，它就是用户的中文姓名，比 `preferred_username`（登录用的拼音账号）更易辨认。没有 `name` 时用 `given_name` + `family_name` 拼接，再没有才退回账号名与邮箱。Keycloak 用空格拼接这两栏（中文习惯是 First name 填姓、Last name 填名，拼出来中间多一个空格），**全部是汉字时这个空格会被去掉**，英文名（如 `Jane Doe`）保持原样。client 的 scope 保留 `profile`（默认即有），这几个 claim 就会进入 id_token；两栏都未填写的用户仍显示账号名。

**识别用户始终依据账号名**（`preferred_username`）：API key 与收藏都归在账号名下，日志中记录的也是账号名，所以在 IdP 中修改显示名不会导致任何人的 key 或收藏「消失」。名字在登录时写入会话 cookie，修改后需要**退出并重新登录**（或等待 `--session-ttl` 到期）才会更新。

```text
GET  /api/auth/me        登录方式与当前用户；未登录也返回 200（前端据此跳转登录）
GET  /api/auth/login     ?next=/logs   生成登录票据，跳转 Keycloak
GET  /api/auth/callback  Keycloak 回调地址
GET  /api/auth/logout    清除会话，跳转 Keycloak 登出后返回首页
```

### API key

Claude Code 这类 MCP 客户端与 curl 无法完成浏览器登录，所以登录用户可以通过页面右上角的钥匙图标**为自己生成 API key、查看已有的 key、随时吊销**。之后在请求中带上 `Authorization: Bearer opdash_…` 即可访问任何接口，包括 `/mcp`。key 代表签发它的用户，权限与该用户登录后相同（opdash 只有「查看」一种权限）。`/api/auth/me` 会返回请求的身份类型（`session` / `basic` / `api_key`），日志中记录 key 的 id 与名称（不记录 key 本身）。

```text
GET    /api/auth/keys        我的 key：名称、前缀、创建 / 到期 / 最近使用时间；不含 key 本身
POST   /api/auth/keys        签发一个，body 可选 {"name": "claude-code", "ttl": "30d"}；key 只在本次响应中出现
DELETE /api/auth/keys/{id}   吊销，立即失效
```

* **格式与存储**：key 的格式为 `opdash_<12 位十六进制 id>.<32 位随机串>`。服务端只保存随机串的 SHA-256（`--api-key-file`，一个几 KB 的 JSON 文件），认证时按 id 找到对应记录、以常量时间比较哈希并检查是否过期；即使文件泄露，也无法据此伪造 key。页面上列出的也只有前缀。`last_used_at` 每分钟落盘一次，不会让每个 MCP 请求都写磁盘。
* **只能管理自己的 key**：列表只显示本人的 key，吊销他人的 key 与吊销不存在的 key 一样返回 404，不给探测 id 的机会。
* **不能用 key 管理 key**：用 API key 调用上述三个接口一律返回 403，泄露的 key 既不能为自己续期，也不能删除其他 key。
* **有效期**：默认最长 90 天（`--api-key-ttl`），页面上可选 7 / 30 / 90 天。过期的 key 在列表中保留 7 天（标注「已过期」），之后从文件中清除。
* **无须审批**：登录用户即可为自己签发。如需限制谁能访问 opdash，请使用 `--oidc-required-role`。
* 未开启认证的部署不需要 key，钥匙按钮不显示，相关接口返回 400 并说明原因。

**为什么存文件而不是 ClickHouse**：opdash 对数据库只读（每条查询带 `readonly=2`，并推荐使用只读账号），为几行 key 增加一条写库路径、再处理集群上的建表，得不偿失。文件采用「写临时文件再 rename」的方式更新，掉电也不会留下半个文件。**容器中须把所在目录挂载为卷**：镜像的工作目录是 `/var/lib/opdash`，`docker-compose.yml` 已挂载一个 named volume，k8s 中给一个几 MB 的 PVC 即可。多副本也可以共享同一个卷：每次使用前都会检查文件的 mtime，其他副本修改后即重新读取。收藏查询的文件与此相同。

## 给 AI 用：MCP

opdash 同时提供一个 [MCP](https://modelcontextprotocol.io)（Model Context Protocol）端点 `POST /mcp`。Claude Code、Codex、Cursor 等 AI 助手接入之后，面对「昨天下午 order 服务为什么慢」这类问题，会自行完成排查：先在服务总览中找出异常的服务，再定位到接口，查看错误分组，取样本链路查看瀑布图与异常堆栈，再检索对应的日志。使用者只需提出问题。

### 接入

先在页面右上角的钥匙图标中为自己生成一个 API key（见上文「API key」），页面会同时生成接入命令：

```bash
# Claude Code：Streamable HTTP 传输，API key 放在请求头中
# 同名服务已存在时 add 会报 already exists，所以先执行 remove：未接入过时它只在 stderr 提示找不到，
# 不影响后面的命令；已接入过则相当于换成新的 key
claude mcp remove opdash 2>/dev/null
claude mcp add --transport http opdash https://opdash.example.com/mcp \
  --header "Authorization: Bearer opdash_…"

# 验证握手（无须客户端）
curl -s -H "Authorization: Bearer opdash_…" https://opdash.example.com/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
```

Codex 不通过命令行添加远程服务，而是写入 `~/.codex/config.toml`（项目级为 `.codex/config.toml`）；已有 `[mcp_servers.opdash]` 时替换整段：

```toml
[mcp_servers.opdash]
url = "https://opdash.example.com/mcp"
http_headers = { Authorization = "Bearer opdash_…" }
# 不希望把 key 写入配置文件（例如该文件会提交到版本库）时，改为从环境变量读取：
# bearer_token_env_var = "OPDASH_API_KEY"
```

其他客户端（Cursor、Claude Desktop 或自行编写的客户端）填写地址 `https://opdash.example.com/mcp` 与请求头 `Authorization: Bearer <key>` 即可，服务端是标准的 Streamable HTTP，没有任何客户端专属的约定。更换 key（吊销后重签、到期后重签）时只需替换 key，地址等其余配置不变。只支持 stdio 的旧客户端可以用 `npx mcp-remote https://opdash.example.com/mcp --header "Authorization: Bearer opdash_…"` 桥接。

### 同时接入多个环境

生产、测试等环境各部署一套 opdash、使用不同域名时，需要同时做到以下两点，模型才不会把测试环境的数据当作生产环境的数据：

1. **服务端声明环境**：为每套 opdash 配置 `--env`（`OPDASH_ENV=生产`、`OPDASH_ENV=测试`）。环境名会写入 MCP 握手的 `serverInfo.title` 与使用说明的首句（「【环境：生产】本 MCP 连接的是生产环境的 opdash（opdash.example.com）……」），`get_meta` 与 `db_sources` 的返回中也带有 `env`。未配置时，使用说明只写明访问所用的域名。
2. **客户端按环境命名**：每套使用不同的服务名，工具名会带上这个前缀（`mcp__opdash-prod__search_logs` 与 `mcp__opdash-uat__search_logs`），在对话中指定环境也更方便。为每套配置 `--mcp-name`（`OPDASH_MCP_NAME=opdash-prod`），页面「API key」对话框生成的命令就会使用这个名称；若名称相同，命令中的 `remove` 会把先接入的那一套删除。手动接入的写法如下：

```bash
claude mcp add --transport http opdash-prod https://opdash.example.com/mcp \
  --header "Authorization: Bearer opdash_…"        # 生产环境签发的 key
claude mcp add --transport http opdash-uat https://opdash-uat.example.com/mcp \
  --header "Authorization: Bearer opdash_…"        # UAT 环境签发的 key
```

各套 opdash 的 API key 互不通用，需要分别在各自的页面上生成。

### 工具

| 工具 | 用途 | 对应的页面 / 接口 |
| --- | --- | --- |
| `get_meta` | 版本、时区、当前时间、各表可筛选的维度列、指标表是否启用 | `/api/meta` |
| `list_services` | 时间范围内有 span 的服务名 | `/api/traces/values` |
| `service_overview` | 每个服务的请求量、错误率、P95 及其与对比时段的变化，`health` 字段按首页的阈值给出 red / yellow / ok，异常的排在前面 | 首页 |
| `service_operations` | 一个服务的接口表，每列带对比时段的变化，`gone` / `new` 标出消失与新出现的接口；可按 P95 涨幅、错误增量排序 | `/services/:name` |
| `service_timeseries` | 请求量、错误、P50 / P95 / P99 随时间的曲线，叠加对比时段 | `/services/:name` |
| `error_groups` | 出错的 span 按「同一种报错」归组，附样本链路 | `/errors` |
| `search_traces` | 按服务、接口、span 类型、是否出错、耗时区间、属性筛选链路 | `/traces` |
| `get_trace` | 一条链路的全部 span，带层级 `depth` 与相对开始时间 `offset_ms`；span 过多时保留全部出错的与最慢的；`include_logs` 可同时取回日志 | `/traces/:trace_id` |
| `get_span` | 一个 span 的属性、resource、events（含异常堆栈）与 links | 链路详情右侧面板 |
| `search_logs` | 按关键字、正则、级别、维度列、trace_id、span_id 检索日志（默认不统计总数，需要时使用 `log_histogram` 或 `count=true`） | `/logs` |
| `log_histogram` | 日志条数按时间、按级别的分布，用于回答「错误从几点开始」 | 日志页直方图 |
| `log_facets` | 若干维度列各自最常见的取值，用于回答「报错集中在哪个 pod」 | 日志页下拉框 |
| `log_context` | 某条日志前后的若干行 | 日志页上下文 |
| `list_attrs` | span 或指标上有哪些属性名、某个属性有哪些取值，供 `attr` / `by` 参数参考，避免猜测 | 链路页、指标页的属性下拉框 |
| `list_metrics` / `query_metric` | 指标目录；按 agg、field、by 与过滤条件查询一个指标的时间序列，未给出 agg / field 时按指标类型自动选择 | `/metrics` 全部指标 |
| `metric_exemplars` | 指标点上关联的 trace id，把 P99 尖峰换成一条具体的链路，交给 `get_trace` | 图上的圆点 |
| `metric_events` | 进程重启、新 pod 启动的时刻 | 看板上的虚线 |
| `cost_summary` | 云账单按账期（或按天）的花费，分云给出，用于回答「这几个月花了多少」「本月比上月是否上涨」 | `/cost` 顶部的图 |
| `cost_breakdown` | 按产品、地域、账号、实例排行，两朵云合并排序，用于回答「多出来的钱花在哪个产品上」 | `/cost` 排行 |
| `cost_detail` | 账单明细，每行一个计费项 | `/cost` 明细表 |
| `cost_allocation` | 按归属规则（`--bill-alloc`）把账单分摊到业务线：各业务线金额、日均、月度预估、产品构成与未归属金额；未配置规则时给出按产品的日均与预估 | `/cost` 分析视图 |
| `cost_compare` | 按产品比较两段等长日期的花费（与前一日、与上周同日、近 N 天与前 N 天），按变化额排序，用于回答「昨天为什么贵了」 | `/cost` 产品费用对比 |
| `trace_db_calls` | 一条链路中的全部数据库调用：语句、耗时、库名、对端地址；JDBC 参数齐全时给出代入参数后的语句，标出对应的数据源，并列出重复执行的语句，便于识别 N+1 查询 | `/api/traces/{trace_id}/db` |
| `db_calls_top` | 一段时间内的数据库调用按语句汇总：次数、错误数、P50 / P95 / 最大 / 总耗时，附最慢一次的样本链路，用于回答「这个服务最耗时的 SQL 是哪条」 | `/api/traces/db_calls` |
| `db_sources` | 已配置的业务数据源：名称、类型、环境、说明及使用它的服务 | `/api/db/sources` |
| `db_tables` | 数据源中的表（MySQL / ClickHouse）、索引（Elasticsearch）或按 glob 扫描到的键（Redis） | `/api/db/{source}/tables` |
| `db_describe` | 表结构：建表语句、索引及其基数、行数与大小；Elasticsearch 给出字段映射，Redis 给出键的类型、TTL、长度与样本 | `/api/db/{source}/describe` |
| `db_query` | 执行一条只读查询：SQL（仅限 SELECT / WITH / SHOW / DESCRIBE / EXPLAIN）、Elasticsearch 查询 DSL 或 ES SQL、Redis 只读命令 | `/api/db/{source}/query` |
| `db_slow_queries` | 数据库自身记录的慢查询：MySQL 的 performance_schema 语句摘要与正在执行的语句、ClickHouse 的 `system.query_log`、Redis 的 SLOWLOG 与命令耗时、Elasticsearch 各索引的检索耗时 | `/api/db/{source}/slow` |

**只列出已启用的工具**：未配置指标表的部署不列出指标相关工具，未接入 goscan 的不列出 `cost_*`，未配置 `--datasources` 的不列出 `db_*`。列出之后模型也只会得到一句「未启用」，只是白白占用上下文。

**工具只做读操作**：全部标记了 `readOnlyHint`，客户端可据此免去每次调用的确认。MCP 中没有同步账单的工具：触发一次持续数分钟的云厂商拉取不在只读承诺之内，补数据由人在页面上操作。

**参数约定**：

* 时间参数：`from` / `to` / `at` 接受 RFC3339、不带时区的本地时间（按 `--timezone` 解释）、unix 秒或毫秒，以及 `now-30m` 这类相对写法；`range` 表示跨度（`15m` / `1h` / `24h`），未给出 `from` 时 `from = to - range`。
* 费用工具的时间参数是**账期**：`from` / `to` 写作 `2026-09`，或用 `months` 表示「最近几个月」，与其他工具的 `from` / `to` / `range` 不同。`cost_allocation` 默认只统计当前账期；跨账期且未指定 `days` 时改读阿里云的月度账单（行数约为日度账单的几十分之一），此时不提供日均与预估。`cost_compare` 只向前读取比较所需的天数（「与前一日比较」读取 9 天），而非页面固定的 62 天。
* 参数名写错（如把 `service` 写成 `service_name`）时，直接报告「不认识的参数」并列出可用的参数，而不会被静默忽略、返回一份全站数据作为答案。
* `initialize` 的 `instructions` 中写明了排障步骤、上述写法、当前时间以及表中实际可筛选的列名，模型接入后即可使用。

**结果的整理**：工具不直接访问查询层，而是把参数翻译为 `/api/*` 的查询串，在进程内经过同一个 axum Router，取得 JSON 后再整理为适合模型阅读的形式。因此参数校验、错误提示、读量护栏都与页面完全一致：模型传入不存在的列名，看到的是与页面相同的提示「不认识的筛选列 x；可用的筛选列: …」，照此修改即可。整理只做减法：

* 时间戳一律转换为 `--timezone` 的本地时间，模型无须自行换算毫秒；
* 省略空字段，不返回 `stats`、sparkline、空桶这类页面装饰；
* 长 message 与堆栈按参数截断并注明原长；
* 大列表的默认上限比页面小（日志 50 行、链路 20 条、错误 30 组）；
* 单次结果超过 64 KB 时，先裁减列表，再截断长文本，并在 `notes` 中说明裁减了什么，以免一次 `search_logs` 占满模型的上下文。

### 直连业务库

日志与链路能回答「哪里慢、哪里报错」，但不少问题要落到数据上才能下结论：这笔订单的状态究竟是什么，那条 SQL 是否使用了索引，缓存中的值是否已过期。配置 `--datasources` 之后，在代码仓库中运行的 MCP 客户端可以对照代码直接查询业务库，不必再请他人登录堡垒机代查。

支持四种数据库：MySQL（含 MariaDB、TiDB 等协议兼容的数据库）、Redis、Elasticsearch、ClickHouse。配置写法见 [`examples/datasources.toml`](examples/datasources.toml)；密码写作 `${ENV}` 由 Secret 注入，配置文件本身可以放进 ConfigMap。

**只读保证分三层**，由外向内：

1. **账号**：请为每个数据源建立只读账号。这是唯一真正可靠的一层，以下两层仅作兜底。
2. **数据库侧的只读模式**：MySQL 每次都在 `START TRANSACTION READ ONLY` 中执行并随即 `ROLLBACK`；ClickHouse 每条查询带 `readonly=2`；Elasticsearch 只开放 `_search`、`_sql`、`_mapping` 等读接口，URL 由 opdash 拼接，索引名中不允许出现 `/`、`?`；Redis 只放行白名单中的只读命令，`KEYS` 也不在其中（它会阻塞大库，列出键一律使用 SCAN）。
3. **语句校验**：SQL 在到达数据库之前先经过校验，只接受单条 `SELECT` / `WITH` / `SHOW` / `DESCRIBE` / `EXPLAIN`。任何位置出现写入类关键字（`WITH … DELETE`、`EXPLAIN ANALYZE UPDATE`）、MySQL 可执行注释 `/*! … */`，或读取外部数据、占用锁的函数（ClickHouse 的 `url()` / `file()` / `remote()`，MySQL 的 `LOAD_FILE()` / `GET_LOCK()`），都会被拒绝。

结果同样有上限：每个数据源有 `max_rows`（默认 500）与执行超时（默认 15 秒，MySQL 还会设置 `max_execution_time`）；Redis 中一次取出整个集合的命令会先检查集合大小，超过 1000 个成员即拒绝，并建议改用 `*SCAN`。每一次 `db_query` 都会在 opdash 的日志中记录数据源与语句，以备事后核查。

**链路与数据源的对应**：`trace_db_calls` 与 `db_calls_top` 读取 span 上的 `db.system`、`db.namespace`、`server.address` 等属性（新旧两版 OTel 语义约定都支持），按「对端地址 → 库名 → 服务名」的优先级对应到配置中的数据源，写在结果的 `source` 字段中；三者都对应不上时宁可不标注，以免模型去查询错误的数据库。Java agent 采集了 JDBC 参数（`db.query.parameter.<下标>`）时，还会给出代入参数后的语句 `statement_filled`，可以直接交给 `db_query` 执行 `EXPLAIN`。这两个工具读取的是 span 表，未配置数据源时同样可用，只是不标注 `source`。

### 传输与认证

* **Streamable HTTP，无状态**：每次 POST 一个 JSON-RPC 请求（也接受旧协议的批量数组），返回一个 JSON；不发送 `Mcp-Session-Id`，`GET /mcp`（服务端推送流）与 `DELETE /mcp`（结束会话）返回 405。没有会话就没有需要清理的状态，多副本部署也无须会话保持。
* **认证与页面一致**：`/mcp` 位于认证中间件之内，接受会话 cookie、Basic 认证与 API key 三种身份。MCP 客户端无法完成浏览器登录，所以使用 API key：每人使用自己的 key，日志中能看出是谁在查询。请不要把 `--basic-auth` 的共享密码分发给大家。未开启认证的部署，`/mcp` 同样不认证。
* **Origin 校验**：请求带有 `Origin` 头时，必须与 `Host` 是同一主机，否则返回 403（协议要求的 DNS rebinding 防护）。命令行客户端不带 Origin，不受影响。
* **错误的返回方式**：工具调用失败（参数错误、查询超时、读量超限）作为**工具结果**返回（`isError: true`），模型能看到原因并自行修改参数重试；只有「工具不存在」「方法不存在」这类协议层面的问题才作为 JSON-RPC 错误返回。

## 部署

镜像由 CI 构建并推送到 GHCR（推送 `v*` tag 时触发，见下文「发布」）。

```bash
# docker：连接线上 ClickHouse
OPDASH_CLICKHOUSE_URL=http://ck:8123 OPDASH_CLICKHOUSE_USER=opdash OPDASH_CLICKHOUSE_PASSWORD=xxx \
  docker compose up -d

# k8s：Deployment、Service、Ingress 按集群自行编写（deploy/ 目录包含内部域名与密钥，不纳入版本库），
# 环境变量参照上文的配置表；Secret 中存放 ClickHouse 密码、Keycloak client 密钥与 session secret
kubectl -n logging port-forward svc/opdash 4880:4880     # 未配置 Ingress 时先在本地查看
```

部署时注意：

* **认证**：对外暴露时务必配置 Keycloak 登录（`OPDASH_OIDC_*`，见上文「登录」），至少也要配置 `OPDASH_BASIC_AUTH`。opdash 能查看全部线上日志。
* **读量护栏**：k8s 上建议把 `OPDASH_MAX_READ_BYTES` 设为 100 GiB 左右，按集群规模调整。
* **探针**：readiness 探针请求 `/api/health`（不认证，会实际 ping ClickHouse），数据库不可用时摘除流量；liveness 使用 tcpSocket 即可，数据库不可用时重启进程并无帮助。
* **日志跟随的长连接**：跟随使用 SSE 长连接（`/api/logs/tail`），响应带 `X-Accel-Buffering: no` 且不压缩，nginx 与 ingress 通常无须调整；每 15 秒发送一次保活注释，空闲时也不会被超时断开。若中间还有其他代理，请确认它没有开启响应缓冲，且读超时大于 15 秒。
* **持久化**：API key 与收藏查询的文件位于 `/var/lib/opdash`，须挂载为卷，见上文「API key」。

### 发布

1. 同时修改 `Cargo.toml` 与 `Cargo.lock` 中的版本号（`cargo build` 会顺带更新 lock 文件），以 `chore(release): 0.18.7` 为标题提交并推送到 main。CI 会校验 tag 与 `Cargo.toml` 的版本是否一致。
2. 打附注 tag，说明按 GitHub Release 的格式书写（见 `CLAUDE.md`），**单独推送 tag**：tag 与分支放在同一条 `git push` 中推送不会触发构建。

```bash
git tag -a v0.18.7 -F notes.md    # notes.md 中是按 GitHub Release 格式写好的说明
git push origin v0.18.7
```

产出 `ghcr.io/easayliu/opdash:v0.18.7` 与 `:latest`（`v0.2.0-rc1` 这类预发布版本不更新 `latest`）。`.github/workflows/ci.yml` 在 push 与 PR 时依次执行 `pnpm build`、`pnpm lint`、`pnpm test`、`cargo fmt --check`、`cargo clippy -D warnings`、`cargo test`；`docker.yml` 在构建镜像前复用它作为闸门。

## 测试

```bash
cargo test                                   # 单元测试 + 基于假 ClickHouse 的集成测试（不需要数据库）
OPDASH_E2E_CLICKHOUSE_URL=http://host:8123 OPDASH_E2E_CLICKHOUSE_USER=x OPDASH_E2E_CLICKHOUSE_PASSWORD=y \
  cargo test --test e2e_clickhouse -- --nocapture   # 对真实数据库逐个调用每个接口，只读
```

假 ClickHouse（`tests/support`）回放预设的响应并记录收到的请求，断言的是发出的 SQL、`param_*`、设置项与认证头。它无法验证 ClickHouse 如何解析参数、JSON 子列如何读取以及 Distributed 表上 `LIMIT BY` 的行为，这些由 E2E 测试覆盖。

## API

除标注的写操作外均为 GET，返回 JSON。错误格式为 `{"error": "...", "kind": "bad_request|timeout|too_heavy|unavailable|internal"}`，遇到 `timeout` / `too_heavy` 时前端会提示缩小范围。时间入参统一为 unix 毫秒；出参中日志用 `ts_ms`，span 用 `start_us`（微秒）+ `duration_ns`。每个响应都带有 `stats`（扫描行数、字节数、耗时），页面上会显示出来，让使用者了解这次查询的代价。

```text
GET  /api/meta                 表结构、维度列、各项上限
GET  /api/health               ping ClickHouse 并返回表结构状态，不认证
     /api/auth/*               登录与 API key，见上文「登录」「API key」
     /api/saved                收藏查询（GET / POST / PUT / DELETE），见上文「收藏查询」

GET  /api/logs/search          ?from&to&q&regex&level&logger&thread&host&trace_id&span_id&<维度列>&order&limit&offset&count
GET  /api/logs/histogram       筛选参数同 search
GET  /api/logs/facets          ?field=level|logger|host|<维度列>&limit
GET  /api/logs/context         ?host&file&ts&before&after
GET  /api/logs/export          同 search，&format=csv|jsonl
GET  /api/logs/tail            日志跟随（SSE）

GET  /api/traces/search        ?from&to&service&span_name&kind&error_only&min_ms&max_ms&attr=k=v&rattr=k=v&sort=time|duration&limit&trace_id
GET  /api/traces/heatmap       筛选参数同 search   耗时 × 时间分布图
GET  /api/traces/{trace_id}                   ?at&span   瀑布图所用的轻量列，不含属性
GET  /api/traces/{trace_id}/spans/{span_id}   ?at&service&name&ts   单个 span 的属性、events 与 links
GET  /api/traces/{trace_id}/db                ?at   这条链路中的数据库调用（语句、参数、对应的数据源）
GET  /api/traces/db_calls      ?from&to&service&system&min_ms&error_only&sort=total|p95|max|calls|errors&limit
GET  /api/traces/values        ?field=service|span_name&service&kind=entry|client|all
GET  /api/traces/attr_keys     ?service&scope=span|resource
GET  /api/traces/attr_values   ?key&service&scope

GET  /api/errors               ?from&to&kind=entry|client|all&service&span_name   错误分组，默认 entry
GET  /api/services             ?from&to&compare=day|week|prev&<维度列>
GET  /api/services/operations          ?service=a&service=b&...   一次最多 24 个服务
GET  /api/services/{name}/operations   ?from&to&kind=entry|client&compare=day|week|prev|none
GET  /api/services/{name}/timeseries   ?from&to&span_name&compare=day|week|prev|none

GET  /api/metrics              ?from&to&service   指标目录（只扫描最近 6 小时）
GET  /api/metrics/query        ?from&to&metric&service&agg&field&by&attr=k=v&rattr=k=v&q&step&limit
                               未给出 agg / field 时按指标类型选择（直方图→分位数、counter→rate、gauge→avg）；
                               按类型必然查不到数据的组合（如直方图 field=value）返回 400
GET  /api/metrics/labels       ?metric&column=attributes|resource_attributes
GET  /api/metrics/label_values ?metric&key&column
GET  /api/metrics/exemplars    ?metric&service&attr&limit
GET  /api/metrics/events       ?from&to&metric&service&field   进程重启与新 pod 启动的时刻；不给 service 时为全站

GET  /api/bills/periods        库中有哪些账期（费用页据此确定默认区间）
GET  /api/bills/summary        ?from=2026-04&to=2026-09&amount=payable|paid|original&provider&<维度>&q
                               账期格式为 YYYY-MM，一次最多 36 个；不给出时取最近 6 个
GET  /api/bills/daily          参数同上，按天汇总（只查询有日粒度的表）
GET  /api/bills/breakdown      ?by=product|item|region|zone|account|instance|project|subscription|currency&limit
GET  /api/bills/allocation     ?days=7   按归属规则分摊到业务线，给出日均、预付费摊销与月度预估
GET  /api/bills/allocation/day ?day=YYYY-MM-DD&amount   某一天与前一天按产品、规则的费用对比
GET  /api/bills/product-days   ?days&amount   最近若干天各产品每天的费用
GET  /api/bills/detail         ?provider=volcengine|alicloud&granularity=monthly|daily&limit&offset
GET  /api/bills/facets         同 detail，&dims=product,region,…   明细表头筛选的候选值
GET  /api/bills/export         同 detail，&format=csv|jsonl
POST   /api/bills/sync                   {provider, from, to, granularity, force, mode}   手动拉取一次，转发给 goscan
GET    /api/bills/sync/{task_id}         本次拉取的进度（事件流不可用时页面每 2 秒查询一次）
GET    /api/bills/sync/{task_id}/events  同上，SSE 推送：event: task 为任务状态，event: done 表示已结束
DELETE /api/bills/sync/{task_id}         停止同步：goscan 写完当前这一趟再停，已结束的任务返回 409
GET    /api/bills/sync/running           ?provider   这朵云正在进行的同步（手动或定时发起），没有则 task 为 null

GET  /api/db/sources           已配置的业务数据源（--datasources），不含账号密码
GET  /api/db/{source}/tables   ?database&match&limit
GET  /api/db/{source}/describe ?target&database
GET  /api/db/{source}/query    ?q&database&index&limit   只读查询，校验规则见上文「直连业务库」
GET  /api/db/{source}/slow     ?from&to&database&min_ms&sort=total|avg|max|calls&limit

POST /mcp                      MCP 端点，见上文「给 AI 用：MCP」
```

## 尚未实现

* 日志分页使用 `OFFSET`，最多翻到第 10000 条（`--max-offset`），再往后需要缩小范围。keyset 分页需要行内唯一键，而表中没有。
* 上下文按 `host + file` 取，容器重启后换了文件（`0.log` → `1.log`）就会中断；有 `pod` 列时可以在日志页按 pod 筛选。
* 指标页只绘制单个指标，没有多指标运算（如用 `a / b` 计算成功率）、没有保存的面板，也不支持 PromQL。表达式需要先有一层解析，目前的「agg + by + 过滤」足以查看曲线。
* 指数直方图（`ExponentialHistogram`）不计算分位数：它的桶以 `base^i` 编码，没有 `explicit_bounds`，需要另写一套换算。Summary 的分位数由采集端计算，多条时间线无法合并，同样只提供 count 与 sum。
* 没有告警，不写业务数据，也没有用户与权限管理（只有「能否访问」一种权限）。
