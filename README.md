# opdash

trace、日志和指标的查询页面。数据来自 [logpipe](../log) 写的 `logs.app_log`、[tracepipe](../trace) 写的
`logs.otel_trace` 和 [metricpipe](../metric) 写的 `logs.otel_metric` 三张 ClickHouse 表，给内部业务开发
排障用：拿一个 trace id 看整条链路和这条请求的全部日志；按服务 / 接口找慢请求、错请求；按关键字翻日志、
看堆栈、看上下文；看某个服务的错误率和 P95；按标签画指标曲线，从指标上的 exemplar 直接跳到那次请求。
单二进制，只读，不需要别的服务。

指标表是**可选**的：没部署 metricpipe 的地方 `logs.otel_metric` 不存在，指标页签自动不显示，另外两张表
照常用。

```text
  浏览器 ──▶ /api/*  axum ── 参数化 SQL、readonly=2 ──▶ ClickHouse HTTP（单机或 Distributed）
     ▲          │
     └── /  内嵌 SPA（rust-embed，ui/dist）
```

## 页面

| 路径 | 干什么 |
| --- | --- |
| `/logs` | 日志检索：关键字 / 正则、级别、服务 / namespace / pod 等维度、logger、thread；直方图拖选缩小范围；展开看全文；上下文；跟随（SSE 推送，秒级；可切终端模式正序打印、自动滚到底）；导出 CSV / JSONL |
| `/traces` | 链路检索：服务、接口、span 类型、只看错误、耗时区间、属性 `key=value`；耗时 × 时间散点图 |
| `/traces/:trace_id` | 链路详情：瀑布图、span 属性 / 资源 / 事件（异常堆栈）/ 链接、这条 trace 的日志 |
| `/metrics` | 指标，两个页签：**服务看板**（选一个服务，按 OTel 语义约定自动拼出 HTTP / JVM / 连接池 / Kafka / Go 几套面板，顶上三个数是请求量、错误率、P95）和**全部指标**（233 个指标名平铺，自己选算法、分组、过滤；图上的圆点是 exemplar，点开就是那次请求的链路） |
| `/services` | 服务概览：请求数、QPS、错误率、P50 / P95 / P99（只算 Server / Consumer 这类入口 span） |
| `/services/:name` | 单个服务：入口接口 / 下游调用两张表，请求量与错误、延迟分位趋势 |

### 四个页面互相怎么跳

三个信号加服务概览，两两之间都能跳，**跳过去看到的是同一个服务、同一段时间**（地址拼装都在
`ui/src/lib/links.ts`，散在各页手写迟早有一处忘了带 `from` / `to`）：

| 从 | 到 | 入口 |
| --- | --- | --- |
| 日志行 | 链路详情 | 行尾的 trace id；16 位的 span id 跳「这个 span 的全部日志」 |
| 日志页（筛了服务） | 指标看板 / 最慢的链路 / 出错的链路 / 服务概览 | 筛选栏下面那一排 |
| 链路列表（筛了服务） | 指标看板 / 错误日志 / 服务概览 | 筛选栏下面那一排 |
| 链路详情 | 服务指标 / 服务概览 / 这条链路的全部日志 | 顶部；指标的时间窗以这条 trace 为中心前后各 15 分钟 |
| 链路详情 · 某个 span | 这个服务的指标 / 只看这个 span 的日志 | 右侧 span 面板 |
| 指标看板 | 最慢的链路 / 出错的链路 / 错误日志 / 服务概览 | 顶部；图上拖一段之后带的就是拖出来的窗口 |
| 指标图上的 exemplar | 那一次请求的链路详情 | 图上的圆点 |
| 服务概览 | 三个信号 | 顶部 |

从「一个时刻」（一条日志、一个 span、一个 exemplar）跳到按时间段看的页面时，前后各放宽
15 分钟（`WINDOW_AROUND_MS`）——只给那一毫秒的话指标图上一个点都没有，放宽了才看得出尖峰
是从什么时候开始的。按 trace id / span id 查日志则**不带**时间范围，理由见下面「查询是怎么写的」。

顶栏：时间范围（相对 / 绝对）、直达框（粘一个 trace id 直接开链路，16 位 hex 当 span id，其它当关键字搜日志）、
深浅色。**所有筛选条件都在 URL 里**，链接复制给同事就是同一个视图；相对范围（`range=1h`）打开时按当时的
时间算，绝对范围（`from=&to=`）永远是那一段。

按 trace id / span id 查日志时**不带时间范围**：两张表的 `trace_id` 都有 bloom filter，点查不需要时间条件，
而 3 小时前的 trace id 在「最近 15 分钟」下本来就查不到。

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
| `--timezone` | `OPDASH_TIMEZONE` | `Asia/Shanghai` | 直方图分桶对齐的时区，和两张表 `timestamp` 列的时区一致 |
| `--query-timeout` | `OPDASH_QUERY_TIMEOUT` | `30s` | 传给 ClickHouse 的 `max_execution_time` |
| `--max-range` | `OPDASH_MAX_RANGE` | `31d` | 允许查询的最大时间跨度 |
| `--max-rows` / `--max-offset` | `OPDASH_MAX_ROWS` / `OPDASH_MAX_OFFSET` | `1000` / `10000` | 日志一页最多几行；最多翻到第几条 |
| `--export-max-rows` | `OPDASH_EXPORT_MAX_ROWS` | `50000` | 导出上限 |
| `--max-trace-spans` | `OPDASH_MAX_TRACE_SPANS` | `5000` | 一条 trace 最多取多少 span，超过标记截断 |
| `--max-read-bytes` / `--max-read-rows` | `OPDASH_MAX_READ_BYTES` / `OPDASH_MAX_READ_ROWS` | `0`（不限） | 单条查询的读量护栏（`max_bytes_to_read` / `max_rows_to_read`），超过立刻报错让用户缩小范围，比等超时体验好；集群上按分片各自计 |
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

```text
GET /api/auth/me        登录方式和当前用户；没登录也 200（前端据此跳登录）
GET /api/auth/login     ?next=/logs   生成登录票，跳 Keycloak
GET /api/auth/callback  Keycloak 跳回来的地址
GET /api/auth/logout    清会话，跳 Keycloak 登出再回首页
```

没登录的请求：浏览器导航（Accept 带 `text/html`）302 去登录；API 和静态资源回
`401 {"error":"需要登录","kind":"unauthenticated","login_url":"/api/auth/login"}`，前端拿到就整页跳登录。
`/api/health` 和 `/api/auth/*` 不认证。

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

### 查询是怎么写的（排障时看这里）

* 所有用户输入都走 ClickHouse 查询参数 `{name:Type}`，SQL 文本里只有白名单里的列名。`String` 参数按
  TSV 规则转义（ClickHouse 那头按 TSV 解），正则里的 `\d+` 才能原样到达。
* 每个请求带：`readonly=2`、`cancel_http_readonly_queries_on_client_close=1`（关掉页面查询就停）、
  `max_execution_time`、`wait_end_of_query=1`（错误一定是干净的 5xx 而不是 200 + 半截 JSON）、
  `output_format_json_quote_64bit_integers=0`；span 表的查询另带 `optimize_skip_unused_shards=1`。
* 时间范围：`timestamp >= fromUnixTimestamp64Milli({from:Int64})`，参数代入后是常量，能裁剪分区、走主键。
* 日志检索 / 上下文 / 导出统一 `ORDER BY timestamp, level, trace_id`——**就是表自己的排序键**，
  ClickHouse 才能纯按顺序倒着读、读够 LIMIT 就停。排序键以外的列一旦参与排序，计划里会多出
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
  词是**纯 ASCII 字母数字且 ≥16 位**时（span id 16 位、trace id / msgId 32 位）发
  `hasToken(lower(message), 小写词)`，能整块跳过不含它的 granule；其余仍是子串匹配。线上实测
  一小时窗口查一个 msgId：**7.83 GB / 1063 ms → 0.030 GB / 140 ms，命中数一致**。

  规则的边角，改之前先看清楚：

  * 判断必须严格（`^[A-Za-z0-9]+$`）——`hasToken` 遇到带分隔符的 needle 会**抛异常**而不是返回空。
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
* **别把 `service_name` 挪进日志表的排序键**——直觉上「按服务排就能只扫这个服务的 message」，
  实测是亏的：近 7 天 12823 次日志检索里只有 **394 次（3%）**带服务筛选，其余 97% 是「最近 N 条」。
  时间打头时后者顺序读、读够就停（0.334 GB）；服务打头就没法顺序读，要把整段时间的行排一遍
  （**3.49 GB，10 倍**，`optimize_read_in_order = 0` 模拟出来的）。为 3% 的查询让 97% 的查询贵十倍，
  不划算。想要「按服务扫得少」得等一个不牺牲时间序的方案（投影要多存一份 message，136 GiB，
  更不划算）。
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
* 相对范围（`最近 N 分钟`）在前端解析成绝对毫秒后**固定到下次刷新**：翻页、改排序、切页面都不会让
  `to` 往前爬。否则每改一个参数 `from` / `to` 就变几毫秒，直方图和 facet 明明和翻页无关也得重扫一遍，
  而且第二页和第一页的窗口边界对不上、行会错位。点刷新（或重新选范围）才推进到当前时间。
* 链路检索两次往返：先 `SELECT trace_id ... ORDER BY timestamp DESC LIMIT 1 BY trace_id LIMIT n`
  拿候选 id，再 `WHERE trace_id IN {ids:Array(String)} GROUP BY trace_id` 聚合摘要。不用嵌套子查询，
  Distributed 表上 `distributed_product_mode=deny` 也没问题。「请求耗时」= 根 span 的耗时（根缺失时
  退回最早的 span）；「总跨度」= 最早 span 开始到最晚 span 结束，异步消费会让它比请求耗时长得多。
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

k8s 上 `OPDASH_MAX_READ_BYTES` 建议给个 20 GiB 左右的护栏，按集群规模调。readiness 探针打
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
GET /api/metrics/labels       ?metric&column=attributes|resource_attributes
GET /api/metrics/label_values ?metric&key&column
GET /api/metrics/exemplars    ?metric&service&attr&limit
GET /api/services             ?from&to
GET /api/services/{name}/operations   ?kind=entry|client
GET /api/services/{name}/timeseries   ?span_name
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
