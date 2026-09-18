//! API key 的存储：一个 JSON 文件（`--api-key-file`），每把 key 存的是 **哈希**，不是 key 本身。
//!
//! key 长这样：`opdash_<12 位 hex id>.<32 位随机串>`。认证时按 id 找到那一行，把随机串的 SHA-256
//! 和存的比（常量时间），再看有没有过期。文件里没有明文，泄露了文件也签不出 key；用户能在页面上
//! 列出自己的 key、随时吊销（从文件里删掉那一行），这是之前签名 token 那版做不到的。
//!
//! 为什么是文件不是 ClickHouse：opdash 对库是只读的（每条查询 `readonly=2`，推荐给它只读账号），
//! 为了一张几行的 key 表加一条写库路径、再处理集群上的建表，不值。文件够小（几十把 key、几 KB），
//! 写法是「写临时文件再 rename」，掉电也不会留半个文件。单副本挂一个卷就行；多副本共享一个卷的话，
//! 每次用到都先看文件的 mtime 变了没有，变了就重读，副本之间几秒内一致。
//!
//! `last_used_at` 每次认证成功都更新，但只在内存里标脏，由后台任务每分钟落一次盘——不能让每个
//! MCP 请求都写一次磁盘。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::session::now_secs;

/// API key 的前缀：一眼认得出、secret 扫描器也好写规则。
pub const API_KEY_PREFIX: &str = "opdash_";
/// 有效期的下限。
pub const API_KEY_MIN_TTL_SECS: i64 = 60;
/// key 的名字最长多少字符（只是给人认的标签）。
pub const API_KEY_NAME_MAX: usize = 64;
/// 过期这么久之后从文件里清掉；之前留着是为了页面上还能看见「已过期」。
const PURGE_AFTER_SECS: i64 = 7 * 86_400;
/// 文件格式版本，改结构时用来迁移。
const FILE_VERSION: u32 = 1;

/// 一把 key 给人看的部分。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKey {
    /// 12 位 hex，key 的前半段，页面上和日志里认它
    pub id: String,
    /// 用户起的名字，如 `claude-code`
    pub name: String,
    /// 签发它的人（OIDC 用户名，或 Basic 的账号名）；只有本人能列出和吊销
    pub user: String,
    pub email: Option<String>,
    /// unix 秒
    pub created_at: i64,
    pub expires_at: i64,
    /// 最近一次拿它通过认证的时间，unix 秒；从没用过是 None
    pub last_used_at: Option<i64>,
}

impl ApiKey {
    pub fn expired(&self) -> bool {
        self.expires_at <= now_secs()
    }
}

/// 文件里的一行：给人看的部分 + 随机串的 SHA-256（hex）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredKey {
    #[serde(flatten)]
    key: ApiKey,
    hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    keys: Vec<StoredKey>,
}

struct State {
    keys: Vec<StoredKey>,
    /// 上次读 / 写文件时它的 mtime；别的副本改了文件就对不上，重读
    mtime: Option<SystemTime>,
    /// 内存里有还没落盘的 `last_used_at`
    dirty: bool,
}

pub struct KeyStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl KeyStore {
    /// 打开（没有就建一个空的）。建不出来、读不懂都在这里报，启动就失败，别等到有人生成 key 才发现。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)
                .map_err(|e| format!("建不出 {} 所在的目录: {e}", path.display()))?;
        }
        let store =
            Self { path, state: Mutex::new(State { keys: Vec::new(), mtime: None, dirty: false }) };
        if store.path.exists() {
            let mut st = store.state.lock();
            store.load_into(&mut st)?;
        } else {
            // 现在就写一个空文件：目录不可写这类问题启动时就暴露
            let mut st = store.state.lock();
            store.save(&mut st)?;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_into(&self, st: &mut State) -> Result<(), String> {
        let raw =
            fs::read(&self.path).map_err(|e| format!("读不了 {}: {e}", self.path.display()))?;
        let file: FileFormat = if raw.iter().all(u8::is_ascii_whitespace) {
            FileFormat::default()
        } else {
            serde_json::from_slice(&raw)
                .map_err(|e| format!("{} 不是 opdash 的 API key 文件: {e}", self.path.display()))?
        };
        if file.version > FILE_VERSION {
            return Err(format!(
                "{} 是更新版本的 opdash 写的（version {}），这个版本只认到 {FILE_VERSION}",
                self.path.display(),
                file.version
            ));
        }
        // 内存里没落盘的 last_used_at 不能被文件里旧的盖掉：同一把 key 取两边较晚的那个
        let mut keys = file.keys;
        for k in &mut keys {
            if let Some(mine) = st.keys.iter().find(|m| m.key.id == k.key.id) {
                k.key.last_used_at = k.key.last_used_at.max(mine.key.last_used_at);
            }
        }
        st.keys = keys;
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
            tracing::warn!(error = %e, "重读 API key 文件失败，继续用内存里的");
        }
    }

    /// 写临时文件再 rename，不会留下半个文件。顺手清掉过期很久的。
    fn save(&self, st: &mut State) -> Result<(), String> {
        let cutoff = now_secs() - PURGE_AFTER_SECS;
        st.keys.retain(|k| k.key.expires_at > cutoff);
        let file = FileFormat { version: FILE_VERSION, keys: st.keys.clone() };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json).map_err(|e| format!("写不了 {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("替换 {} 失败: {e}", self.path.display()))?;
        st.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        st.dirty = false;
        Ok(())
    }

    /// 签一把新 key。返回的第一项是完整的 key，只在这一刻给用户看一次。
    pub fn create(
        &self,
        user: &str,
        email: Option<&str>,
        name: &str,
        ttl_secs: i64,
    ) -> Result<(String, ApiKey), String> {
        let id = hex(&random_bytes(6));
        let secret = B64.encode(random_bytes(24));
        let now = now_secs();
        let name: String = name.trim().chars().take(API_KEY_NAME_MAX).collect();
        let key = ApiKey {
            id: id.clone(),
            name: if name.is_empty() { "api-key".to_owned() } else { name },
            user: user.to_owned(),
            email: email.map(str::to_owned),
            created_at: now,
            expires_at: now + ttl_secs.max(API_KEY_MIN_TTL_SECS),
            last_used_at: None,
        };
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        st.keys.push(StoredKey { key: key.clone(), hash: hash_secret(&secret) });
        self.save(&mut st)?;
        Ok((format!("{API_KEY_PREFIX}{id}.{secret}"), key))
    }

    /// `user` 自己的 key，新的在前。过期的也在（页面上标出来），过期太久的已经被 save 清掉。
    pub fn list(&self, user: &str) -> Vec<ApiKey> {
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        // 文件里是按创建顺序追加的，先倒过来再稳定排序：同一秒建的两把也是新的在前
        let mut out: Vec<ApiKey> =
            st.keys.iter().rev().filter(|k| k.key.user == user).map(|k| k.key.clone()).collect();
        out.sort_by_key(|k| std::cmp::Reverse(k.created_at));
        out
    }

    /// 吊销 `user` 自己的一把 key。不是他的 / 不存在 → `Ok(false)`，不区分——别让人探测别人的 id。
    pub fn revoke(&self, user: &str, id: &str) -> Result<bool, String> {
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let before = st.keys.len();
        st.keys.retain(|k| !(k.key.id == id && k.key.user == user));
        if st.keys.len() == before {
            return Ok(false);
        }
        self.save(&mut st)?;
        Ok(true)
    }

    /// 拿 Bearer 里的 token 认人。对上了顺手记 `last_used_at`（只标脏，后台落盘）。
    pub fn authenticate(&self, token: &str) -> Option<ApiKey> {
        let (id, secret) = parse_token(token)?;
        let mut st = self.state.lock();
        self.reload_if_changed(&mut st);
        let now = now_secs();
        let idx = st.keys.iter().position(|k| k.key.id == id)?;
        let entry = &mut st.keys[idx];
        if !super::basic::constant_time_eq(&entry.hash, &hash_secret(secret)) {
            return None;
        }
        if entry.key.expires_at <= now {
            return None;
        }
        // 同一分钟内不反复标脏，省得后台每分钟都重写一遍文件
        let touched = entry.key.last_used_at.is_none_or(|t| now - t >= 60);
        if touched {
            entry.key.last_used_at = Some(now);
        }
        let key = entry.key.clone();
        if touched {
            st.dirty = true;
        }
        Some(key)
    }

    /// 有没落盘的 `last_used_at` 就写一次。
    pub fn flush_if_dirty(&self) -> Result<(), String> {
        let mut st = self.state.lock();
        if !st.dirty {
            return Ok(());
        }
        // 先合并别的副本写的，再落盘，不然会把它们刚建的 key 冲掉
        self.reload_if_changed(&mut st);
        self.save(&mut st)
    }

    /// 后台每 `every` 落一次 `last_used_at`。
    pub fn spawn_flusher(self: Arc<Self>, every: Duration) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = self.flush_if_dirty() {
                    tracing::warn!(error = %e, "API key 的 last_used_at 落盘失败");
                }
            }
        });
    }
}

/// `opdash_<id>.<secret>` → `(id, secret)`。形状不对就 None，不用去查文件。
pub fn parse_token(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(API_KEY_PREFIX)?;
    let (id, secret) = rest.split_once('.')?;
    let ok = id.len() == 12
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && !secret.is_empty()
        && secret.len() <= 64
        && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some((id, secret))
}

fn hash_secret(secret: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, secret.as_bytes()).as_ref())
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
            "opdash-keys-{tag}-{}-{}.json",
            std::process::id(),
            now_secs()
        ))
    }

    #[test]
    fn create_authenticate_list_revoke_roundtrip() {
        let path = temp_path("roundtrip");
        let store = KeyStore::open(&path).unwrap();
        let (token, key) = store.create("alice", Some("a@x"), "  claude-code ", 3600).unwrap();
        assert!(token.starts_with("opdash_"), "{token}");
        assert_eq!(key.name, "claude-code");
        assert_eq!(key.expires_at - key.created_at, 3600);
        let (id, secret) = parse_token(&token).unwrap();
        assert_eq!(id, key.id);
        assert_eq!(id.len(), 12);
        assert_eq!(secret.len(), 32);

        // 文件里只有哈希，没有明文
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(secret), "{raw}");
        assert!(raw.contains(&key.id));

        let got = store.authenticate(&token).expect("key 要认得");
        assert_eq!(got.user, "alice");
        assert!(got.last_used_at.is_some());
        assert!(store.authenticate(&format!("{token}x")).is_none(), "改一位不认");
        assert!(store.authenticate(&token.replace("opdash_", "")).is_none(), "没前缀不认");
        assert!(store.authenticate("opdash_nope.nope").is_none());

        assert_eq!(store.list("alice").len(), 1);
        assert!(store.list("bob").is_empty(), "看不见别人的");
        assert!(!store.revoke("bob", &key.id).unwrap(), "吊销不了别人的");
        assert!(store.revoke("alice", &key.id).unwrap());
        assert!(store.authenticate(&token).is_none(), "吊销后立刻失效");
        assert!(store.list("alice").is_empty());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn survives_reopen_and_sees_external_edits() {
        let path = temp_path("reopen");
        let (token, id) = {
            let store = KeyStore::open(&path).unwrap();
            let (token, key) = store.create("alice", None, "k", 3600).unwrap();
            (token, key.id)
        };
        // 重启：同一个文件，key 还在
        let store = KeyStore::open(&path).unwrap();
        assert!(store.authenticate(&token).is_some());
        // 另一个副本（这里直接改文件）删掉了它：mtime 变了就重读
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, r#"{"version":1,"keys":[]}"#).unwrap();
        let _ = id;
        assert!(store.authenticate(&token).is_none(), "别的副本吊销的也生效");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn expired_keys_are_rejected_and_eventually_purged() {
        let path = temp_path("expire");
        let store = KeyStore::open(&path).unwrap();
        // 直接往状态里塞一把过期很久的
        {
            let mut st = store.state.lock();
            st.keys.push(StoredKey {
                key: ApiKey {
                    id: "aaaaaaaaaaaa".into(),
                    name: "old".into(),
                    user: "alice".into(),
                    email: None,
                    created_at: 0,
                    expires_at: now_secs() - PURGE_AFTER_SECS - 10,
                    last_used_at: None,
                },
                hash: hash_secret("s"),
            });
            st.keys.push(StoredKey {
                key: ApiKey {
                    id: "bbbbbbbbbbbb".into(),
                    name: "recent".into(),
                    user: "alice".into(),
                    email: None,
                    created_at: 0,
                    expires_at: now_secs() - 10,
                    last_used_at: None,
                },
                hash: hash_secret("s"),
            });
        }
        assert!(store.authenticate("opdash_bbbbbbbbbbbb.s").is_none(), "刚过期的不认");
        // 一次保存之后：刚过期的还在（页面上能看见），过期很久的清掉了
        store.create("alice", None, "new", 3600).unwrap();
        let names: Vec<String> = store.list("alice").into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["new", "recent"]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_garbage_files() {
        let path = temp_path("garbage");
        fs::write(&path, "not json").unwrap();
        assert!(KeyStore::open(&path).is_err());
        fs::write(&path, r#"{"version":99,"keys":[]}"#).unwrap();
        assert!(KeyStore::open(&path).is_err());
        fs::write(&path, "").unwrap();
        assert!(KeyStore::open(&path).is_ok(), "空文件当没有 key");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn token_shape() {
        assert!(parse_token("opdash_0123456789ab.abcDEF_-").is_some());
        assert!(parse_token("opdash_0123456789ab").is_none());
        assert!(parse_token("opdash_0123456789a.x").is_none(), "id 不是 12 位");
        assert!(parse_token("opdash_0123456789ab.").is_none());
        assert!(parse_token("opdash_0123456789ab.a b").is_none());
        assert!(parse_token("x_0123456789ab.abc").is_none());
    }
}
