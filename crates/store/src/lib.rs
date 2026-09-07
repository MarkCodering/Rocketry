//! SQLite persistence. Blocking database work runs outside Tokio worker threads.
use anyhow::{Context, Result};
use async_trait::async_trait;
use rocketry_core::*;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
    pub artifacts: PathBuf,
    _lock: Arc<File>,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("instance.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("database is already open; attach to the running Rocketry server")?;
        let mut db = Connection::open(root.join("rocketry.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(
            version <= 2,
            "database was created by a newer Rocketry version"
        );
        if version == 0 {
            let tx = db.transaction()?;
            tx.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY, data TEXT NOT NULL);
CREATE TABLE messages(seq INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
CREATE INDEX messages_session ON messages(session,seq);
CREATE TABLE runs(id TEXT PRIMARY KEY, session TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT, run TEXT NOT NULL REFERENCES runs(id), data TEXT NOT NULL);
CREATE INDEX events_run ON events(run,seq);
CREATE TABLE approvals(id TEXT PRIMARY KEY, run TEXT NOT NULL REFERENCES runs(id), call_id TEXT NOT NULL, data TEXT NOT NULL, UNIQUE(run,call_id));
CREATE TABLE tool_calls(run TEXT NOT NULL REFERENCES runs(id), call_id TEXT NOT NULL, state TEXT NOT NULL, call TEXT NOT NULL, result TEXT, PRIMARY KEY(run,call_id));
CREATE TABLE memory(namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(namespace,key));
CREATE VIRTUAL TABLE memory_fts USING fts5(namespace UNINDEXED,key UNINDEXED,value);
PRAGMA user_version=1;")?;
            tx.commit()?;
        }
        if version < 2 {
            let tx = db.transaction()?;
            tx.execute_batch("CREATE TABLE workflow_steps(root TEXT NOT NULL REFERENCES runs(id),path TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(root,path)); PRAGMA user_version=2;")?;
            tx.commit()?;
        }
        let artifacts = root.join("artifacts");
        std::fs::create_dir_all(&artifacts)?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            artifacts,
            _lock: Arc::new(lock),
        })
    }
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            f(&mut *db
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?)
        })
        .await?
    }
    pub async fn approvals(&self, run: &str) -> Result<Vec<Approval>> {
        let run = run.to_owned();
        self.with(move |db| {
            let mut s = db.prepare("SELECT data FROM approvals WHERE run=?1")?;
            let rows = s.query_map([run], |r| r.get::<_, String>(0))?;
            rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
        })
        .await
    }
    pub async fn request_approval(&self, run: &str, call: &ToolCall) -> Result<Approval> {
        let a = Approval {
            id: id(),
            run_id: run.into(),
            call: call.clone(),
            decision: None,
        };
        self.with(move |db| {
            db.execute(
                "INSERT OR IGNORE INTO approvals VALUES(?1,?2,?3,?4)",
                params![a.id, a.run_id, a.call.id, serde_json::to_string(&a)?],
            )?;
            let raw: String = db.query_row(
                "SELECT data FROM approvals WHERE run=?1 AND call_id=?2",
                params![a.run_id, a.call.id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str(&raw)?)
        })
        .await
    }
    pub async fn decide(&self, id: &str, allow: bool) -> Result<Approval> {
        let id = id.to_owned();
        self.with(move |db| {
            let raw: String =
                db.query_row("SELECT data FROM approvals WHERE id=?1", [&id], |r| {
                    r.get(0)
                })?;
            let mut a: Approval = serde_json::from_str(&raw)?;
            anyhow::ensure!(a.decision.is_none(), "approval already resolved");
            a.decision = Some(allow);
            db.execute(
                "UPDATE approvals SET data=?1 WHERE id=?2",
                params![serde_json::to_string(&a)?, id],
            )?;
            Ok(a)
        })
        .await
    }
    pub async fn begin_tool(&self, run: &str, call: &ToolCall) -> Result<()> {
        let run = run.to_owned();
        let call = call.clone();
        self.with(move |db| {
            db.execute(
                "INSERT INTO tool_calls(run,call_id,state,call) VALUES(?1,?2,'started',?3)",
                params![run, call.id, serde_json::to_string(&call)?],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn finish_tool(
        &self,
        run: &str,
        session: &str,
        call: &ToolCall,
        result: &Value,
    ) -> Result<()> {
        let (run, session, call, result) = (
            run.to_owned(),
            session.to_owned(),
            call.clone(),
            result.clone(),
        );
        self.with(move |db| {
            let tx = db.transaction()?;
            let mut m = Message::text("tool", serde_json::to_string(&result)?);
            m.call_id = Some(call.id.clone());
            tx.execute(
                "INSERT INTO messages(session,data) VALUES(?1,?2)",
                params![session, serde_json::to_string(&m)?],
            )?;
            tx.execute(
                "UPDATE tool_calls SET state='finished',result=?1 WHERE run=?2 AND call_id=?3",
                params![result.to_string(), run, call.id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn uncertain(&self, run: &str) -> Result<Vec<ToolCall>> {
        let run = run.to_owned();
        self.with(move |db| {
            let mut s =
                db.prepare("SELECT call FROM tool_calls WHERE run=?1 AND state='started'")?;
            let rows = s.query_map([run], |r| r.get::<_, String>(0))?;
            rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
        })
        .await
    }
    pub async fn recover(&self) -> Result<()> {
        for mut r in self.runs().await? {
            if !r.status.terminal() {
                r.status = if self.uncertain(&r.id).await?.is_empty() {
                    RunStatus::Interrupted
                } else {
                    RunStatus::NeedsReconciliation
                };
                self.save_run(&r).await?;
                self.append_event(&r.id, EventKind::Status(r.status))
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn memory_put(&self, ns: &str, key: &str, value: &str) -> Result<()> {
        anyhow::ensure!(
            !key.trim().is_empty() && key.len() <= 256,
            "memory key must be 1-256 bytes"
        );
        anyhow::ensure!(value.len() <= 65536, "memory value exceeds 64 KiB");
        let (ns, key, value) = (ns.to_owned(), key.to_owned(), value.to_owned());
        self.with(move |db| {
            let tx = db.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO memory VALUES(?1,?2,?3)",
                params![ns, key, value],
            )?;
            tx.execute(
                "DELETE FROM memory_fts WHERE namespace=?1 AND key=?2",
                params![ns, key],
            )?;
            tx.execute(
                "INSERT INTO memory_fts VALUES(?1,?2,?3)",
                params![ns, key, value],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn memory_search(&self, ns: &str, query: &str) -> Result<Value> {
        let (ns, query) = (ns.to_owned(), query.to_owned());
        self.with(move|db|{let mut s=db.prepare("SELECT key,value FROM memory_fts WHERE namespace=?1 AND memory_fts MATCH ?2 ORDER BY rank LIMIT 20")?;let phrase=format!("\"{}\"",query.replace('"',"\"\""));let rows=s.query_map(params![ns,phrase],|r|Ok(serde_json::json!({"key":r.get::<_,String>(0)?,"value":r.get::<_,String>(1)?})))?;Ok(Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?))}).await
    }
    /// Bounded, most recently written entries; large values are clearly marked.
    pub async fn memory_list(&self, ns: &str) -> Result<Value> {
        let ns = ns.to_owned();
        self.with(move |db| {
            let mut s = db.prepare("SELECT key,substr(value,1,4096),length(value)>4096 FROM memory WHERE namespace=?1 ORDER BY rowid DESC LIMIT 64")?;
            let rows = s.query_map(params![ns], |r| Ok(serde_json::json!({
                "key":r.get::<_,String>(0)?, "value":r.get::<_,String>(1)?, "truncated":r.get::<_,bool>(2)?
            })))?;
            Ok(Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?))
        }).await
    }
    pub async fn memory_delete(&self, ns: &str, key: &str) -> Result<bool> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.with(move |db| {
            let tx = db.transaction()?;
            let deleted = tx.execute(
                "DELETE FROM memory WHERE namespace=?1 AND key=?2",
                params![ns, key],
            )? > 0;
            tx.execute(
                "DELETE FROM memory_fts WHERE namespace=?1 AND key=?2",
                params![ns, key],
            )?;
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }
    pub async fn artifact(&self, run: &str, bytes: Vec<u8>) -> Result<String> {
        let key = format!("{run}-{}", id());
        tokio::fs::write(self.artifacts.join(&key), bytes).await?;
        Ok(key)
    }
}
#[async_trait]
impl SessionStore for Store {
    async fn create_session(&self, title: &str) -> Result<Session> {
        let s = Session {
            id: id(),
            title: title.chars().take(100).collect(),
            created_at: now(),
        };
        self.with(move |db| {
            db.execute(
                "INSERT INTO sessions VALUES(?1,?2)",
                params![s.id, serde_json::to_string(&s)?],
            )?;
            Ok(s)
        })
        .await
    }
    async fn sessions(&self) -> Result<Vec<Session>> {
        self.with(|db| {
            let mut s = db.prepare("SELECT data FROM sessions ORDER BY rowid DESC LIMIT 1000")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect()
        })
        .await
    }
    async fn messages(&self, session: &str) -> Result<Vec<Message>> {
        let session = session.to_owned();
        self.with(move |db| {
            let mut s = db.prepare("SELECT data FROM messages WHERE session=?1 ORDER BY seq")?;
            s.query_map([session], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect()
        })
        .await
    }
    async fn add_message(&self, session: &str, message: &Message) -> Result<()> {
        let (session, data) = (session.to_owned(), serde_json::to_string(message)?);
        self.with(move |db| {
            db.execute(
                "INSERT INTO messages(session,data) VALUES(?1,?2)",
                params![session, data],
            )?;
            Ok(())
        })
        .await
    }
    async fn save_run(&self, run: &Run) -> Result<()> {
        let run = run.clone();
        self.with(move|db|{db.execute("INSERT INTO runs VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data",params![run.id,run.session_id,serde_json::to_string(&run)?])?;Ok(())}).await
    }
    async fn run(&self, id: &str) -> Result<Run> {
        let id = id.to_owned();
        self.with(move |db| {
            let s: String =
                db.query_row("SELECT data FROM runs WHERE id=?1", [id], |r| r.get(0))?;
            Ok(serde_json::from_str(&s)?)
        })
        .await
    }
    async fn runs(&self) -> Result<Vec<Run>> {
        self.with(|db| {
            let mut s = db.prepare("SELECT data FROM runs ORDER BY rowid DESC")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect()
        })
        .await
    }
    async fn append_event(&self, run: &str, kind: EventKind) -> Result<Event> {
        let e = Event {
            version: 1,
            sequence: 0,
            run_id: run.into(),
            timestamp: now(),
            kind,
        };
        self.with(move |db| {
            db.execute(
                "INSERT INTO events(run,data) VALUES(?1,?2)",
                params![e.run_id, serde_json::to_string(&e)?],
            )?;
            Ok(Event {
                sequence: db.last_insert_rowid(),
                ..e
            })
        })
        .await
    }
    async fn events(&self, run: &str, after: i64, limit: usize) -> Result<Vec<Event>> {
        let run = run.to_owned();
        self.with(move |db| {
            let mut s = db.prepare(
                "SELECT seq,data FROM events WHERE run=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
            )?;
            s.query_map(params![run, after, limit.min(1000)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .map(|r| {
                let (seq, raw) = r?;
                let mut e: Event = serde_json::from_str(&raw)?;
                e.sequence = seq;
                Ok(e)
            })
            .collect()
        })
        .await
    }
}

impl Store {
    pub async fn workflow_step(&self, root: &str, path: &str) -> Result<Option<Value>> {
        let (root, path) = (root.to_string(), path.to_string());
        self.with(move |db| {
            use rusqlite::OptionalExtension;
            let value: Option<String> = db
                .query_row(
                    "SELECT data FROM workflow_steps WHERE root=?1 AND path=?2",
                    params![root, path],
                    |r| r.get(0),
                )
                .optional()?;
            value
                .map(|v| serde_json::from_str(&v).map_err(Into::into))
                .transpose()
        })
        .await
    }
    pub async fn save_workflow_step(&self, root: &str, path: &str, value: Value) -> Result<()> {
        let (root, path) = (root.to_string(), path.to_string());
        self.with(move|db|{db.execute("INSERT INTO workflow_steps VALUES(?1,?2,?3) ON CONFLICT(root,path) DO UPDATE SET data=excluded.data",params![root,path,value.to_string()])?;Ok(())}).await
    }
}

impl Store {
    /// Commit a status transition and its ordered event in the same transaction.
    pub async fn transition(&self, run: &Run) -> Result<()> {
        let run = run.clone();
        self.with(move |db| {
            let tx = db.transaction()?;
            let event = Event {
                version: 1,
                sequence: 0,
                run_id: run.id.clone(),
                timestamp: now(),
                kind: EventKind::Status(run.status.clone()),
            };
            tx.execute(
                "UPDATE runs SET data=?1 WHERE id=?2",
                params![serde_json::to_string(&run)?, run.id],
            )?;
            tx.execute(
                "INSERT INTO events(run,data) VALUES(?1,?2)",
                params![run.id, serde_json::to_string(&event)?],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn durable_tool_and_recovery() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let s = store.create_session("test").await?;
        let r = Run {
            id: id(),
            session_id: s.id,
            parent_id: None,
            agent: Agent {
                name: "test".into(),
                instructions: String::new(),
                provider: "demo".into(),
                tools: vec![],
                output_schema: None,
            },
            status: RunStatus::Running,
            created_at: now(),
            error: None,
            workspace: dir.path().into(),
        };
        store.save_run(&r).await?;
        let call = ToolCall {
            id: id(),
            name: "write".into(),
            arguments: Value::Null,
        };
        store.begin_tool(&r.id, &call).await?;
        store.recover().await?;
        assert_eq!(
            store.run(&r.id).await?.status,
            RunStatus::NeedsReconciliation
        );
        store
            .finish_tool(&r.id, &r.session_id, &call, &Value::Bool(true))
            .await?;
        assert!(store.uncertain(&r.id).await?.is_empty());
        assert_eq!(store.messages(&r.session_id).await?.len(), 1);
        store.memory_put("a", "k", "launch control").await?;
        assert_eq!(
            store
                .memory_search("b", "launch")
                .await?
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store
                .memory_search("a", "launch")
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        Ok(())
    }
}
