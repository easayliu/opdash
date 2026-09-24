//! Elasticsearch 数据源（OpenSearch 的这几个接口是同一套，也能用）。
//!
//! 只读靠「URL 由 opdash 拼」：模型给的只有索引名和查询体，能到达的只有 `_search`、`_sql`、
//! `_mapping`、`_cat/indices`、`_stats/search`、`_tasks` 这几个读接口。索引名里不许有 `/`、`?`、
//! `#`，拼不出别的路径；`_delete_by_query` 这类写接口根本不在代码里。
//!
//! 查询两种写法：以 `{` 开头的是查询 DSL（发到 `<index>/_search`），否则当 ES SQL（`_sql`，
//! 它本身就只能读）。

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Map, Value, json};

use super::sql::{self, Dialect};
use super::{QueryOpts, Resolved, SlowOpts, Source, Table, clip};
use crate::error::{Error, Result};

pub struct Elastic {
    http: reqwest::Client,
    base: String,
    auth: Option<String>,
}

impl Elastic {
    pub fn new(r: &Resolved) -> std::result::Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(r.timeout)
            .connect_timeout(super::CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("建 HTTP 客户端失败: {e}"))?;
        let auth = match (&r.api_key, &r.user) {
            (Some(k), _) => Some(format!("ApiKey {k}")),
            (None, Some(u)) => {
                use base64::Engine;
                let raw = format!("{u}:{}", r.password.as_deref().unwrap_or(""));
                Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)))
            }
            (None, None) => None,
        };
        Ok(Self { http, base: r.url.trim_end_matches('/').to_owned(), auth })
    }

    async fn send(
        &self,
        src: &Source,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}/{path}", self.base));
        if let Some(a) = &self.auth {
            req = req.header(AUTHORIZATION, a);
        }
        if let Some(b) = body {
            req = req.header(CONTENT_TYPE, "application/json").body(b.to_string());
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Source {
                    status: 504,
                    message: format!(
                        "Elasticsearch 数据源 {} 超过 {} 秒没有响应",
                        src.name,
                        src.timeout.as_secs()
                    ),
                }
            } else {
                Error::Source {
                    status: 502,
                    message: format!(
                        "Elasticsearch 数据源 {}（{}）不可用: {e}",
                        src.name, self.base
                    ),
                }
            }
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Source {
            status: 502,
            message: format!("读取 Elasticsearch 响应失败: {e}"),
        })?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!(text));
        if status.is_success() {
            return Ok(value);
        }
        // {"error": {"type": "...", "reason": "...", "root_cause": [...]}, "status": 400}
        let err = &value["error"];
        let reason = err["root_cause"][0]["reason"]
            .as_str()
            .or_else(|| err["reason"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| clip(&text, 500));
        let ty =
            err["root_cause"][0]["type"].as_str().or_else(|| err["type"].as_str()).unwrap_or("");
        Err(Error::Source {
            status: match status.as_u16() {
                401 | 403 => 400,
                404 => 400,
                408 | 504 => 504,
                s if s >= 500 => 502,
                _ => 400,
            },
            message: format!(
                "Elasticsearch 错误 {}{}: {reason}",
                status.as_u16(),
                if ty.is_empty() { String::new() } else { format!(" {ty}") }
            ),
        })
    }

    pub async fn tables(&self, src: &Source, pattern: Option<&str>, limit: u32) -> Result<Value> {
        let pattern = pattern.filter(|p| !p.is_empty());
        if let Some(p) = pattern {
            check_index(p)?;
        }
        let path = format!(
            "_cat/indices/{}?format=json&h=index,health,status,docs.count,store.size&s=index&expand_wildcards=open",
            pattern.unwrap_or("*")
        );
        let v = self.send(src, reqwest::Method::GET, &path, None).await?;
        let mut table = Table::new(&["index", "health", "docs", "size"]);
        // 以点开头的是系统索引，不按名字问就不列
        let show_hidden = pattern.is_some_and(|p| p.starts_with('.'));
        for row in v.as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = row["index"].as_str().unwrap_or("");
            if name.starts_with('.') && !show_hidden {
                continue;
            }
            if table.rows.len() >= limit as usize {
                table.truncated = true;
                break;
            }
            table.rows.push(vec![
                json!(name),
                row["health"].clone(),
                row["docs.count"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|n| json!(n))
                    .unwrap_or(Value::Null),
                row["store.size"].clone(),
            ]);
        }
        Ok(json!({ "indices": table }))
    }

    pub async fn describe(&self, src: &Source, index: &str) -> Result<Value> {
        check_index(index)?;
        let v = self.send(src, reqwest::Method::GET, &format!("{index}/_mapping"), None).await?;
        let mut out = Map::new();
        // 通配符会命中多个索引；同一类索引的映射通常一样，最多给三个
        for (name, m) in v.as_object().into_iter().flatten().take(3) {
            let mut fields = Table::new(&["field", "type"]);
            flatten_mapping("", &m["mappings"]["properties"], &mut fields.rows);
            out.insert(name.clone(), json!(fields));
        }
        let total = v.as_object().map(|o| o.len()).unwrap_or(0);
        let mut result = json!({ "index": index, "mappings": out });
        if total > 3 {
            result["note"] = json!(format!("{index} 命中了 {total} 个索引，只列出前 3 个的映射"));
        }
        Ok(result)
    }

    pub async fn query(&self, src: &Source, query: &str, opts: &QueryOpts) -> Result<Value> {
        if query.starts_with('{') {
            let index = opts.index.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
                Error::bad_request("查询 DSL 需要给 index（索引名或通配符，如 order-*）")
            })?;
            check_index(index)?;
            let mut body: Value = serde_json::from_str(query)
                .map_err(|e| Error::bad_request(format!("查询 DSL 不是合法 JSON: {e}")))?;
            let Some(obj) = body.as_object_mut() else {
                return Err(Error::bad_request("查询 DSL 应是 JSON 对象"));
            };
            let size =
                obj.get("size").and_then(Value::as_u64).unwrap_or(u64::from(opts.limit.min(20)));
            obj.insert("size".into(), json!(size.min(u64::from(opts.limit))));
            obj.entry("timeout").or_insert_with(|| json!(format!("{}s", src.timeout.as_secs())));
            let v = self
                .send(src, reqwest::Method::POST, &format!("{index}/_search"), Some(&body))
                .await?;
            let hits: Vec<Value> = v["hits"]["hits"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .map(|h| {
                    let mut o =
                        json!({ "_index": h["_index"], "_id": h["_id"], "_source": h["_source"] });
                    if !h["highlight"].is_null() {
                        o["highlight"] = h["highlight"].clone();
                    }
                    o
                })
                .collect();
            let mut out = json!({
                "took_ms": v["took"],
                "total": v["hits"]["total"],
                "timed_out": v["timed_out"],
                "hits": hits,
            });
            if !v["aggregations"].is_null() {
                out["aggregations"] = v["aggregations"].clone();
            }
            return Ok(out);
        }
        // ES SQL：接口本身只读，校验只是为了把 DELETE 这类写法早点、说清楚地拦下来
        let stmt = sql::check_read_only(query, Dialect::Mysql).map_err(Error::bad_request)?;
        let body = json!({
            "query": stmt,
            "fetch_size": opts.limit,
            "request_timeout": format!("{}s", src.timeout.as_secs()),
        });
        let v = self.send(src, reqwest::Method::POST, "_sql?format=json", Some(&body)).await?;
        let columns: Vec<String> = v["columns"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|c| c["name"].as_str().unwrap_or("").to_owned())
            .collect();
        let mut rows: Vec<Vec<Value>> = v["rows"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect();
        let mut truncated = false;
        // 有游标说明后面还有，关掉它，不留在集群上占资源
        if let Some(cursor) = v["cursor"].as_str() {
            truncated = true;
            let _ = self
                .send(src, reqwest::Method::POST, "_sql/close", Some(&json!({ "cursor": cursor })))
                .await;
        }
        if rows.len() > opts.limit as usize {
            rows.truncate(opts.limit as usize);
            truncated = true;
        }
        Ok(json!({ "columns": columns, "rows": rows, "truncated": truncated }))
    }

    pub async fn slow(&self, src: &Source, opts: &SlowOpts) -> Result<Value> {
        let stats = self.send(src, reqwest::Method::GET, "_stats/search", None).await?;
        let mut per_index: Vec<(String, u64, u64, u64, u64, u64)> = stats["indices"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(name, _)| !name.starts_with('.'))
            .map(|(name, s)| {
                let q = &s["total"]["search"];
                let n = |k: &str| q[k].as_u64().unwrap_or(0);
                (
                    name.clone(),
                    n("query_total"),
                    n("query_time_in_millis"),
                    n("fetch_total"),
                    n("fetch_time_in_millis"),
                    n("query_current"),
                )
            })
            .filter(|r| r.1 > 0)
            .collect();
        per_index.sort_by(|a, b| b.2.cmp(&a.2));
        let mut indices = Table::new(&[
            "index",
            "queries",
            "query_avg_ms",
            "query_total_s",
            "fetch_avg_ms",
            "running",
        ]);
        for (name, qn, qt, fn_, ft, cur) in per_index.iter().take(opts.limit as usize) {
            let avg = |t: u64, n: u64| {
                if n == 0 { 0.0 } else { (t as f64 / n as f64 * 100.0).round() / 100.0 }
            };
            indices.rows.push(vec![
                json!(name),
                json!(qn),
                json!(avg(*qt, *qn)),
                json!(qt / 1000),
                json!(avg(*ft, *fn_)),
                json!(cur),
            ]);
        }
        indices.truncated = per_index.len() > opts.limit as usize;
        let tasks = self
            .send(src, reqwest::Method::GET, "_tasks?actions=*search*&detailed=true", None)
            .await?;
        let mut running = Table::new(&["task", "running_ms", "action", "description"]);
        for n in tasks["nodes"].as_object().into_iter().flatten().map(|(_, n)| n) {
            for (id, t) in n["tasks"].as_object().into_iter().flatten() {
                let ms = t["running_time_in_nanos"].as_u64().unwrap_or(0) as f64 / 1e6;
                if ms < opts.min_ms {
                    continue;
                }
                running.rows.push(vec![
                    json!(id),
                    json!(ms.round()),
                    t["action"].clone(),
                    json!(clip(t["description"].as_str().unwrap_or(""), 1000)),
                ]);
            }
        }
        running
            .rows
            .sort_by(|a, b| b[1].as_f64().unwrap_or(0.0).total_cmp(&a[1].as_f64().unwrap_or(0.0)));
        running.rows.truncate(50);
        Ok(json!({
            "indices": indices,
            "running": running,
            "notes": [
                "indices 是节点启动以来的累计值，不受时间范围影响；query_avg_ms 高的索引优先看",
                "Elasticsearch 的慢查询日志只写在节点日志文件里（index.search.slowlog）；若节点日志已采集进 opdash，可用 search_logs 搜 slowlog",
            ],
        }))
    }
}

/// 索引名（可带通配符和逗号）只能是这些字符，拼进 URL 路径时不会变成别的接口。
fn check_index(index: &str) -> Result<()> {
    let ok = !index.is_empty()
        && index.len() <= 255
        && !index.starts_with('_')
        && index
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '*' | ',' | '+'));
    if ok {
        Ok(())
    } else {
        Err(Error::bad_request(format!(
            "索引名 {index:?} 不合法：只能包含字母、数字、- _ . * ,，且不能以 _ 开头"
        )))
    }
}

/// 映射里的 `properties` 递归拍平：`user.name → keyword`。`fields`（多字段）也列出来，
/// 写查询时要知道 `name.keyword` 存不存在。
fn flatten_mapping(prefix: &str, props: &Value, out: &mut Vec<Vec<Value>>) {
    let Some(map) = props.as_object() else { return };
    for (k, v) in map {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let ty =
            v["type"].as_str().unwrap_or(if v["properties"].is_object() { "object" } else { "" });
        out.push(vec![json!(path), json!(ty)]);
        if let Some(sub) = v["fields"].as_object() {
            for (fk, fv) in sub {
                out.push(vec![json!(format!("{path}.{fk}")), fv["type"].clone()]);
            }
        }
        flatten_mapping(&path, &v["properties"], out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_cannot_escape_the_path() {
        check_index("order-*").unwrap();
        check_index("logs-2026.09.24,logs-2026.09.23").unwrap();
        assert!(check_index("a/_delete_by_query").is_err());
        assert!(check_index("_all").is_err());
        assert!(check_index("a?x=1").is_err());
        assert!(check_index("").is_err());
    }

    #[test]
    fn flattens_nested_mappings() {
        let props = json!({
            "user": { "properties": { "name": { "type": "text", "fields": { "keyword": { "type": "keyword" } } } } },
            "amount": { "type": "scaled_float" },
        });
        let mut rows = Vec::new();
        flatten_mapping("", &props, &mut rows);
        let names: Vec<String> = rows
            .iter()
            .map(|r| format!("{}={}", r[0].as_str().unwrap(), r[1].as_str().unwrap_or("")))
            .collect();
        assert_eq!(
            names,
            ["amount=scaled_float", "user=object", "user.name=text", "user.name.keyword=keyword"]
        );
    }
}
