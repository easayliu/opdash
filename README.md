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
| `--basic-auth` | `OPDASH_BASIC_AUTH` | 不认证 | `user:password`，配了就要求浏览器登录；`/api/health` 不认证 |

`RUST_LOG=opdash=debug` 能看到每条 SQL 和绑定的参数。

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
* 日志：`ORDER BY timestamp DESC, host, file, thread, logger, message LIMIT n OFFSET m`——排序键前缀是
  `timestamp`，ClickHouse 按排序键倒着读、读够就停；后面几列只是让同一毫秒的行有确定顺序。关键字是
  `positionCaseInsensitiveUTF8(message, ...)`，没有索引，扫的是时间范围内的全部行，所以关键字搜索
  用 `exact_rows_before_limit=1` 一次扫描顺带把总数算出来，不扫两遍。
* 链路检索两次往返：先 `SELECT trace_id ... ORDER BY timestamp DESC LIMIT 1 BY trace_id LIMIT n`
  拿候选 id，再 `WHERE trace_id IN {ids:Array(String)} GROUP BY trace_id` 聚合摘要。不用嵌套子查询，
  Distributed 表上 `distributed_product_mode=deny` 也没问题。「请求耗时」= 根 span 的耗时（根缺失时
  退回最早的 span）；「总跨度」= 最早 span 开始到最晚 span 结束，异步消费会让它比请求耗时长得多。
* 属性过滤写成子列标识符 `` span_attributes.`http.route` ``：只读那一个子列（线上 10 分钟数据 12 MB、
  40 ms）。`getSubcolumn(col, {path:String})` 虽然能把路径当参数，但 MergeTree 上会把整个 JSON 列读出来
  （同一查询 5.9 GB、5 秒）。路径进 SQL 前按标识符规则校验（不含反引号 / 反斜杠 / 控制字符）。
* 服务概览：`quantilesTDigest` 而不是默认的 `quantiles`（后者是 8192 个样本的水塘抽样，尾部分位最不准）。
* 直方图分桶：`intDiv(toUnixTimestamp64Milli(timestamp) - origin, width)`，原点是范围起点那天的本地零点。
  不用 `toStartOfInterval(..., INTERVAL n SECOND)`：它按 UTC 取整，6 小时一桶时边界落在北京时间 02 / 08 点。
* 老分区没有索引：`ADD INDEX` 只对新写入的 part 生效，按 trace id 查历史日志慢的话在库上
  `ALTER TABLE logs.app_log_local ON CLUSTER log MATERIALIZE INDEX idx_trace_id`。

## 部署

镜像由 CI 构建推送到 GHCR（打 `v*` tag 触发，见下面「发布」）。

```bash
# docker：指向线上库
OPDASH_CLICKHOUSE_URL=http://ck:8123 OPDASH_CLICKHOUSE_USER=opdash OPDASH_CLICKHOUSE_PASSWORD=xxx \
  docker compose up -d

# k8s：Secret 里填账号密码，可选 Basic 认证；Deployment + Service + Ingress
kubectl apply -f deploy/opdash-deployment.yaml
kubectl -n logging port-forward svc/opdash 4880:4880     # 没配 Ingress 先本地看
```

`deploy/opdash-deployment.yaml` 里 `OPDASH_MAX_READ_BYTES` 默认 20 GiB，按集群规模调。readiness 探针打
`/api/health`（会真的 ping ClickHouse），库挂了会摘流量。对外暴露务必配 `OPDASH_BASIC_AUTH` 或放在
SSO 网关后面：这个页面能翻全部线上日志。

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
GET /api/health               ping ClickHouse + 表结构状态，不走 Basic 认证
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
