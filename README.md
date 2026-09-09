# opdash

trace 和日志的查询页面。数据来自 [logpipe](../log) 写的 `logs.app_log` 和 [tracepipe](../trace) 写的
`logs.otel_trace` 两张 ClickHouse 表，给内部业务开发排障用：拿一个 trace id 看整条链路和这条请求的
全部日志；按服务 / 接口找慢请求、错请求；按关键字翻日志、看堆栈、看上下文；看某个服务的错误率和
P95。单二进制，只读，不需要别的服务。

```text
  浏览器 ──▶ /api/*  axum ── 参数化 SQL、readonly=2 ──▶ ClickHouse HTTP（单机或 Distributed）
     ▲          │
     └── /  内嵌 SPA（rust-embed，ui/dist）
```

## 页面

| 路径 | 干什么 |
| --- | --- |
| `/logs` | 日志检索：关键字 / 正则、级别、服务 / namespace / pod 等维度、logger、thread；直方图拖选缩小范围；展开看全文；上下文；跟随；导出 CSV / JSONL |
| `/traces` | 链路检索：服务、接口、span 类型、只看错误、耗时区间、属性 `key=value`；耗时 × 时间散点图 |
| `/traces/:trace_id` | 链路详情：瀑布图、span 属性 / 资源 / 事件（异常堆栈）/ 链接、这条 trace 的日志 |
| `/services` | 服务概览：请求数、QPS、错误率、P50 / P95 / P99（只算 Server / Consumer 这类入口 span） |
| `/services/:name` | 单个服务：入口接口 / 下游调用两张表，请求量与错误、延迟分位趋势 |

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
| `--timezone` | `OPDASH_TIMEZONE` | `Asia/Shanghai` | 直方图分桶对齐的时区，和两张表 `timestamp` 列的时区一致 |
| `--query-timeout` | `OPDASH_QUERY_TIMEOUT` | `30s` | 传给 ClickHouse 的 `max_execution_time` |
| `--max-range` | `OPDASH_MAX_RANGE` | `31d` | 允许查询的最大时间跨度 |
| `--max-rows` / `--max-offset` | `OPDASH_MAX_ROWS` / `OPDASH_MAX_OFFSET` | `1000` / `10000` | 日志一页最多几行；最多翻到第几条 |
| `--export-max-rows` | `OPDASH_EXPORT_MAX_ROWS` | `50000` | 导出上限 |
| `--max-trace-spans` | `OPDASH_MAX_TRACE_SPANS` | `5000` | 一条 trace 最多取多少 span，超过标记截断 |
| `--max-read-bytes` / `--max-read-rows` | `OPDASH_MAX_READ_BYTES` / `OPDASH_MAX_READ_ROWS` | `0`（不限） | 单条查询的读量护栏（`max_bytes_to_read` / `max_rows_to_read`），超过立刻报错让用户缩小范围，比等超时体验好；集群上按分片各自计 |
| `--max-concurrent-queries` | `OPDASH_MAX_CONCURRENT_QUERIES` | `16` | 同时最多几条查询在库上跑，排队 10 秒没名额回 503 |
| `--schema-refresh` | `OPDASH_SCHEMA_REFRESH` | `5m` | 多久重读一次 `system.columns` |
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

固定列是程序写死的（logpipe 的 9 列、tracepipe 的 22 列），启动时读 `system.columns` 校验，缺列直接在
`/api/health` 里报出来。**固定列之外的字符串列自动变成筛选维度**：k8s 元数据（`service_name` /
`namespace` / `pod` / `container` / `stream`）、采集端配置里 `fields` 加的静态列（`cluster` / `env` ……）
都不用改 opdash，`/api/meta` 的 `dimensions` 里有什么页面就显示什么筛选项。新加了列过 5 分钟自动认到。

span 表的四个属性列必须是 ClickHouse 的 `JSON` 类型（tracepipe v0.2.0 起，ClickHouse 25.3+）。
v0.1 的 `Map` 表不兼容，`/api/health` 会点名哪一列是 Map，按 tracepipe README 重建即可。

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
* 链路详情也两次往返：先 `WHERE trace_id = ?` 只读 `span_id, service_name, span_name, timestamp` 定位，
  再按 `service_name IN ... AND span_name IN ... AND timestamp BETWEEN ...` 走排序键前缀取 JSON 属性、
  events / links 这些重列。`trace_id` 只有 bloom filter（GRANULARITY 4，2.5% 误报），一天一亿多 span 时
  过了索引的块绝大多数是误报（线上 EXPLAIN：18371 个 granule 剩 476 个，和期望误报数正好对上），一步
  到位地读完整 JSON 就是几个 GB、30 秒超时；拆开后误报块只读几十字节一行，重列由主键精确圈到。
  第二步不传 span id 列表：参数都在 URL 里，5000 个 id 会超过 64 KB 的 URI 上限；两步排序相同、
  时间区间卡在第一步的首尾毫秒，取同样多的行就是同一批 span。
* 属性过滤写成子列标识符 `` span_attributes.`http.route` ``：只读那一个子列（线上 10 分钟数据 12 MB、
  40 ms）。`getSubcolumn(col, {path:String})` 虽然能把路径当参数，但 MergeTree 上会把整个 JSON 列读出来
  （同一查询 5.9 GB、5 秒）。路径进 SQL 前按标识符规则校验（不含反引号 / 反斜杠 / 控制字符）。
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
GET /api/traces/{trace_id}
GET /api/traces/values        ?field=service|span_name&service&kind=entry|client|all
GET /api/traces/attr_keys     ?service&scope=span|resource
GET /api/traces/attr_values   ?key&service&scope
GET /api/services             ?from&to
GET /api/services/{name}/operations   ?kind=entry|client
GET /api/services/{name}/timeseries   ?span_name
```

## 还没做

* 日志分页是 `OFFSET`，最多翻到第 10000 条（`--max-offset`）；再往后让用户缩小范围。keyset 分页需要一个
  行内唯一键，表里没有。
* 上下文按 `host + file` 取，容器重启换了文件（`0.log` → `1.log`）就断了；有 `pod` 列时可以在日志页按 pod 筛。
* 没有告警、没有指标（有 spanmetrics + Prometheus）、不写库、没有用户系统。
