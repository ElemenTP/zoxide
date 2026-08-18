//! In-process zoxide session.
//!
//! `Session` owns a `Database` for the lifetime of the shell process. Keeping
//! the database open avoids the parse + deserialize cost of `zoxide add` /
//! `zoxide query` on every shell hook and directory jump. zoxide is
//! single-threaded and contains no runtime/thread pool, so a session can be
//! copied by `fork()` and used safely from the child as well.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config;
use crate::db::{Database, Dir, Epoch, Rank, Stream, StreamOptions};
use crate::util::{self, Fzf, FzfChild};

/// Options for one `Session::query` call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionStats {
    pub adds: u64,
    pub queries: u64,
    pub removes: u64,
}

/// A persistent zoxide database session.
pub struct Session {
    db: Database,
    stats: SessionStats,
}

impl Session {
    /// Open the database in the standard zoxide data directory
    /// (`_ZO_DATA_DIR` or the platform-local data directory).
    pub fn new() -> Result<Self> {
        let data_dir = config::data_dir()?;
        Self::with_data_dir(data_dir)
    }

    /// Open the database in an explicit directory. Used by tests and by hosts
    /// that want to isolate the database without changing `_ZO_DATA_DIR`.
    fn with_data_dir(data_dir: impl AsRef<Path>) -> Result<Self> {
        let db = Database::open_dir(data_dir)?;
        Ok(Self { db, stats: SessionStats::default() })
    }

    /// Add a directory to the database.
    ///
    /// Mirrors `zoxide add` for a single path: resolves symlinks according to
    /// `_ZO_RESOLVE_SYMLINKS`, honors `_ZO_EXCLUDE_DIRS`, updates rank +
    /// last-accessed time, ages the database (`_ZO_MAXAGE`) and saves when
    /// dirty.
    pub fn add(&mut self, path: impl AsRef<Path>, score: Rank) -> Result<()> {
        // These characters can't be printed cleanly to a single line, so they
        // are excluded for the same reason as the upstream CLI.
        const EXCLUDE_CHARS: &[char] = &['\n', '\r'];

        let exclude_dirs = config::exclude_dirs()?;
        let max_age = config::maxage()?;
        let now = util::current_time()?;

        let path = if config::resolve_symlinks() {
            util::canonicalize(path.as_ref())
        } else {
            util::resolve_path(path.as_ref())
        }?;
        let path = util::path_to_str(&path)?;

        if path.contains(EXCLUDE_CHARS) || exclude_dirs.iter().any(|glob| glob.matches(path)) {
            return Ok(());
        }
        if !Path::new(path).is_dir() {
            bail!("not a directory: {path}");
        }

        self.db.add_update(path, score, now);

        if self.db.dirty() {
            self.db.age(max_age);
        }
        self.db.save()?;

        self.stats.adds += 1;
        Ok(())
    }

    /// Remove a directory from the database. Mirrors `zoxide remove`.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        if !self.db.remove(path) {
            let path_abs = util::resolve_path(path)?;
            let path_abs = util::path_to_str(&path_abs)?;
            if path_abs == path || !self.db.remove(path_abs) {
                bail!("path not found in database: {path}");
            }
        }

        self.db.save()?;
        self.stats.removes += 1;
        Ok(())
    }

    /// Run a query and return the result as a string.
    ///
    /// Return convention: `first` and `interactive` modes return the matched
    /// path (or `score\tpath`) without a trailing newline; `list` mode joins
    /// the results with `\n` and has no trailing newline.
    pub fn query(&mut self, options: QueryOptions) -> Result<String> {
        if options.interactive && options.list {
            bail!("--interactive and --list cannot be used together");
        }

        let now = util::current_time()?;

        let output = {
            let mut stream = self.get_stream(&options, now)?;
            if options.interactive {
                Self::query_interactive(&mut stream, &options, now)?
            } else if options.list {
                Self::query_list(&mut stream, &options, now)?
            } else {
                Self::query_first(&mut stream, &options, now)?
            }
        };

        // Query may lazily remove stale entries from the database; persist
        // that cleanup exactly like the upstream CLI does.
        self.db.save()?;

        self.stats.queries += 1;
        Ok(output)
    }

    /// Snapshot the session counters and current entry count.
    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    pub fn entry_count(&self) -> usize {
        self.db.dirs().len()
    }

    fn get_stream<'a>(
        &'a mut self,
        options: &QueryOptions,
        now: Epoch,
    ) -> Result<Stream<'a>> {
        let mut stream_options = StreamOptions::new(now)
            .with_keywords(options.keywords.iter().map(String::as_str))
            .with_exclude(config::exclude_dirs()?)
            .with_base_dir(options.base_dir.clone());
        if !options.all {
            stream_options = stream_options
                .with_exists(true)
                .with_resolve_symlinks(config::resolve_symlinks());
        }

        Ok(Stream::new(&mut self.db, stream_options))
    }

    fn query_interactive(
        stream: &mut Stream<'_>,
        options: &QueryOptions,
        now: Epoch,
    ) -> Result<String> {
        let mut fzf = Self::get_fzf()?;
        let selection = loop {
            match stream.next() {
                Some(dir) if Self::is_excluded(dir, options.exclude.as_deref()) => continue,
                Some(dir) => {
                    if let Some(selection) = fzf.write(dir, now)? {
                        break selection;
                    }
                }
                None => break fzf.wait()?,
            }
        };
        // fzf normally terminates its selection with a newline. The CLI uses
        // `$(zoxide query -i)` and command substitution removes it; the zsh
        // builtin puts the raw string into ZOXIDE_RESULT, so trim it here to
        // honor the FFI contract "no trailing newline".
        let selection = selection.trim_end_matches(['\n', '\r']).to_string();

        if options.score {
            Ok(selection)
        } else {
            let path = selection.get(7..).context("could not read selection from fzf")?;
            Ok(path.to_string())
        }
    }

    fn query_list(
        stream: &mut Stream<'_>,
        options: &QueryOptions,
        now: Epoch,
    ) -> Result<String> {
        let mut output = String::new();
        while let Some(dir) = stream.next() {
            if Self::is_excluded(dir, options.exclude.as_deref()) {
                continue;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&Self::render(dir, options.score, now));
        }
        Ok(output)
    }

    fn query_first(
        stream: &mut Stream<'_>,
        options: &QueryOptions,
        now: Epoch,
    ) -> Result<String> {
        let mut dir = stream.next().context("no match found")?;
        while Self::is_excluded(dir, options.exclude.as_deref()) {
            dir = stream.next().context("you are already in the only match")?;
        }

        Ok(Self::render(dir, options.score, now))
    }

    fn is_excluded(dir: &Dir<'_>, exclude: Option<&str>) -> bool {
        Some(dir.path.as_ref()) == exclude
    }

    fn render(dir: &Dir<'_>, with_score: bool, now: Epoch) -> String {
        let display = dir.display().with_score(now);
        if with_score {
            display.to_string()
        } else {
            dir.path.to_string()
        }
    }

    fn get_fzf() -> Result<FzfChild> {
        let mut fzf = Fzf::new()?;
        if let Some(fzf_opts) = config::fzf_opts() {
            fzf.env("FZF_DEFAULT_OPTS", fzf_opts)
        } else {
            fzf.args([
                // Search mode
                "--exact",
                // Search result
                "--no-sort",
                // Interface
                "--bind=ctrl-z:ignore,btab:up,tab:down",
                "--cycle",
                "--keep-right",
                // Layout
                "--border=sharp",
                "--height=45%",
                "--info=inline",
                "--layout=reverse",
                // Display
                "--tabstop=1",
                // Scripting
                "--exit-0",
            ])
            .enable_preview()
        }
        .spawn()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_keywords(keywords: &[&str]) -> QueryOptions {
        QueryOptions {
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            ..QueryOptions::default()
        }
    }

    #[test]
    fn add_then_query_returns_path() {
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();

        let mut session = Session::with_data_dir(data_dir.path()).unwrap();
        session.add(path, 1.0).unwrap();

        let result = session.query(query_keywords(&[target.path().file_name().unwrap().to_str().unwrap()])).unwrap();
        assert_eq!(result, util::resolve_path(path).unwrap().to_str().unwrap());
        assert_eq!(session.stats().adds, 1);
        assert_eq!(session.stats().queries, 1);
    }

    #[test]
    fn add_is_persistent_across_sessions() {
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();

        {
            let mut session = Session::with_data_dir(data_dir.path()).unwrap();
            session.add(path, 2.0).unwrap();
        }
        {
            let session = Session::with_data_dir(data_dir.path()).unwrap();
            assert_eq!(session.entry_count(), 1);
        }
    }

    #[test]
    fn query_list_joins_lines_without_trailing_newline() {
        let data_dir = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let mut session = Session::with_data_dir(data_dir.path()).unwrap();
        session.add(first.path(), 1.0).unwrap();
        session.add(second.path(), 2.0).unwrap();

        let options = QueryOptions { list: true, all: true, ..query_keywords(&[]) };
        let result = session.query(options).unwrap();
        assert!(!result.ends_with('\n'));
        assert_eq!(result.lines().count(), 2);
    }

    #[test]
    fn query_without_match_returns_error() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut session = Session::with_data_dir(data_dir.path()).unwrap();
        let err = session.query(query_keywords(&["definitely-no-such-entry"])).unwrap_err();
        assert!(err.to_string().contains("no match found"), "{err}");
    }

    #[test]
    fn remove_deletes_entry() {
        let data_dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().to_str().unwrap();

        let mut session = Session::with_data_dir(data_dir.path()).unwrap();
        session.add(path, 1.0).unwrap();
        assert_eq!(session.entry_count(), 1);

        session.remove(path).unwrap();
        assert_eq!(session.entry_count(), 0);
        assert_eq!(session.stats().removes, 1);
    }

    #[test]
    fn conflicting_query_modes_are_rejected() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut session = Session::with_data_dir(data_dir.path()).unwrap();
        let options = QueryOptions {
            interactive: true,
            list: true,
            all: true,
            ..query_keywords(&[])
        };
        assert!(session.query(options).is_err());
    }
}
