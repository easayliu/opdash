# 查询实现与实测

本文记录 opdash 各接口的 SQL 为什么这样写，以及支撑这些决定的线上实测数据，排查查询慢、结果不对时从这里看起。页面设计见 [design.md](design.md)，账单表的查询见 [bills.md](bills.md)。

文中的数据都注明了测量日期。表结构、数据量和 ClickHouse 版本都会变化，引用之前请留意时效。

## 通用约定

* **用户输入一律走查询参数** `{name:Type}`，SQL 文本中只出现白名单内的列名。`String` 参数按 TSV 规则转义（ClickHouse 按 TSV 解析），正则中的 `\d+` 才能原样到达。
* **每个请求都带以下设置**：`readonly=2`、`cancel_http_readonly_queries_on_client_close=1`（关闭页面即停止查询）、`max_execution_time`、`wait_end_of_query=1`（出错时一定是干净的 5xx，而不是 200 加半截 JSON）、`output_format_json_quote_64bit_integers=0`；span 表的查询另带 `optimize_skip_unused_shards=1`。
* **时间范围**写作 `timestamp >= fromUnixTimestamp64Milli({from:Int64})`。参数代入后是常量，可以裁剪分区、使用主键。无论排序键如何，时间范围都能利用主键的通用排除搜索（generic exclusion search），只读取范围内的 granule。
* **相对范围在前端固定到下次刷新**：「最近 N 分钟」在前端解析为绝对毫秒后保持不变，翻页、改排序、切换页面都不会让 `to` 向前推移。否则每改一个参数，`from` / `to` 就变化几毫秒，与翻页无关的直方图和 facet 也要重新扫描，第二页与第一页的窗口边界也对不上，行会错位。点击刷新（或重新选择范围）才推进到当前时间。
* **属性过滤使用子列标识符** `` span_attributes.`http.route` ``，只读取这一个子列（线上 10 分钟数据 12 MB、40 ms）。`getSubcolumn(col, {path:String})` 虽然能把路径作为参数，但在 MergeTree 上会读出整个 JSON 列（同一查询 5.9 GB、5 秒）。路径进入 SQL 之前按标识符规则校验（不得含反引号、反斜杠、控制字符）。
* **手写 `PREWHERE` 通常没有意义**：`optimize_move_to_prewhere` 默认开启，实测把 `service_name` 显式移到 PREWHERE，读量几乎不变（39.6 GB → 40.5 GB，反而略增）。例外见下文「`trace_id IN` 放进 PREWHERE」。
* **历史分区没有新加的索引**：`ADD INDEX` 只对新写入的 part 生效。按 trace id 查历史日志较慢时，在库上执行 `ALTER TABLE logs.app_log_local ON CLUSTER log MATERIALIZE INDEX idx_trace_id`（`otel_trace_local` 同理）。哪些 part 没有索引，看 `system.parts` 的 `secondary_indices_compressed_bytes`，为 0 即没有。

## 日志

### 一条日志能有多大

线上 `message` 的 p50 为 **129 字符**、p99 为 9.4 KB，但一小时的 844 万条中有 **200 条超过 1 MB，最大 49 MB**（业务把整个响应体写进了日志）。只要碰上一条，页面就无法使用：一条 52 行的链路日志响应达 **63.8 MB / 12.6 秒**，浏览器主线程连续无响应**近两分钟**。

因此，列表、上下文、跟随这三条路径都**在 SQL 中截断** `message`（`--max-message-chars`，默认 16384 字符，是 p99 的 1.7 倍，正常的堆栈与 SQL 不会被截），同时带回原始长度 `message_len`。前端在文字断开处标注「· 已截断」，展开后说明全文长度，并提示通过导出获取全文。同一个请求改后为 **103.8 KB / 0.38 秒**，主线程最长阻塞 5 ms。

需要注意：

* **必须在 SQL 中截断，不能取回后再截**：那 12.6 秒中 ClickHouse 只占 1.7 秒，其余 11 秒都花在 ClickHouse 到 opdash 的传输上，在 Rust 中截断省不掉这部分。
* **单位是字符而非字节**（`substringUTF8` / `lengthUTF8`）：按字节截断会把多字节字符切成两半，而 ClickHouse 对非法 UTF-8 的行为未定义，输出的 JSON 可能无法解析。
* **原始长度要写成 `` `app_log`.message ``**：直接写 `lengthUTF8(message)` 会解析到截断后的别名 `message`，测出的长度永远等于上限（线上曾返回 16384，而实际是 41149053）。`export_columns` 中的 `time` 别名踩过同一个坑，单元测试固定了这一写法。
* **导出不截断**：导出是获取全文的唯一途径。

### 排序与翻页

日志检索、上下文、导出统一使用 `ORDER BY timestamp, level, trace_id`，**即表排序键去掉服务之后的部分**（锁定单个服务时，这正是该服务数据段的物理顺序），ClickHouse 才能按顺序倒读、读满 LIMIT 即停止。排序键以外的列一旦参与排序，执行计划中会多出 `PartialSorting` + `FinishSorting`，为了确定这 200 行的先后要多读大量数据。线上 26.3 实测（1 小时窗口）：

| | 排序键以外的列参与排序 | 只按排序键 |
|---|---|---|
| 翻一页 200 行 | 3.03 GB / 4240 ms | **0.14 GB / 166 ms** |
| 关键字 + 200 行 | 9.19 GB / 20.8 s | **6.00 GB / 4.7 s** |
| 查看上下文 50 行 | 7.58 GB / 9075 ms | **0.47 GB / 137 ms** |

代价是 `(timestamp, level, trace_id)` 相同的行之间先后顺序不确定（线上 10 分钟内 38% 的行处于这种并列组中，最大的一组 233 行），页边界恰好落在一组中间时，翻页可能重复或遗漏几行。要做到精确，需要改用 keyset 翻页，而 keyset 需要行内唯一键，表中没有，所以尚未实现。

### 关键字匹配

关键字语法见 [README](../README.md#日志检索的关键字语法)。每个词翻译为 `positionCaseInsensitiveUTF8(message, ...)`；全部由词组成的 OR 合并为一个 `multiSearchAnyCaseInsensitiveUTF8(message, [...])`，一次扫描完成。`message` 本身没有索引，扫描的是时间范围内的全部行，只有下面「token 索引」一节所述的情形例外。

### token 索引

`app_log_local` 上有 `INDEX idx_message_tokens lower(message) TYPE tokenbf_v1(131072, 3, 0) GRANULARITY 1`。关键字按与 tokenizer 相同的规则切分（非字母数字的 ASCII 字符均为分隔符，下划线也算），**足够长（≥ 16 位）的 token** 才作为 needle 使用：

* **纯 id**（span id 16 位，trace id、msgId 32 位）：发 `hasToken(lower(message), 小写词)`，单独即可表达完整语义。线上实测一小时窗口查询一个 msgId：**7.83 GB / 1063 ms → 0.030 GB / 140 ms，命中数一致**。
* **键加 id**（`msgId:AC10…`、`traceId=…`）：id 这个 token 用 `hasToken` 跳过 granule，再以 AND 连接原有的子串条件，保证键也匹配。2026-09-18 实测一小时窗口查询 `msgId":"AC10…`（键加 32 位 id）：**10.24 GB / 1.2 s → 0.02 GB / 0.2 s**（读取 630 万行 → 4661 行，剩余的是 bloom filter 的误判）。
* **切分后全是短词的复合标识符不发**，如 `RESULT_CHANGE`、`im_enter_direct_msg`。2026-09-18 测量（1 小时窗口、3 分片）：`change` / `result` 分别命中 359 / 364 个 granule，一个都跳不掉，多出的两个 `hasToken` 反而使耗时增加 10% ~ 40%（1320 → 1950 ms、1517 → 1655 ms）。常见词无论如何组合都进不了索引，原因见下文「跳数索引的上限」。这类词 24 小时的子串检索需要读取 250 ~ 290 GB（实测每小时 10 ~ 12 GB），读量护栏按总量计算，设为 100 GiB 时在十来个小时处必然触发。

修改这套规则之前，先看清以下边界条件：

* needle 必须是切分好的纯字母数字 token：`hasToken` 遇到带分隔符的 needle 会**抛出异常**，而不是返回空。
* 排除词（`-词` / `NOT 词`）一律保持子串语义：整词匹配比子串匹配窄，取反之后反而变宽，会漏掉本应排除的行；否定条件本来也用不上 bloom filter。
* OR 组仍走 `multiSearchAnyCaseInsensitiveUTF8`，不使用索引。
* 语义确实收紧了：搜索 id 的前半段将不再命中。响应中的 `token_terms` 列出了按整词匹配的词，页面标注为「按整词匹配 · 已走索引」；需要搜索片段时请使用正则模式。
* 短词有意不走索引，搜索 `health` 必须能匹配 `healthcheck`。阈值见 `TOKEN_MIN_LEN`。

### 跳数索引的上限

**ClickHouse 26.2 GA 的文本索引（`TYPE text`）对常见关键字同样无效**，这是 2026-09-10 实测后得出的结论。

判断一个跳数索引的**上限**无须真正建立索引，只需统计「至少命中一次的 granule 有多少个」（`uniqExactIf((_part, intDiv(_part_offset, 8192)), 条件)`）。一小时窗口共 740 个 granule：

| 关键字 | 命中的 granule | 索引最多能节省 |
|---|---|---|
| `青栀`（生僻中文） | 23 / 740 | 32× |
| `发送私信事件监听器` | 740 / 740 | 0 |
| `im_enter_direct_msg` | 740 / 740 | 0 |
| `WX_RECOGNIZE_SHADOW` | 736 / 740 | 0 |
| `sendWebHooksMsgId` | 740 / 740 | 0 |
| `timeout` / `msgId` | ~740 / 740 | 0 |

一个 granule 是 8192 行，约为 5 秒的全量日志（每秒 1600 行，所有服务混在一起）。只要一个词平均每几秒出现一次，它就存在于每个 granule 中，**任何**跳数索引都无法跳过。真正稀疏的是 32 位 id 一类，而这已经由 `idx_message_tokens` 覆盖。（表中的关键字取自 `system.query_log` 中近 7 天用户实际搜索过的词。）

**`ngrambf_v1` 也试过，无效，不必再尝试**：8192 行日志中就有 13 万个不同的 trigram，几乎覆盖了现实中的全部 trigram 空间。取 20 个 granule、6 个真实关键字（含 32 位十六进制 msgId）验证，一个都跳不掉。token 不同，它的取值空间无限大，一个 msgId 只落在真正包含它的一两个 granule 上。

### 关键字检索的扫描成本

用不上索引的关键字（带标点或中文的子串）仍要扫描时间范围内的全部 `message`：2026-09-10 复测**一小时约 10 GB、两小时约 40 GB**（未压缩，全集群；`message` 一列就占全表 911 GB 中的 761 GB，每行 1740 字节）。分片之间并不均衡，两小时的关键字检索在单个分片上就可能触及 `--max-read-bytes` 的 20 GiB 护栏——线上 24 小时内的 14 次 307 错误都由此而来。护栏本身没有问题，要让两小时以上的关键字检索完成，只能调大护栏（40 GiB 量级），或者接受「关键字检索以一小时左右为限」。

这类查询的长尾成因尚未查明：不是 Keeper（复制队列全为 0），不是后台合并（p90 与合并字节数的相关系数为 −0.12），也不是读带宽限流。线上部署因此把 `OPDASH_QUERY_TIMEOUT` 设为 90s，而非默认的 30s。

### 排序键

**日志表的排序键应改为 `(service_name, timestamp, level, trace_id)`，以服务打头**（2026-09-18 的结论，此前为 `(timestamp, level, trace_id)`）。**线上尚未更换**：2026-09-21 查询 `system.tables`，三个分片的 `app_log_local` 仍然是 `(timestamp, level, trace_id)`，下述收益均未兑现。

当初不以服务打头的理由是「近 7 天 12823 次检索中只有 3% 带服务筛选」。到 2026-09 使用方式已经改变：近 7 天 584 次检索中 67% 带服务，关键字检索中 74% 带服务。而「服务 + 关键字」恰恰是代价最高的一类：排序键中没有服务时，服务筛选**完全不减少读量**（`message` 按 granule 整块读取，每个 granule 中都有这个服务），3 小时以上的这类检索 61 次中有 41 次失败（触及 20 GiB 护栏）。

建立一张新排序键的试验表并灌入相同数据进行对比（关闭全部缓存，正常量级的 3 小时窗口、2144 万行）：

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

「无筛选最近 N 条」不能再顺序读取、读满即停，而要把范围内的排序列读出来排序一遍，读量增加 20 倍，但绝对值很小（排序列都很窄），耗时没有变差；主键分析对第二列 `timestamp` 的范围条件仍然有效（generic exclusion search），不会读取整个分区。「只有关键字」失去了提前停止，常见词在理论上会变慢，实测没有测出差异。

更换方法：建立 `app_log_v2`，按分区回灌，用 `EXCHANGE TABLES` 原子交换表名，再补灌交换前后的两个分区。Distributed 表与 logpipe 都按表名写入，无须修改。

### 检索与直方图串行发出

带关键字时，检索与直方图两条查询**串行发出**（2026-09-10 修改）。两条查询的 WHERE 完全相同，而 `message` 没有索引，需要扫描整个时间范围。并发发出就是同一段数据扫描两遍；错开之后，第二条会命中 ClickHouse 26.x 的 **query condition cache**（`use_query_condition_cache`，服务端默认开启，记录「哪些 granule 不满足该条件」）。线上实测第一条 **39.8 GB / 3.5 s**，紧随其后的同条件第二条 **0 GB / 18 ms**。

顺序是「检索在前、直方图在后」：列表是使用者正在等待的部分，不能因直方图而变慢。关键字命中较多时，检索读满 200 行即停止，此时直方图需要自行扫描；两种情况合计，集群大约只扫描一遍。

### 总条数不单独 count()

「共 N 条」不单独执行 `count()`：直方图各桶之和就是总数（时间条件左闭右开，桶按同一原点切分，每一行都落在某个桶中）。日志页给 `/api/logs/search` 传 `count=0` 关闭计数，省下一条扫描同样数据的查询。按 trace id 查询（没有直方图）时才使用 `count()`；关键字检索使用 `exact_rows_before_limit=1`，一次扫描顺带得出总数。

MCP 的 `search_logs` 同样默认 `count=0`（模型需要总数时使用 `log_histogram`，它同时给出时间分布；或显式传入 `count=true`）。

### 按 trace id / span id 查日志要带时间范围

按 trace id 或 span id 查询日志**同样要带时间范围**（2026-09-21 修改，此前的做法相反）。日志表的 **`span_id` 上没有任何索引**；`trace_id` 的 `idx_trace_id` 是 `bloom_filter`，默认误判率 2.5%，摊到 30 天的分区上也只能剪掉约 97%。两条路径不带时间范围都是数十 GB 的全表扫描。关闭 query condition cache 实测：

| 查询 | 读行数 | 读量 | 耗时 |
| --- | --- | --- | --- |
| `span_id=…`，不带时间范围 | 313.5 亿 | 37.9 GiB | 8.8 s |
| `span_id=…` + 1 小时窗口 | 830 万 | 158 MB | 0.18 s |
| `trace_id=…`，不带时间范围 | 10.4 亿 | 5.4 GB | 14 s |
| `trace_id=…` + 样本时刻前后 1 小时 | 21 万 | 15.5 MB | 0.16 s |

因此，拼接链接的地方（`ui/src/lib/links.ts` 的 `logsHref`）一律带上时间窗：有确定时刻时传 `around(ts)`（错误分组的 `last_ms` 与 `sample_trace` 来自同一个 `argMax`，链路详情有 trace 自身的跨度，日志行有 `ts_ms`），只有 id 时传页面当前的范围。日志页按 id 查询时也按页面当前范围裁剪，**相对范围（`range=1h`）同样生效**。此前只认字面写出的 `from` / `to`，于是 `logs?span_id=…&range=1h` 每打开一次就是一次 38 GB 的扫描。窗口没有覆盖到时，空状态上提供「不限时间再查一次」作为兜底，只有这一步才会付出上表第一行的代价。

### 只取能显示的列

日志检索只取**能显示的列**：字符串、数字、时间列，即 `/api/meta` 中提供给前端的维度。线上的 `app_log` 在物理上带着全套 span 列（`resource_attributes JSON`、`events.attributes Array(JSON)` 等，logpipe 不写入，全是默认值），全部取出只会白白读取和传输：一小时窗口取 200 行，0.129 GB → 0.098 GB。

### 直方图分桶

分桶表达式为 `intDiv(toUnixTimestamp64Milli(timestamp) - origin, width)`，原点是范围起点当天的本地零点。不使用 `toStartOfInterval(..., INTERVAL n SECOND)`：它按 UTC 取整，6 小时一桶时边界会落在北京时间 02 点与 08 点。

## 链路

### 链路检索：两次往返

链路检索分两步：先用 `SELECT trace_id ... ORDER BY timestamp DESC LIMIT 1 BY trace_id LIMIT n` 取候选 id，再用 `WHERE trace_id IN {ids:Array(String)} GROUP BY trace_id` 聚合摘要。不使用嵌套子查询，在 Distributed 表上 `distributed_product_mode=deny` 也没有问题。

「请求耗时」取根 span 的耗时（根缺失时退回最早的 span）；「总跨度」是从最早的 span 开始到最晚的 span 结束，异步消费会使它远长于请求耗时。

### 两条查询都不扫描整个时间窗

2026-09-14 修改。修改前，一次「最新 50 条」需要 5248 万行 / 2.1 GB / 4.4 秒，两条查询各占一半，但原因不同。

**候选查询**：`timestamp` 不是排序键前缀，`ORDER BY timestamp DESC` 在 `EXPLAIN` 中是 `Sorting (Sorting for ORDER BY)`，窗口内每一行的 `trace_id + timestamp` 都要读出来重新排序。但最新的 50 条集中在窗口末尾（线上实测这 50 条的时间跨度只有 **16 毫秒**），所以先查最后 1 分钟，不够再查 15 分钟，仍不够才查整个窗口。1 分钟窗口 124 万行，整个 1 小时窗口 3043 万行。最小一级取 1 分钟，是因为时间是第三级排序键，每个 `(service_name, span_name)` 组合都要读一段 granule，5 秒的窗口也要读 103 万行，再缩小已无收益。按耗时排序时不能这样做（最慢的那条可能位于窗口中任何位置）。

**摘要查询**：`trace_id` 上只有 bloom filter（GRANULARITY 4，误报率 2.5%），50 个 id 以 OR 连接，一个索引块存活的概率是 `1 - 0.975⁵⁰ ≈ 72%`，线上 `EXPLAIN indexes=1` 实测为 8513 / 11480，与期望值吻合。也就是说，这一步基本挡不住数据块，**唯一的手段是缩小主键能圈定的时间窗**。因此时间条件锚定在候选自身的跨度上（±10 分钟），而非整个搜索窗口。

收窄会截断持续时间较长的链路（如 MQ 消费），这类链路通过 **`root_count = 0`** 识别后，再按完整窗口补查一次：完整的链路一定有一个 `parent_span_id = ''` 的根 span，窗口中找不到根，说明前段被截断了。补查代价不高，id 数量少了，bloom filter 便重新有效。线上采样 10.7 万条链路，跨度超过 10 分钟的只占 0.02%，但它们的 span 多，被「最新 50 条」选中的概率也高，一页中可能有 0 ~ 4 条。

| 窗口 / 排序 | 扫描行数 | 读量 | ClickHouse 耗时 |
|---|---|---|---|
| 1 小时 · 最新在前 | 5865 万 → **1250 万** | 2.35 → **0.51 GB** | 1451 → **466 ms** |
| 6 小时 · 最新在前 | 8900 万 → **1278 万** | 3.58 → **0.53 GB** | 1677 → **539 ms** |
| 6 小时 · 最慢在前 | 5572 万 → 5560 万 | 2.08 → 2.08 GB | 1549 → 1493 ms |

12 个窗口 × 两种排序逐一核对：返回的 50 个 id 以及每条摘要的每个字段都**与修改前完全一致**。按耗时排序在这一轮基本没有收益（候选分布在整个窗口中，无法收窄），由下一节的两项修改接手。

### `trace_id IN` 放进 PREWHERE；按耗时排序的候选在进程内去重

2026-09-14 修改。上一轮之后，「最慢在前」成为页面上代价最高的一条查询，此次从两处入手。

**摘要查询的 `trace_id IN` 移入 PREWHERE**。自动 PREWHERE 不会选择这个条件（大数组 + String 列不符合它的启发式规则），留在 `WHERE` 中时，ClickHouse 会先读出聚合所需的八列再过滤。移入之后每行只读 `trace_id`，其余列只为命中的行读取：1 小时高峰窗口 **1257 MiB / 2524 ms CPU → 1042 MiB / 1899 ms CPU**（5 次取中位数）。1042 MiB ÷ 3000 万行 = 34.7 字节，恰好是一个 `trace_id`，已经到底；再往下只能调整 bloom filter，而那属于 tracepipe 的表。

**按耗时排序的候选去掉 `LIMIT 1 BY trace_id`**，改为多取 10 倍的行，在 Rust 中去重。带 `LIMIT 1 BY` 时执行计划是普通的 `Sorting`，窗口中每一行的 `trace_id` 都要物化后再排序；去掉之后变为 `Limit (preliminary LIMIT)` + `LazilyReadFromMergeTree`，只读排序列来定位前 n 行，`trace_id` 只为这 n 行读取：**285 MiB / 1310 ms CPU / 927 ms → 189 MiB / 723 ms CPU / 136 ms**。

这一改动**只对按耗时排序有效**：最慢的 span 来自不同的链路，实测 7 个窗口各取 500 行，去重后得到 83 ~ 179 个 trace，都远超 50。按时间排序则不行：最新的 span 扎堆出现（同一条繁忙的链路一毫秒内就能写入好几个 span），同样取 500 行只剩 46 个 trace，不足 50；何况它已经通过逐级探测把窗口缩小到 1 分钟。若取满 `limit × 10` 行仍凑不够（一条链路占据了最慢的几百个 span），则退回 `LIMIT 1 BY`。

| 窗口 / 排序 | 读量 | ClickHouse 耗时 | 端到端 |
|---|---|---|---|
| 1 小时 · 最慢在前 | 1538 → **1235 MiB** | 1389 → **674 ms** | 1541 → **782 ms** |
| 6 小时 · 最慢在前 | 2699 → **2140 MiB** | 2150 → **1114 ms** | 2388 → **1233 ms** |
| 6 小时 · 最新在前 | 635 → **571 MiB** | 673 → **617 ms** | 869 → 889 ms |

12 个窗口 × 两种排序核对，返回的 50 个 id、顺序以及每条摘要的每个字段都一致。（「最新在前」偶尔顺序不同，是由于同一毫秒内的并列：这 50 条分布在 17 个毫秒中，最大的一组 12 条同属一毫秒，**同一条 SQL 自己连续执行 5 次也会换顺序**，与本次修改无关。）

### 耗时分布图没有优化空间

耗时 × 时间分布图使用的 `GROUP BY bucket, lvl` 已测量过，**没有可改进之处**：`EXPLAIN` 中只有一个 `Aggregating`，没有 `Sorting`；1 小时高峰窗口 1159 万行 / 116 MiB / 140 ms，是页面上代价最低的一条查询。按 CPU 时间拆分（5 次取中位数；这台集群的墙钟时间可能相差 2 ~ 3 倍）：基础扫描与 `span_kind` 过滤占 42%，多读一列 `duration_ns` 占 26%，每行一次 `log10` 只占 9%。尝试过收窄分组键类型、把 `(bucket, lvl)` 打包成一个 Int64、用 `roundDown` 常量阈值和 `log2` 换算替代 `log10`、去掉 `countIf`，结果都在 ±10% 的噪声范围内。

### 链路详情不取属性列

2026-09-10 修改。四个 JSON 列（`resource_attributes`、`span_attributes`、`events.attributes`、`links.attributes`）构成了这条查询的**全部**成本。线上同一条查询实测：

| | 读量 | 耗时 |
|---|---|---|
| 带这四列（原来） | 0.259 GB | 1.8 s（同类查询的 p99 为 41.9 s，最慢 43.3 s） |
| 不带（现在的瀑布图） | **0.002 GB** | **50 ms** |

原因不在于这几列数据量大（这条 trace 中它们只有几十 KB），而在于**主键前缀只能圈定到秒级**：`(service_name, span_name, toDateTime(timestamp))`，一条 42 毫秒的 trace 会带入 5 万行候选（若精确到毫秒则只有 108 行），而 JSON 列按 granule 整块读取，每个路径一条流，路径一多，读一个 granule 的固定开销就超过了真正需要的那几行。所以瀑布图只取轻量列，属性等在用户点开某个 span 时再通过 `/api/traces/{id}/spans/{span_id}` 单独获取（0.011 GB / 0.9 s）。该接口接受 `service`、`name`、`ts` 三个主键前缀提示，页面上本已具备，带上即可省去再次定位（0.11 GB → 0.011 GB）。

### 链路详情：两次往返

链路详情同样分两步：先用 `WHERE trace_id = ?` 只读 `span_id, service_name, span_name, timestamp` 进行定位，再按 `service_name IN ... AND span_name IN ... AND timestamp BETWEEN ...` 使用排序键前缀读取 JSON 属性、events、links 等重列。

`trace_id` 只有 bloom filter（GRANULARITY 4，误报率 2.5%），在每天一亿多 span 的数据量下，通过索引的数据块绝大多数是误报（线上 EXPLAIN：18371 个 granule 剩余 476 个，与期望的误报数吻合）。一步读完完整 JSON 需要数 GB、触发 30 秒超时；拆开之后，误报块每行只读几十字节，重列则由主键精确圈定。

第二步不传 span id 列表：参数都放在 URL 中，5000 个 id 会超过 64 KB 的 URI 上限。两步的排序相同，时间区间卡在第一步的首尾毫秒，取同样多的行即是同一批 span。

### 超出上限的 trace

2026-09-16 修改：超出上限时退回以 `at` 为中心的窄窗口，链接指定的 span 单独取回。

定位这一步是 `ORDER BY timestamp LIMIT --max-trace-spans`，超出上限时按时间从早到晚截断。线上的 `1719ae…8dbc` 是 **trace id 被复用**的典型（常驻消费者始终使用同一个 id）：21882 个 span，跨度 2 小时 13 分。按原来的方法取回的是最早的 5000 个，即 15:52 开始的那一段；而用户是带着 `at=16:52` 从错误分组点进来的，要看的 span 位于被截掉的后半段，打开后只有一张无关的瀑布图。现在的处理如下。

1. **某一档窗口放不下时，退回第一个足够用的窗口**（足够用指至少有 `max/10` 个 span，以免 `at` 偏离几分钟时退回一张空图）。窄窗口虽然窄，但以 `at` 为中心、在自身时间段内是完整的，而且代价低得多。同一条 trace 实测：

   | | span 数 | 取数读量 | 关联日志的窗口 |
   |---|---|---|---|
   | 不退回（最早的 5000 个） | 5000 | 16.4 MB | 18 分钟，且要看的 span 不在图中 |
   | 退回到 ±15 分钟 | 4738 | 34.3 MB | 39 分钟 / 1.6 GB |
   | **退回到 ±1 分钟（现在）** | **701** | **3.8 MB** | **11 分钟 / 0.28 GB** |

   响应中的 `narrowed` 与 `window_from_ms` / `window_to_ms` 告知前端只显示了哪一段，页面上的标记会写明。
2. **明知放不下时，不发不限时间的那一档**（全表扫描，线上 5.1 GB / 39 s）：上一档已经装入超过一半的上限，扫描回来也只会被截断，最终仍退回窄窗口。
3. **详情接口接受 `span=`**：若指定的 span 仍不在结果中（位于窗口之外，或第一档就放不下、只能按时间截断），再发一条 `WHERE trace_id = ? AND span_id = ? LIMIT 1`（使用同一套探测窗口，代价与定位同量级），取回后追加在 `spans` 末尾，并在 `pinned_span` 中注明。它的父级链路不一定在图中，瀑布图上显示为「父级缺失」。若没有被截断、窗口也已探测到最大一档，则不再追加不限时间的一档：窗口中的 span 已经完整，为一个可能抄错的 id 扫描全部分区并不值得。`/api/traces/{id}/spans/{span_id}` 使用同样的定位方式——不带主键前缀提示时，它原先定位整条 trace 再从中挑选，在超出上限的 trace 上会找不到目标。
4. **没带 `at` 时以当前时刻为中心向前探测**（2026-09-20 新增）：原先没有 `at` 就没有中心点，直接发出不限时间的一档。线上近 24 小时内走过兜底的 11 条 trace 中，有 8 条**一次带窗口的查询都没有发过**（即根本没带 `at`），而它们恰好是最慢的几条——86.9 s / 36.7 s / 28.8 s / 20.0 s / 19.6 s，合计占这个接口 97% 的读量。而手动点开的 trace 几乎都是刚发生的：这 8 条中有 7 条在被查询时发生于 75 分钟以内（最近的一条仅隔 36 秒），最早的一条相隔 13.3 小时。所以改为以 `now()` 为中心，依次探测 ±15 分钟、±2 小时、±26 小时，都未命中才落到不限时间的一档。窗口的后一半落在未来，分区尚不存在，不产生开销；前后对称，是因为「命中」要通过「未贴着窗口边缘」这一检查，向后留得太少，刚发生的 trace 会贴着 `now` 被判为未命中。线上实测（用一个不存在的 id 测量白跑的代价）：0.31 s / 8.4 MB、0.33 s / 59.7 MB、2.29 s / 574.6 MB。三档全部未命中也只多花 2.9 s，而命中一档是 0.3 s 对比 25.8 s。

整页实测（线上那条 trace，带 `at` 与 `span`）：详情 1.5 s + 关联日志 0.40 s + span 属性 0.81 s ≈ 3.0 s。修改前单是关联日志一趟就需要 1.42 s / 1.6 GB，而且那个 span 根本无法选中。

### trace_id 的 bloom filter 索引

上一节问题的根源在索引上，**已于 2026-09-20 修复**。`otel_trace` 原先只有一个 `idx_trace_id bloom_filter GRANULARITY 4`，使用**默认 2.5% 的误报率**：不带时间条件时，线上 EXPLAIN 1289480 个 granule 只筛剩 29384 个（2.28%，与误报率一致，几乎全是误报），一条 126 个 span、跨度仅 192 ms 的 trace 需要读取 1.22 亿行 / 4.0 GB / 25.8 s。

现在增加了第二个索引 `idx_trace_id_v2 trace_id TYPE bloom_filter(0.001) GRANULARITY 4`，**两个同时使用**（两个 bloom filter 的误报率相乘）：

| 使用的索引 | 通过索引的 granule | 读行数 | 读字节 |
|---|---|---|---|
| 只有旧索引（修改前） | 29384 / 1289480 | 1.22 亿 | 4.08 GB |
| 只有新索引 | 1264 / 1289508 | 506 万 | 169 MB |
| **两个都用（现在）** | **112 / 1289480** | **41 万** | **13.7 MB** |

所以旧索引不要删除：它每个分片多占 788 MiB，换来的是读量再减少 12 倍。两个索引合计每分片 2.2 GiB，数据每分片 112 GiB。全表 `MATERIALIZE INDEX` 三个分片同时执行，85 秒完成，part 数与磁盘占用几乎没有变化（官方文档称 mutation 会「重写整个 part」，实测只写索引文件，其余列通过硬链接保留）。新建表若要一步到位，一个 `bloom_filter(0.000025)` 与这两个索引叠加的效果等价、占用也相同（位数与 `-ln(p)` 成正比，`0.025 × 0.001` 与 `0.000025` 的结果一致）。

## 指标

* **累积量的速率在查询时相减得出。** metricpipe 按 OTLP 原样存储，counter 是进程启动以来的累计值（`temporality = 'Cumulative'`）。当初选择不在采集端转换为 delta，是因为多副本路由下难以做对。因此 `agg=rate` / `increase` 的 SQL 为：取桶内最后一个累计值 → 用 `lagInFrame` 取上一个桶的值 → 相减。`cur < prev` 视为进程重启（计数器归零），按 Prometheus 的做法把当前值整体计为增量。`Delta` 类型在桶内求和即可，两种 temporality 在同一条 SQL 中用 `if(temp = 'Cumulative', ...)` 分别处理，不必先查询一次才知道是哪一种。速率除以**两个点之间的实际间隔**而非桶宽：上报周期 60s、步长 30s 时，除以桶宽会使速率减半。
* **相减必须按时间线进行，而时间线包括 resource 属性。** 同一服务的两个 pod 上报的是两条独立的计数器，混在一起相减会得到一串负数（进而被误判为重启）。表中没有 series_id 列，只能现场计算 `cityHash64(service_name, scope_name, toString(resource_attributes), toString(attributes))`，代价是把两个 JSON 属性列整列读出。查询已锁定单个 `metric_name`，读取的行数有限，这一代价可以接受；只做 `avg`、`last` 这类不需要相减的聚合时不计算这一步。
* **直方图分位数**：各时间线的 `bucket_counts` **先按时间线相减，再逐元素相加**（`sumForEach`），最后在服务端根据桶计数与 `explicit_bounds` 插值。分位数不能对多条时间线取平均，那等于把 p95 再平均一次，没有意义。桶边界不同的时间线无法合并，因此 `explicit_bounds` 也进入分组键：真的出现两套边界时就画成两条线，而不是悄无声息地算错。
* **查询前先确认指标类型。** 五种类型共用一张表，用不上的列保留默认值，所以直方图行的 `value` 是 0：用 gauge 的「`avg` + `value`」去查询 `jvm.gc.duration` 不会报错，只会返回一串 0——线上曾因此得出「整个集群没有 GC」的错误结论。`/api/metrics/query` 现在会先发一条 `LIMIT 1`（与主查询相同的 WHERE 前缀，几乎没有开销），取得 `metric_type`、`is_monotonic` 以及是否有 `explicit_bounds`：未给出 `agg` / `field` 时按类型选择（与页面上 `aggOptions` 的第一项一致：带桶的直方图→分位数，指数直方图与 Summary→`mean` + `sum`，counter→`rate`，gauge→`avg`）；给出的组合必然查不到数据时（直方图 `field=value`、非直方图 `agg=quantile`、指数直方图求分位数），直接返回 400 并说明正确的查法。在主查询之前拒绝，也省去了那一次查询。
* **时间线过多时只画最大的 N 条**：聚合之后用 `dense_rank() OVER (ORDER BY total DESC)` 截断，`total` 为 `sum(abs(v)) OVER (PARTITION BY keys)`。改为两次往返（先查前 N 条的键，再查它们的数据点）反而要多扫描一遍表。截断后响应中带 `truncated = true`，页面提示增加过滤条件。
* **ClickHouse 26.x 中累积量相减的类型问题**：`UInt64 - UInt64` 的结果是 **Int64**，与另一分支的 UInt64 无法得出公共类型，`if` 会返回 `Variant(Int64, UInt64)`，外层的 `sumForEach` 直接报错 43（`Illegal type Variant(Array(UInt64), Array(Variant(Int64, UInt64)))`）。`toUInt64()` 必须写在**分支内部**（`if(c >= p, toUInt64(c - p), c)`），写在 `if` 外面也不行，因为 `toUInt64` 不接受 Variant。
* **指标目录只扫描最近 6 小时**（`/api/metrics`），即使页面选择的是 30 天：它要读取整段范围内的 `metric_name` / `service_name`，而「有哪些指标」看最近几小时就足够。若确有只在凌晨上报一次的指标，把时间范围整体移过去即可看到；响应中的 `from_ms` 是实际扫描的窗口，页面上有标注。
* **指标页不强制先选服务**：指标表的排序键是 `(service_name, metric_name, toDateTime(timestamp))`，`metric_name` 上还有 bloom filter，不指定服务时可以依靠索引跳过 granule。
* **标签过滤与链路页的写法相同**：使用子列标识符 `` attributes.`http.route` ``，值一律 `toString(...)` 后再比较（同一个键在不同服务中可能是整数也可能是字符串，直接比较会报 `NO_COMMON_TYPE`）。
* **exemplar 先过滤再展开**：先在源行上用 `notEmpty(exemplars.trace_id)` 过滤，再 `ARRAY JOIN` 展开；顺序反过来会把每一行的空数组也展开一遍。按值从大到小排序，最慢的几次排在最前。

## 服务概览

* 分位数使用 `quantilesTDigest`，而非默认的 `quantiles`：后者是 8192 个样本的水塘抽样，尾部分位数最不准确。
* 其余取舍（对比窗口、`MIN_SAMPLES`、每个服务最多 200 个接口）见 [design.md](design.md#首页服务总览)。
