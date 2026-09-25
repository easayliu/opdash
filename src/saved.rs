//! 收藏的查询：一个 JSON 文件（`--saved-query-file`），每条记的是**页面地址**——路径加查询串。
//!
//! 页面的全部状态本来就在 URL 里（筛选条件、时间范围、排序），所以「收藏一条查询」就是把
//! `/logs?q=timeout&level=ERROR&service_name=order` 这样一段地址存下来、起个名字。日志 / 链路 /
//! 错误 / 指标 / 服务概览五个页面用同一套，前端也不用给每种筛选状态各写一份序列化。
//!
//! **按用户区分**：每条归在收藏它的那个账号名下（[`crate::auth::Identity::user`]，OIDC 的
//! `preferred_username` 或 Basic 的用户名），列表只列本人的，改 / 删别人的和不存在的一样是 404。
//! 没开认证的部署没有「用户」，所有人共用一份，记在 [`SHARED_OWNER`] 名下。
//!
//! 为什么是文件不是 ClickHouse、多副本怎么办：和 API key 一样，见 [`crate::auth::apikey`]。
//! 这里每次改动都直接落盘，没有「攒着写」的字段，所以不需要后台任务。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::auth::session::now_secs;

/// 没开认证时的「用户」：所有人共用。`*` 不可能是 OIDC / Basic 的账号名。
pub const SHARED_OWNER: &str = "*";
/// 名字最长多少字符。
pub const NAME_MAX: usize = 64;
/// 路径最长多少字节。页面路径就那几个，服务详情带着服务名也长不到哪去。
pub const PATH_MAX: usize = 256;
/// 查询串最长多少字节。链路页的 `attr=` 能给好几个，日志页十几个维度全选上也远不到这个数。
pub const QUERY_MAX: usize = 4096;
/// 每个人最多收藏多少条。收藏是给常用查询用的，几百条自己都找不着；这是防误用的兜底。
pub const PER_USER_MAX: usize = 200;
/// 文件格式版本，改结构时用来迁移。
const FILE_VERSION: u32 = 1;

/// 一条收藏。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedQuery {
    /// 12 位 hex
    pub id: String,
    /// 归谁：账号名，或 [`SHARED_OWNER`]
    pub user: String,
    /// 用户起的名字
    pub name: String,
    /// 页面路径，`/logs` / `/traces` / `/services/order` 这样，以 `/` 开头
    pub path: String,
    /// URL 查询串，不带开头的 `?`，可以为空
    pub query: String,
    /// unix 秒
    pub created_at: i64,
    pub updated_at: i64,
}

/// 为什么没存上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// 名字 / 路径 / 查询串不合法，附一句能给用户看的话
    Invalid(String),
    /// 同一个人已经收藏过一模一样的地址；附那条的 id
    Duplicate(String),
    /// 这个人收藏的太多了
    TooMany,
    /// 文件读写失败
    Store(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Invalid(m) => f.write_str(m),
            SaveError::Duplicate(_) => write!(f, "这条查询已经收藏过了"),
            SaveError::TooMany => write!(f, "收藏已达上限（{PER_USER_MAX} 条），请先删除部分收藏"),
            SaveError::Store(e) => write!(f, "收藏文件读写失败：{e}"),
        }
    }
}

/// 新建 / 修改时给的字段。修改时都可选：只给 `name` 是改名，`path` + `query` 一起给是换成新的地址。
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub path: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    queries: Vec<SavedQuery>,
}

struct State {
    queries: Vec<SavedQuery>,
    /// 上次读 / 写文件时它的 mtime；别的副本改了文件就对不上，重读
    mtime: Option<SystemTime>,
}

pub struct SavedQueryStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl SavedQueryStore {
    /// 打开（没有就建一个空的）。建不出来、读不懂都在这里报，启动就失败。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)
                .map_err(|e| format!("无法创建 {} 所在的目录：{e}", path.display()))?;
        }
        let store = Self { path, state: Mutex::new(State { queries: Vec::new(), mtime: None }) };
        {
            let mut st = store.state.lock();
            if store.path.exists() {
                store.load_into(&mut st)?;
            } else {
                // 现在就写一个空文件：目录不可写这类问题启动时就暴露
                store.save(&mut st)?;
            }
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_into(&self, st: &mut State) -> Result<(), String> {
        let raw =
            fs::read(&self.path).map_err(|e| format!("无法读取 {}: {e}", self.path.display()))?;
        let file: FileFormat = if raw.iter().all(u8::is_ascii_whitespace) {
            FileFormat::default()
        } else {
            serde_json::from_slice(&raw)
                .map_err(|e| format!("{} 不是 opdash 的收藏文件：{e}", self.path.display()))?
        };
        if file.version > FILE_VERSION {
            return Err(format!(
                "{} 由更新版本的 opdash 写入（version {}），当前版本最高只支持 {FILE_VERSION}",
                self.path.display(),
                file.version
            ));
        }
        st.queries = file.queries;
        st.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    /// 文件被别人（另一个副本、或者管理员手改）动过就重读。
    fn reload_if_changed(&self, st: &mut State) {
        let now = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if now.is_some()
            && now != st.mtime
            && let Err(e) = self.load_into(st)
        {
            tracing::warn!(error = %e, "重读收藏文件失败，继续用内存里的");
        }
    }

    /// 写临时文件再 rename，不会留下半个文件。
    fn save(&self, st: &mut State) -> Result<(), String> {
        let file = FileFormat { version: FILE_VERSION, queries: st.queries.clone() };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json).map_err(|e| format!("无法写入 {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("替换 {} 失败：{e}", self.path.display()))?;
        st.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    /// `user` 自己的收藏，新的在前。
    pub fn list(&self, user: &str) -> Vec<SavedQuery> {
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let mut out: Vec<SavedQuery> =
            st.queries.iter().rev().filter(|q| q.user == user).cloned().collect();
        out.sort_by_key(|q| std::cmp::Reverse(q.created_at));
        out
    }

    /// 给 `user` 存一条。`name` 空的话就用地址本身当名字。
    pub fn create(
        &self,
        user: &str,
        name: &str,
        path: &str,
        query: &str,
    ) -> Result<SavedQuery, SaveError> {
        let path = clean_path(path)?;
        let query = clean_query(query)?;
        let name = clean_name(name).unwrap_or_else(|| default_name(&path, &query));
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let mine = st.queries.iter().filter(|q| q.user == user);
        if let Some(dup) = mine.clone().find(|q| q.path == path && q.query == query) {
            return Err(SaveError::Duplicate(dup.id.clone()));
        }
        if mine.count() >= PER_USER_MAX {
            return Err(SaveError::TooMany);
        }
        let now = now_secs();
        let saved = SavedQuery {
            id: fresh_id(&st.queries),
            user: user.to_owned(),
            name,
            path,
            query,
            created_at: now,
            updated_at: now,
        };
        st.queries.push(saved.clone());
        self.save(&mut st).map_err(SaveError::Store)?;
        Ok(saved)
    }

    /// 改 `user` 自己的一条：改名和 / 或换地址。不是他的 / 不存在 → `Ok(None)`，不区分。
    pub fn update(
        &self,
        user: &str,
        id: &str,
        patch: Patch,
    ) -> Result<Option<SavedQuery>, SaveError> {
        let name = patch.name.as_deref().map(clean_name);
        let addr = match (patch.path, patch.query) {
            (None, None) => None,
            (Some(p), Some(q)) => Some((clean_path(&p)?, clean_query(&q)?)),
            _ => {
                return Err(SaveError::Invalid("path 与 query 须同时提供".into()));
            }
        };
        if name.is_none() && addr.is_none() {
            return Err(SaveError::Invalid(
                "没有需要修改的字段：请提供 name，或同时提供 path 与 query".into(),
            ));
        }
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let Some(idx) = st.queries.iter().position(|q| q.id == id && q.user == user) else {
            return Ok(None);
        };
        if let Some((p, q)) = &addr
            && let Some(dup) = st
                .queries
                .iter()
                .find(|o| o.user == user && o.id != id && &o.path == p && &o.query == q)
        {
            return Err(SaveError::Duplicate(dup.id.clone()));
        }
        let entry = &mut st.queries[idx];
        if let Some(name) = name {
            // 改成空名字 = 让它按地址起名
            entry.name = name.unwrap_or_else(|| default_name(&entry.path, &entry.query));
        }
        if let Some((p, q)) = addr {
            entry.path = p;
            entry.query = q;
        }
        entry.updated_at = now_secs();
        let saved = entry.clone();
        self.save(&mut st).map_err(SaveError::Store)?;
        Ok(Some(saved))
    }

    /// 删 `user` 自己的一条。不是他的 / 不存在 → `Ok(false)`，不区分。
    pub fn delete(&self, user: &str, id: &str) -> Result<bool, String> {
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let before = st.queries.len();
        st.queries.retain(|q| !(q.id == id && q.user == user));
        if st.queries.len() == before {
            return Ok(false);
        }
        self.save(&mut st)?;
        Ok(true)
    }
}

/// 去掉首尾空白、截到 [`NAME_MAX`] 个字符；空的返回 None。
fn clean_name(raw: &str) -> Option<String> {
    let s: String = raw.trim().chars().filter(|c| !c.is_control()).take(NAME_MAX).collect();
    (!s.is_empty()).then_some(s)
}

/// 没起名字就叫地址：`/logs?q=timeout`，超长截断。
fn default_name(path: &str, query: &str) -> String {
    let full = if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") };
    let mut s: String = full.chars().take(NAME_MAX).collect();
    if s.len() < full.len() {
        s.pop();
        s.push('…');
    }
    s
}

/// 站内路径：`/` 开头、不是 `//`（那是协议相对地址，会跳去别的站）、不带 `?` / `#` / `\`（浏览器把 `\`
/// 当 `/`，`/\evil.example` 也是协议相对地址）、没有空白和控制字符。
fn clean_path(raw: &str) -> Result<String, SaveError> {
    let s = raw.trim();
    if s.is_empty() || !s.starts_with('/') || s.starts_with("//") {
        return Err(SaveError::Invalid("path 应是站内路径，以 / 开头，如 /logs".into()));
    }
    if s.len() > PATH_MAX {
        return Err(SaveError::Invalid(format!("path 太长（最多 {PATH_MAX} 字节）")));
    }
    if s.chars().any(|c| matches!(c, '?' | '#' | '\\') || c.is_whitespace() || c.is_control()) {
        return Err(SaveError::Invalid(
            "path 中不能包含 ?、#、\\、空白或控制字符，查询串请放在 query 中".into(),
        ));
    }
    Ok(s.to_owned())
}

/// 查询串：开头的 `?` 去掉，不带 `#`，没有控制字符。空的也行（比如「服务概览」本身）。
fn clean_query(raw: &str) -> Result<String, SaveError> {
    let s = raw.trim().trim_start_matches('?');
    if s.len() > QUERY_MAX {
        return Err(SaveError::Invalid(format!("query 太长（最多 {QUERY_MAX} 字节）")));
    }
    if s.chars().any(|c| c == '#' || c.is_control()) {
        return Err(SaveError::Invalid("query 中不能包含 # 或控制字符".into()));
    }
    Ok(s.to_owned())
}

/// 12 位 hex，和已有的不重（6 字节随机数撞上的概率可以忽略，但查一下不费事）。
fn fresh_id(existing: &[SavedQuery]) -> String {
    loop {
        let id = hex(&random_bytes(6));
        if !existing.iter().any(|q| q.id == id) {
            return id;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_bytes(n: usize) -> Vec<u8> {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut buf).expect("system rng");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "opdash-saved-{tag}-{}-{}.json",
            std::process::id(),
            now_secs()
        ))
    }

    #[test]
    fn create_list_update_delete_are_per_user() {
        let path = temp_path("roundtrip");
        let store = SavedQueryStore::open(&path).unwrap();
        let a = store.create("alice", " 订单超时 ", "/logs", "?q=timeout&level=ERROR").unwrap();
        assert_eq!(a.name, "订单超时", "名字去首尾空白");
        assert_eq!(a.query, "q=timeout&level=ERROR", "开头的 ? 去掉");
        assert_eq!(a.id.len(), 12);
        let b = store.create("alice", "", "/services/order", "").unwrap();
        assert_eq!(b.name, "/services/order", "没起名就叫地址");
        store.create("bob", "bob 的", "/logs", "q=timeout&level=ERROR").unwrap();

        // 列表只有自己的，新的在前
        let names: Vec<String> = store.list("alice").into_iter().map(|q| q.name).collect();
        assert_eq!(names, ["/services/order", "订单超时"]);
        assert_eq!(store.list("bob").len(), 1);
        assert!(store.list("carol").is_empty());

        // 同一个人同一个地址不重复存；别人存同样的地址没关系
        assert_eq!(
            store.create("alice", "又一条", "/logs", "q=timeout&level=ERROR"),
            Err(SaveError::Duplicate(a.id.clone()))
        );

        // 改名 / 换地址；别人的改不了
        let renamed = store
            .update("alice", &a.id, Patch { name: Some("超时".into()), ..Patch::default() })
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "超时");
        assert_eq!(renamed.query, a.query);
        let moved = store
            .update(
                "alice",
                &a.id,
                Patch {
                    name: None,
                    path: Some("/traces".into()),
                    query: Some("service=order".into()),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!((moved.path.as_str(), moved.query.as_str()), ("/traces", "service=order"));
        assert_eq!(moved.name, "超时", "只换地址不动名字");
        assert!(
            store
                .update("bob", &a.id, Patch { name: Some("x".into()), ..Patch::default() })
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.update("alice", &a.id, Patch::default()),
            Err(SaveError::Invalid(
                "没有需要修改的字段：请提供 name，或同时提供 path 与 query".into()
            ))
        );
        assert!(matches!(
            store.update("alice", &a.id, Patch { path: Some("/x".into()), ..Patch::default() }),
            Err(SaveError::Invalid(_))
        ));
        // 换成另一条已收藏的地址也算重复
        assert_eq!(
            store.update(
                "alice",
                &a.id,
                Patch { name: None, path: Some("/services/order".into()), query: Some("".into()) }
            ),
            Err(SaveError::Duplicate(b.id.clone()))
        );

        assert!(!store.delete("bob", &a.id).unwrap(), "删不了别人的");
        assert!(store.delete("alice", &a.id).unwrap());
        assert!(!store.delete("alice", &a.id).unwrap(), "删过的再删是 false");
        assert_eq!(store.list("alice").len(), 1);
        assert_eq!(store.list("bob").len(), 1, "bob 的没被连带删掉");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_bad_addresses_and_caps_names() {
        let path = temp_path("validate");
        let store = SavedQueryStore::open(&path).unwrap();
        for bad in [
            "",
            "logs",
            "//evil.example.com/x",
            "/logs?q=1",
            "/logs#x",
            "/lo gs",
            "/\\evil.example.com/x",
        ] {
            assert!(
                matches!(store.create("a", "n", bad, ""), Err(SaveError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(store.create("a", "n", "/logs", "q=1#frag"), Err(SaveError::Invalid(_))));
        assert!(matches!(
            store.create("a", "n", "/logs", &"q=".repeat(QUERY_MAX)),
            Err(SaveError::Invalid(_))
        ));
        let long = "名".repeat(NAME_MAX + 10);
        let q = store.create("a", &long, "/logs", "").unwrap();
        assert_eq!(q.name.chars().count(), NAME_MAX, "名字按字符数截断");
        // 默认名字超长也截断，末尾加省略号
        let q2 = store.create("a", "", "/logs", &format!("q={}", "x".repeat(200))).unwrap();
        assert_eq!(q2.name.chars().count(), NAME_MAX);
        assert!(q2.name.ends_with('…'));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn per_user_cap() {
        let path = temp_path("cap");
        let store = SavedQueryStore::open(&path).unwrap();
        for i in 0..PER_USER_MAX {
            store.create("a", "", "/logs", &format!("q={i}")).unwrap();
        }
        assert_eq!(store.create("a", "", "/logs", "q=more"), Err(SaveError::TooMany));
        store.create("b", "", "/logs", "q=more").unwrap();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn survives_reopen_and_sees_external_edits() {
        let path = temp_path("reopen");
        let id = {
            let store = SavedQueryStore::open(&path).unwrap();
            store.create("alice", "n", "/logs", "q=1").unwrap().id
        };
        let store = SavedQueryStore::open(&path).unwrap();
        assert_eq!(store.list("alice").len(), 1, "重启后还在");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, r#"{"version":1,"queries":[]}"#).unwrap();
        assert!(store.list("alice").is_empty(), "别的副本删的也生效");
        assert!(!store.delete("alice", &id).unwrap());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_garbage_files() {
        let path = temp_path("garbage");
        fs::write(&path, "not json").unwrap();
        assert!(SavedQueryStore::open(&path).is_err());
        fs::write(&path, r#"{"version":99,"queries":[]}"#).unwrap();
        assert!(SavedQueryStore::open(&path).is_err());
        fs::write(&path, "").unwrap();
        assert!(SavedQueryStore::open(&path).is_ok(), "空文件当没有收藏");
        fs::remove_file(&path).ok();
    }
}
