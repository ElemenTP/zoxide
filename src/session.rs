//! In-process zoxide session.
//!
//! `Session` deliberately does NOT keep a `Database` open. zoxide's on-disk
//! format has no cross-process locking, so a long-lived in-memory database
//! would silently overwrite updates made by other shells or `zoxide`
//! processes. Instead every operation constructs the same command struct the
//! CLI uses and runs it; the database is opened and closed per command,
//! matching the original binary's concurrency behavior.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::cmd::{Add, Cmd, Query, Remove, Run};
use crate::db::{Database, Rank};

/// Options for one `Session::query` call.
#[derive(Debug, Default)]
pub struct QueryOptions {
    pub keywords: Vec<String>,
    pub exclude: Option<String>,
    pub base_dir: Option<String>,
    pub all: bool,
    pub interactive: bool,
    pub list: bool,
    pub score: bool,
}

/// Per-session counters, exposed through the FFI statistics call.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionStats {
    pub adds: u64,
    pub queries: u64,
    pub removes: u64,
}

/// A stateless zoxide session. Each method delegates to the corresponding
/// upstream command implementation (`cmd::Add`, `cmd::Query`, `cmd::Remove`).
pub struct Session {
    stats: SessionStats,
}

impl Session {
    pub fn new() -> Result<Self> {
        Ok(Self { stats: SessionStats::default() })
    }

    /// Add a directory to the database. Mirrors `zoxide add`.
    pub fn add(&mut self, path: impl AsRef<Path>, score: Rank) -> Result<()> {
        self.stats.adds += 1;
        
        Cmd::Add(Add { paths: vec![path.as_ref().to_path_buf()], score: Some(score) }).run()?;
        Ok(())
    }

    /// Remove a directory from the database. Mirrors `zoxide remove`.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.stats.removes += 1;

        Cmd::Remove(Remove { paths: vec![path.to_owned()] }).run()?;
        Ok(())
    }

    /// Run a query and return the result as a string.
    ///
    /// The upstream query writes to stdout; the in-process session captures it
    /// into a buffer and trims the trailing newline, matching the shell
    /// command-substitution semantics that the shell modules expect.
    pub fn query(&mut self, options: QueryOptions) -> Result<String> {
        self.stats.queries += 1;

        if options.interactive && options.list {
            bail!("--interactive and --list cannot be used together");
        }

        let query = Query {
            keywords: options.keywords,
            all: options.all,
            interactive: options.interactive,
            list: options.list,
            score: options.score,
            exclude: options.exclude,
            base_dir: options.base_dir,
        };

        let mut output = Vec::new();
        query.run_with_writer(&mut output)?;
        let output = String::from_utf8(output).context("query output is not valid UTF-8")?;
        Ok(output.trim_end_matches(['\n', '\r']).to_string())
    }

    /// Snapshot the session counters.
    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    /// Current database entry count. Opens the database read-only for this
    /// call only; it does not keep a handle open.
    pub fn entry_count(&self) -> Result<usize> {
        Ok(Database::open()?.dirs().len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// `cmd::*` reads `_ZO_DATA_DIR` from the process environment. Tests set
    /// this global, so serialize them against each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_data_dir(data_dir: &Path) {
        // SAFETY: session tests hold TEST_LOCK, so no other test can observe
        // the environment while it is temporarily changed.
        unsafe { std::env::set_var("_ZO_DATA_DIR", data_dir.as_os_str()) };
    }

    fn query_keywords(keywords: &[&str]) -> QueryOptions {
        QueryOptions {
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            ..QueryOptions::default()
        }
    }

    #[test]
    fn add_then_query_returns_path() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();
        with_data_dir(data_dir.path());

        let mut session = Session::new().unwrap();
        session.add(path, 1.0).unwrap();

        let result = session
            .query(query_keywords(&[target.path().file_name().unwrap().to_str().unwrap()]))
            .unwrap();
        assert_eq!(result, crate::util::resolve_path(path).unwrap().to_str().unwrap());
        assert_eq!(session.stats().adds, 1);
        assert_eq!(session.stats().queries, 1);
    }

    #[test]
    fn add_is_persistent_across_sessions() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();
        with_data_dir(data_dir.path());

        {
            let mut session = Session::new().unwrap();
            session.add(path, 2.0).unwrap();
        }
        {
            let session = Session::new().unwrap();
            assert_eq!(session.entry_count().unwrap(), 1);
        }
    }

    #[test]
    fn query_list_joins_lines_without_trailing_newline() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        with_data_dir(data_dir.path());

        let mut session = Session::new().unwrap();
        session.add(first.path(), 1.0).unwrap();
        session.add(second.path(), 2.0).unwrap();

        let options = QueryOptions { list: true, all: true, ..query_keywords(&[]) };
        let result = session.query(options).unwrap();
        assert!(!result.ends_with('\n'));
        assert_eq!(result.lines().count(), 2);
    }

    #[test]
    fn query_without_match_returns_error() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        with_data_dir(data_dir.path());

        let mut session = Session::new().unwrap();
        let err = session.query(query_keywords(&["definitely-no-such-entry"])).unwrap_err();
        assert!(err.to_string().contains("no match found"), "{err}");
    }

    #[test]
    fn query_sees_updates_from_other_sessions() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        with_data_dir(data_dir.path());

        let mut session_a = Session::new().unwrap();
        session_a.add(first.path(), 1.0).unwrap();

        // A second session opens the database after session A closed it. Any
        // update made here must be visible to session A's next command.
        let mut session_b = Session::new().unwrap();
        session_b.add(second.path(), 1.0).unwrap();

        let keyword = second.path().file_name().unwrap().to_str().unwrap();
        let result = session_a.query(query_keywords(&[keyword])).unwrap();
        assert_eq!(result, crate::util::resolve_path(second.path()).unwrap().to_str().unwrap());
        assert_eq!(session_a.entry_count().unwrap(), 2);
    }

    #[test]
    fn remove_deletes_entry() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();
        with_data_dir(data_dir.path());

        let mut session = Session::new().unwrap();
        session.add(path, 1.0).unwrap();
        assert_eq!(session.entry_count().unwrap(), 1);

        session.remove(path).unwrap();
        assert_eq!(session.entry_count().unwrap(), 0);
        assert_eq!(session.stats().removes, 1);
    }

    #[test]
    fn conflicting_query_modes_are_rejected() {
        let _guard = test_lock();
        let data_dir = tempfile::tempdir().unwrap();
        with_data_dir(data_dir.path());

        let mut session = Session::new().unwrap();
        let options =
            QueryOptions { interactive: true, list: true, all: true, ..query_keywords(&[]) };
        assert!(session.query(options).is_err());
    }
}
