// Copyright 2015-2020 AXIA Technologies (UK) Ltd.
// This file is part of AXIA.

// AXIA is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// AXIA is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with AXIA.  If not, see <http://www.gnu.org/licenses/>.

/// The database objects is split into `Db` and `DbInner`.
/// `Db` creates shared `DbInner` instance and manages background
/// worker threads that all use the inner object.
///
/// There are 3 worker threads:
/// log_worker: Processes commit queue and reindexing. For each commit
/// in the queue, log worker creates a write-ahead record using `Log`.
/// Additionally, if there are active reindexing, it creates log records
/// for batches of relocated index entries.
/// flush_worker: Flushes log records to disk by calling `fsync` on the
/// log files.
/// commit_worker: Reads flushed log records and applies operations to the
/// index and value tables.
/// Each background worker is signalled with a conditional variable once
/// there is some work to be done.

use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::convert::TryInto;
use std::collections::{HashMap, VecDeque};
use parking_lot::{RwLock, Mutex, Condvar};
use fs2::FileExt;
use crate::{
	table::Key,
	error::{Error, Result},
	column::{ColId, Column, IterState},
	log::{Log, LogAction},
	index::PlanOutcome,
	options::{Metadata, Options},
};

// These are in memory, so we use usize
const MAX_COMMIT_QUEUE_BYTES: usize = 16 * 1024 * 1024;
// These are disk-backed, so we use u64
const MAX_LOG_QUEUE_BYTES: i64 = 128 * 1024 * 1024;
const MIN_LOG_SIZE: u64 = 64 * 1024 * 1024;
const KEEP_LOGS: usize = 16;

/// Value is just a vector of bytes. Value sizes up to 4Gb are allowed.
pub type Value = Vec<u8>;


// Commit data passed to `commit`
#[derive(Default)]
struct Commit {
	// Commit ID. This is not the same as log record id, as some records
	// are originated within the DB. E.g. reindex.
	id: u64,
	// Size of user data pending insertion (keys + values) or
	// removal (keys)
	bytes: usize,
	// Operations.
	changeset: Vec<(ColId, Key, Option<Value>)>,
}

// Pending commits. This may not grow beyond `MAX_COMMIT_QUEUE_BYTES` bytes.
#[derive(Default)]
struct CommitQueue {
	// Log record.
	record_id: u64,
	// Total size of all commits in the queue.
	bytes: usize,
	// FIFO queue.
	commits: VecDeque<Commit>,
}

#[derive(Default)]
struct IdentityKeyHash(u64);
type IdentityBuildHasher = std::hash::BuildHasherDefault<IdentityKeyHash>;

impl std::hash::Hasher for IdentityKeyHash {
	fn write(&mut self, bytes: &[u8]) {
		self.0 = u64::from_le_bytes((&bytes[0..8]).try_into().unwrap())
	}
	fn write_u8(&mut self, _: u8)       { unreachable!() }
	fn write_u16(&mut self, _: u16)     { unreachable!() }
	fn write_u32(&mut self, _: u32)     { unreachable!() }
	fn write_u64(&mut self, _: u64)     { unreachable!() }
	fn write_usize(&mut self, _: usize) { }
	fn write_i8(&mut self, _: i8)       { unreachable!() }
	fn write_i16(&mut self, _: i16)     { unreachable!() }
	fn write_i32(&mut self, _: i32)     { unreachable!() }
	fn write_i64(&mut self, _: i64)     { unreachable!() }
	fn write_isize(&mut self, _: isize) { unreachable!() }
	fn finish(&self) -> u64 { self.0 }
}

struct DbInner {
	columns: Vec<Column>,
	options: Options,
	metadata: Metadata,
	shutdown: AtomicBool,
	log: Log,
	commit_queue: Mutex<CommitQueue>,
	commit_queue_full_cv: Condvar,
	log_worker_wait: WaitCondvar<bool>,
	commit_worker_wait: Arc<WaitCondvar<bool>>,
	// Overlay of most recent values int the commit queue. ColumnId -> (Key -> (RecordId, Value)).
	commit_overlay: RwLock<Vec<HashMap<Key, (u64, Option<Value>), IdentityBuildHasher>>>,
	log_queue_wait: WaitCondvar<i64>, // This may underflow occasionally, but is bound for 0 eventually
	flush_worker_wait: Arc<WaitCondvar<bool>>,
	cleanup_worker_wait: WaitCondvar<bool>,
	last_enacted: AtomicU64,
	next_reindex: AtomicU64,
	bg_err: Mutex<Option<Arc<Error>>>,
	_lock_file: std::fs::File,
}

pub struct WaitCondvar<S> {
	cv: Condvar,
	work: Mutex<S>,
}

impl<S: Default> WaitCondvar<S> {
	fn new() -> Self {
		WaitCondvar {
			cv: Condvar::new(),
			work: Mutex::new(S::default()),
		}
	}
}

impl WaitCondvar<bool> {
	fn signal(&self) {
		let mut work = self.work.lock();
		*work = true;
		self.cv.notify_all();
	}

	pub fn wait(&self) {
		let mut work = self.work.lock();
		while !*work {
			self.cv.wait(&mut work)
		};
		*work = false;
	}

	#[cfg(test)]
	fn wait_notify(&self) {
		let mut work = self.work.lock();
		self.cv.wait(&mut work)
	}
}

impl DbInner {
	fn open(options: &Options, inner_options: &InternalOptions) -> Result<DbInner> {
		if inner_options.create {
			std::fs::create_dir_all(&options.path)?
		};
		let mut lock_path: std::path::PathBuf = options.path.clone();
		lock_path.push("lock");
		let lock_file = std::fs::OpenOptions::new().create(true).read(true).write(true).open(lock_path.as_path())?;
		if !inner_options.skip_check_lock {
			lock_file.try_lock_exclusive().map_err(|e| Error::Locked(e))?;
		}

		let metadata = options.load_and_validate_metadata(inner_options.create)?;
		let mut columns = Vec::with_capacity(metadata.columns.len());
		let mut commit_overlay = Vec::with_capacity(metadata.columns.len());
		let log = Log::open(&options)?;
		let last_enacted = log.replay_record_id().unwrap_or(2) - 1;
		for c in 0 .. metadata.columns.len() {
			columns.push(Column::open(c as ColId, &options, &metadata)?);
			commit_overlay.push(
				HashMap::with_hasher(std::hash::BuildHasherDefault::<IdentityKeyHash>::default())
			);
		}
		log::debug!(target: "axia-db", "Opened db {:?}, metadata={:?}", options, metadata);
		Ok(DbInner {
			columns,
			options: options.clone(),
			metadata,
			shutdown: std::sync::atomic::AtomicBool::new(false),
			log,
			commit_queue: Mutex::new(Default::default()),
			commit_queue_full_cv: Condvar::new(),
			log_worker_wait: WaitCondvar::new(),
			commit_worker_wait: Arc::new(WaitCondvar::new()),
			commit_overlay: RwLock::new(commit_overlay),
			log_queue_wait: WaitCondvar::new(),
			flush_worker_wait: Arc::new(WaitCondvar::new()),
			cleanup_worker_wait: WaitCondvar::new(),
			next_reindex: AtomicU64::new(1),
			last_enacted: AtomicU64::new(last_enacted),
			bg_err: Mutex::new(None),
			_lock_file: lock_file,
		})
	}

	fn get(&self, col: ColId, key: &[u8]) -> Result<Option<Value>> {
		let key = self.columns[col as usize].hash(key);
		let overlay = self.commit_overlay.read();
		// Check commit overlay first
		if let Some(v) = overlay.get(col as usize).and_then(|o| o.get(&key).map(|(_, v)| v.clone())) {
			return Ok(v);
		}
		// Go into tables and log overlay.
		let log = self.log.overlays();
		self.columns[col as usize].get(&key, log)
	}

	fn get_size(&self, col: ColId, key: &[u8]) -> Result<Option<u32>> {
		let key = self.columns[col as usize].hash(key);
		let overlay = self.commit_overlay.read();
		// Check commit overlay first
		if let Some(l) = overlay.get(col as usize).and_then(
			|o| o.get(&key).map(|(_, v)| v.as_ref().map(|v| v.len() as u32))
		) {
			return Ok(l);
		}
		// Go into tables and log overlay.
		let log = self.log.overlays();
		self.columns[col as usize].get_size(&key, log)
	}

	// Commit simply adds the the data to the queue and to the overlay and
	// exits as early as possible.
	fn commit<I, K>(&self, tx: I) -> Result<()>
	where
		I: IntoIterator<Item=(ColId, K, Option<Value>)>,
		K: AsRef<[u8]>,
	{
		let commit: Vec<_> = tx.into_iter().map(
			|(c, k, v)| (c, self.columns[c as usize].hash(k.as_ref()), v)
		).collect();

		self.commit_raw(commit)
	}

	fn commit_raw(&self, commit: Vec<(ColId, Key, Option<Value>)>) -> Result<()> {
		{
			let mut queue = self.commit_queue.lock();
			if queue.bytes > MAX_COMMIT_QUEUE_BYTES {
				log::debug!(target: "axia-db", "Waiting, qb={}", queue.bytes);
				self.commit_queue_full_cv.wait(&mut queue);
			}
			{
				let bg_err = self.bg_err.lock();
				if let Some(err) = &*bg_err {
					return Err(Error::Background(err.clone()));
				}
			}

			let mut overlay = self.commit_overlay.write();

			queue.record_id += 1;
			let record_id = queue.record_id + 1;

			let mut bytes = 0;
			for (c, k, v) in &commit {
				bytes += k.len();
				bytes += v.as_ref().map_or(0, |v|v.len());
				// Don't add removed ref-counted values to overlay.
				if !self.metadata.columns[*c as usize].ref_counted || v.is_some() {
					overlay[*c as usize].insert(*k, (record_id, v.clone()));
				}
			}

			let commit = Commit {
				id: record_id,
				changeset: commit,
				bytes,
			};

			log::debug!(
				target: "axia-db",
				"Queued commit {}, {} bytes",
				commit.id,
				bytes,
			);
			queue.commits.push_back(commit);
			queue.bytes += bytes;
			self.log_worker_wait.signal();
		}
		Ok(())
	}

	fn process_commits(&self) -> Result<bool> {
		{
			// Wait if the queue is too big.
			let mut queue = self.log_queue_wait.work.lock();
			if !self.shutdown.load(Ordering::Relaxed) && *queue > MAX_LOG_QUEUE_BYTES {
				log::debug!(target: "axia-db", "Waiting, log_bytes={}", queue);
				self.log_queue_wait.cv.wait(&mut queue);
			}
		}
		let commit = {
			let mut queue = self.commit_queue.lock();
			if let Some(commit) = queue.commits.pop_front() {
				queue.bytes -= commit.bytes;
				log::debug!(
					target: "axia-db",
					"Removed {}. Still queued commits {} bytes",
					commit.bytes,
					queue.bytes,
				);
				if queue.bytes <= MAX_COMMIT_QUEUE_BYTES && (queue.bytes + commit.bytes) > MAX_COMMIT_QUEUE_BYTES {
					// Past the waiting threshold.
					log::debug!(
						target: "axia-db",
						"Waking up commit queue worker",
					);
					self.commit_queue_full_cv.notify_one();
				}
				Some(commit)
			} else {
				None
			}
		};

		if let Some(commit) = commit {
			let mut reindex = false;
			let mut writer = self.log.begin_record();
			log::debug!(
				target: "axia-db",
				"Processing commit {}, record {}, {} bytes",
				commit.id,
				writer.record_id(),
				commit.bytes,
			);
			let mut ops: u64 = 0;
			for (c, key, value) in commit.changeset.iter() {
				match self.columns[*c as usize].write_plan(key, value, &mut writer)? {
					// Reindex has triggered another reindex.
					PlanOutcome::NeedReindex => {
						reindex = true;
					},
					_ => {},
				}
				ops += 1;
			}
			// Collect final changes to value tables
			for c in self.columns.iter() {
				c.complete_plan(&mut writer)?;
			}
			let record_id = writer.record_id();
			let l = writer.drain();

			let bytes = {
				let bytes = self.log.end_record(l)?;
				let mut logged_bytes = self.log_queue_wait.work.lock();
				*logged_bytes += bytes as i64;
				self.flush_worker_wait.signal();
				bytes
			};

			{
				// Cleanup the commit overlay.
				let mut overlay = self.commit_overlay.write();
				for (c, key, _) in commit.changeset.iter() {
					let overlay = &mut overlay[*c as usize];
					if let std::collections::hash_map::Entry::Occupied(e) = overlay.entry(*key) {
						if e.get().0 == commit.id {
							e.remove_entry();
						}
					}
				}
			}

			if reindex {
				self.start_reindex(record_id);
			}

			log::debug!(
				target: "axia-db",
				"Processed commit {} (record {}), {} ops, {} bytes written",
				commit.id,
				record_id,
				ops,
				bytes,
			);
			Ok(true)
		} else {
			Ok(false)
		}
	}

	fn start_reindex(&self, record_id: u64) {
		self.next_reindex.store(record_id, Ordering::SeqCst);
	}

	fn process_reindex(&self) -> Result<bool> {
		let next_reindex = self.next_reindex.load(Ordering::SeqCst);
		if next_reindex == 0 || next_reindex > self.last_enacted.load(Ordering::SeqCst) {
			return Ok(false)
		}
		// Process any pending reindexes
		for column in self.columns.iter() {
			let (drop_index, batch) = column.reindex(&self.log)?;
			if !batch.is_empty() || drop_index.is_some() {
				let mut next_reindex = false;
				let mut writer = self.log.begin_record();
				log::debug!(
					target: "axia-db",
					"Creating reindex record {}",
					writer.record_id(),
				);
				for (key, address) in batch.into_iter() {
					match column.write_reindex_plan(&key, address, &mut writer)? {
						PlanOutcome::NeedReindex => {
							next_reindex = true
						},
						_ => {},
					}
				}
				if let Some(table) = drop_index {
					writer.drop_table(table);
				}
				let record_id = writer.record_id();
				let l = writer.drain();

				let mut logged_bytes = self.log_queue_wait.work.lock();
				let bytes = self.log.end_record(l)?;
				log::debug!(
					target: "axia-db",
					"Created reindex record {}, {} bytes",
					record_id,
					bytes,
				);
				*logged_bytes += bytes as i64;
				if next_reindex {
					self.start_reindex(record_id);
				}
				self.flush_worker_wait.signal();
				return Ok(true)
			}
		}
		self.next_reindex.store(0, Ordering::SeqCst);
		Ok(false)
	}

	fn enact_logs(&self, validation_mode: bool) -> Result<bool> {
		let cleared = {
			let reader = match self.log.read_next(validation_mode) {
				Ok(reader) => reader,
				Err(Error::Corruption(_)) if validation_mode => {
					log::debug!(target: "axia-db", "Bad log header");
					self.log.clear_replay_logs()?;
					return Ok(false);
				}
				Err(e) => return Err(e),
			};
			if let Some(mut reader) = reader {
				log::debug!(
					target: "axia-db",
					"Enacting log {}",
					reader.record_id(),
				);
				if validation_mode {
					if reader.record_id() != self.last_enacted.load(Ordering::Relaxed) + 1 {
						log::warn!(
							target: "axia-db",
							"Log sequence error. Expected record {}, got {}",
							self.last_enacted.load(Ordering::Relaxed) + 1,
							reader.record_id(),
						);
						std::mem::drop(reader);
						self.log.clear_replay_logs()?;
						return Ok(false);
					}
					// Validate all records before applying anything
					loop {
						let next = match reader.next() {
							Ok(next) => next,
							Err(e) => {
								log::debug!(target: "axia-db", "Error reading log: {:?}", e);
								std::mem::drop(reader);
								self.log.clear_replay_logs()?;
								return Ok(false);
							}
						};
						match next {
							LogAction::BeginRecord => {
								log::debug!(target: "axia-db", "Unexpected log header");
								std::mem::drop(reader);
								self.log.clear_replay_logs()?;
								return Ok(false);
							},
							LogAction::EndRecord => {
								break;
							},
							LogAction::InsertIndex(insertion) => {
								let col = insertion.table.col() as usize;
								if let Err(e) = self.columns[col].validate_plan(LogAction::InsertIndex(insertion), &mut reader) {
									log::warn!(target: "axia-db", "Error replaying log: {:?}. Reverting", e);
									std::mem::drop(reader);
									self.log.clear_replay_logs()?;
									return Ok(false);
								}
							},
							LogAction::InsertValue(insertion) => {
								let col = insertion.table.col() as usize;
								if let Err(e) = self.columns[col].validate_plan(LogAction::InsertValue(insertion), &mut reader) {
									log::warn!(target: "axia-db", "Error replaying log: {:?}. Reverting", e);
									std::mem::drop(reader);
									self.log.clear_replay_logs()?;
									return Ok(false);
								}
							},
							LogAction::DropTable(_) => {
								continue;
							}
						}
					}
					reader.reset()?;
					reader.next()?;
				}
				loop {
					match reader.next()? {
						LogAction::BeginRecord => {
							return Err(Error::Corruption("Bad log record".into()));
						},
						LogAction::EndRecord => {
							break;
						},
						LogAction::InsertIndex(insertion) => {
							self.columns[insertion.table.col() as usize]
								.enact_plan(LogAction::InsertIndex(insertion), &mut reader)?;

						},
						LogAction::InsertValue(insertion) => {
							self.columns[insertion.table.col() as usize]
								.enact_plan(LogAction::InsertValue(insertion), &mut reader)?;

						},
						LogAction::DropTable(id) => {
							log::debug!(
								target: "axia-db",
								"Dropping index {}",
								id,
							);
							self.columns[id.col() as usize].drop_index(id)?;
							// Check if there's another reindex on the next iteration
							self.start_reindex(reader.record_id());
						}
					}
				}
				log::debug!(
					target: "axia-db",
					"Enacted log record {}, {} bytes",
					reader.record_id(),
					reader.read_bytes(),
				);
				let record_id = reader.record_id();
				let bytes = reader.read_bytes();
				let cleared = reader.drain();
				self.last_enacted.store(record_id, Ordering::SeqCst);
				Some((record_id, cleared, bytes))
			} else {
				log::debug!(target: "axia-db", "End of log");
				None
			}
		};

		if let Some((record_id, cleared, bytes)) = cleared {
			self.log.end_read(cleared, record_id);
			{
				if !validation_mode {
					let mut queue = self.log_queue_wait.work.lock();
					if *queue < bytes as i64 {
						log::warn!(
							target: "axia-db",
							"Detected log undeflow record {}, {} bytes, {} queued, reindex = {}",
							record_id,
							bytes,
							*queue,
							self.next_reindex.load(Ordering::SeqCst),
						);
					}
					*queue -= bytes as i64;
					if *queue <= MAX_LOG_QUEUE_BYTES && (*queue + bytes as i64) > MAX_LOG_QUEUE_BYTES {
						self.log_queue_wait.cv.notify_all();
					}
					log::debug!(target: "axia-db", "Log queue size: {} bytes", *queue);
				}
			}
			Ok(true)
		} else {
			Ok(false)
		}
	}

	fn flush_logs(&self, min_log_size: u64) -> Result<bool> {
		let (flush_next, read_next, cleanup_next) = self.log.flush_one(min_log_size)?;
		if read_next {
			self.commit_worker_wait.signal();
		}
		if cleanup_next {
			self.cleanup_worker_wait.signal();
		}
		Ok(flush_next)
	}

	fn cleanup_logs(&self) -> Result<bool> {
		let keep_logs = if self.options.sync_data { 0 } else { KEEP_LOGS };
		let num_cleanup = self.log.num_dirty_logs();
		if num_cleanup > keep_logs {
			if self.options.sync_data {
				for c in self.columns.iter() {
					c.flush()?;
				}
			}
			self.log.clean_logs(num_cleanup - keep_logs)
		} else {
			Ok(false)
		}
	}

	fn clean_all_logs(&self) -> Result<()> {
		for c in self.columns.iter() {
			c.flush()?;
		}
		let num_cleanup = self.log.num_dirty_logs();
		self.log.clean_logs(num_cleanup)?;
		Ok(())
	}

	fn replay_all_logs(&mut self) -> Result<()> {
		while let Some(id) = self.log.replay_next()? {
			log::debug!(target: "axia-db", "Replaying database log {}", id);
			while self.enact_logs(true)? { }
		}
		// Re-read any cached metadata
		for c in self.columns.iter() {
			c.refresh_metadata()?;
		}
		log::debug!(target: "axia-db", "Replay is complete.");
		Ok(())
	}

	fn shutdown(&self) {
		self.shutdown.store(true, Ordering::SeqCst);
		self.log_queue_wait.cv.notify_all();
		self.flush_worker_wait.signal();
		self.log_worker_wait.signal();
		self.commit_worker_wait.signal();
		self.cleanup_worker_wait.signal();
	}

	fn kill_logs(&self) -> Result<()> {
		log::debug!(target: "axia-db", "Processing leftover commits");
		// Finish logged records and proceed to log and enact queued commits.
		while self.enact_logs(false)? {};
		self.flush_logs(0)?;
		while self.process_commits()? {};
		while self.enact_logs(false)? {};
		self.flush_logs(0)?;
		while self.enact_logs(false)? {};
		self.clean_all_logs()?;
		self.log.kill_logs()?;
		if self.options.stats {
			let mut path = self.options.path.clone();
			path.push("stats.txt");
			match std::fs::File::create(path) {
				Ok(file) => {
					let mut writer = std::io::BufWriter::new(file);
					self.collect_stats(&mut writer, None)
				}
				Err(e) => log::warn!(target: "axia-db", "Error creating stats file: {:?}", e),
			}
		}
		Ok(())
	}

	fn collect_stats(&self, writer: &mut impl std::io::Write, column: Option<u8>) {
		if let Some(col) = column {
			self.columns[col as usize].write_stats(writer);
		} else {
			for c in self.columns.iter() {
				c.write_stats(writer);
			}
		}
	}

	fn clear_stats(&self, column: Option<u8>) {
		if let Some(col) = column {
			self.columns[col as usize].clear_stats();
		} else {
			for c in self.columns.iter() {
				c.clear_stats();
			}
		}
	}

	fn store_err(&self, result: Result<()>) {
		if let Err(e) = result {
			log::warn!(target: "axia-db", "Background worker error: {}", e);
			let mut err =  self.bg_err.lock();
			if err.is_none() {
				*err = Some(Arc::new(e));
				self.shutdown();
			}
			self.commit_queue_full_cv.notify_one();
		}
	}

	fn iter_column_while(&self, c: ColId, f: impl FnMut(IterState) -> bool) -> Result<()> {
		self.columns[c as usize].iter_while(&self.log, f)
	}
}

pub struct Db {
	inner: Arc<DbInner>,
	commit_thread: Option<std::thread::JoinHandle<()>>,
	flush_thread: Option<std::thread::JoinHandle<()>>,
	log_thread: Option<std::thread::JoinHandle<()>>,
	cleanup_thread: Option<std::thread::JoinHandle<()>>,
	do_drop: bool,
}

impl Db {
	pub fn with_columns(path: &std::path::Path, num_columns: u8) -> Result<Db> {
		let options = Options::with_columns(path, num_columns);
		let mut inner_options = InternalOptions::default();
		inner_options.create = true;
		Self::open_inner(&options, &inner_options)
			.map(|r| r.0)
	}

	/// Open the database with given options.
	pub fn open(options: &Options) -> Result<Db> {
		let inner_options = InternalOptions::default();
		Self::open_inner(options, &inner_options)
			.map(|r| r.0)
	}

	/// Create the database using given options.
	pub fn open_or_create(options: &Options) -> Result<Db> {
		let mut inner_options = InternalOptions::default();
		inner_options.create = true;
		Self::open_inner(options, &inner_options)
			.map(|r| r.0)
	}

	pub fn open_read_only(options: &Options) -> Result<Db> {
		let mut inner_options = InternalOptions::default();
		inner_options.read_only = true;
		Self::open_inner(options, &inner_options)
			.map(|r| r.0)
	}

	fn open_inner(
		options: &Options,
		inner_options: &InternalOptions,
	) -> Result<(Db, Option<Arc<WaitCondvar<bool>>>)> {
		assert!(options.is_valid());
		let mut db = DbInner::open(options, &inner_options)?;
		// This needs to be call before log thread: so first reindexing
		// will run in correct state.
		db.replay_all_logs()?;
		let db = Arc::new(db);
		if inner_options.read_only {
			return Ok((Db {
				inner: db,
				commit_thread: None,
				flush_thread: None,
				log_thread: None,
				cleanup_thread: None,
				do_drop: inner_options.commit_stages.do_drop(),
			}, None))
		}
		let run_test_cv = match inner_options.commit_stages {
			EnableCommitPipelineStages::LogOverlay => Some(db.flush_worker_wait.clone()),
			EnableCommitPipelineStages::DbFile => Some(db.commit_worker_wait.clone()),
			_ => None,
		};
		let commit_thread = if inner_options.commit_stages.spawn_commit_thread() {
			let commit_worker_db = db.clone();
			Some(std::thread::spawn(move ||
				commit_worker_db.store_err(Self::commit_worker(commit_worker_db.clone()))
			))
		} else {
			None
		};
		let flush_thread = if inner_options.commit_stages.spawn_flush_thread() {
			let flush_worker_db = db.clone();
			let min_log_size = if matches!(inner_options.commit_stages, EnableCommitPipelineStages::DbFile) {
				0
			} else {
				MIN_LOG_SIZE
			};
			Some(std::thread::spawn(move ||
				flush_worker_db.store_err(Self::flush_worker(flush_worker_db.clone(), min_log_size))
			))
		} else {
			None
		};
		let log_thread = if inner_options.commit_stages.spawn_log_thread() {
			let log_worker_db = db.clone();
			Some(std::thread::spawn(move ||
				log_worker_db.store_err(Self::log_worker(log_worker_db.clone()))
			))
		} else {
			None
		};
		let cleanup_thread = if inner_options.commit_stages.spawn_cleanup_thread() {
			let cleanup_worker_db = db.clone();
			Some(std::thread::spawn(move ||
				cleanup_worker_db.store_err(Self::cleanup_worker(cleanup_worker_db.clone()))
			))
		} else {
			None
		};
		Ok((Db {
			inner: db,
			commit_thread,
			flush_thread: flush_thread,
			log_thread: log_thread,
			cleanup_thread: cleanup_thread,
			do_drop: inner_options.commit_stages.do_drop(),
		}, run_test_cv))
	}

	pub fn get(&self, col: ColId, key: &[u8]) -> Result<Option<Value>> {
		self.inner.get(col, key)
	}

	pub fn get_size(&self, col: ColId, key: &[u8]) -> Result<Option<u32>> {
		self.inner.get_size(col, key)
	}

	pub fn commit<I, K>(&self, tx: I) -> Result<()>
	where
		I: IntoIterator<Item=(ColId, K, Option<Value>)>,
		K: AsRef<[u8]>,
	{
		self.inner.commit(tx)
	}

	pub(crate) fn commit_raw(&self, commit: Vec<(ColId, Key, Option<Value>)>) -> Result<()> {
		self.inner.commit_raw(commit)
	}

	pub fn num_columns(&self) -> u8 {
		self.inner.columns.len() as u8
	}

	pub(crate) fn iter_column_while(&self, c: ColId, f: impl FnMut(IterState) -> bool) -> Result<()> {
		self.inner.iter_column_while(c, f)
	}

	fn commit_worker(db: Arc<DbInner>) -> Result<()> {
		let mut more_work = false;
		while !db.shutdown.load(Ordering::SeqCst) || more_work {
			if !more_work {
				db.commit_worker_wait.wait();
			}

			more_work = db.enact_logs(false)?;
		}
		log::debug!(target: "axia-db", "Commit worker shutdown");
		Ok(())
	}

	fn log_worker(db: Arc<DbInner>) -> Result<()> {
		// Start with pending reindex.
		let mut more_work = db.process_reindex()?;
		while !db.shutdown.load(Ordering::SeqCst) || more_work {
			if !more_work {
				db.log_worker_wait.wait();
			}

			let more_commits = db.process_commits()?;
			let more_reindex = db.process_reindex()?;
			more_work = more_commits || more_reindex;
		}
		log::debug!(target: "axia-db", "Log worker shutdown");
		Ok(())
	}

	fn flush_worker(db: Arc<DbInner>, min_log_size: u64) -> Result<()> {
		let mut more_work = false;
		while !db.shutdown.load(Ordering::SeqCst) {
			if !more_work {
				db.flush_worker_wait.wait();
			}
			more_work = db.flush_logs(min_log_size)?;
		}
		log::debug!(target: "axia-db", "Flush worker shutdown");
		Ok(())
	}

	fn cleanup_worker(db: Arc<DbInner>) -> Result<()> {
		let mut more_work = true;
		while !db.shutdown.load(Ordering::SeqCst) || more_work {
			if !more_work {
				db.cleanup_worker_wait.wait();
			}
			more_work = db.cleanup_logs()?;
		}
		log::debug!(target: "axia-db", "Cleanup worker shutdown");
		Ok(())
	}

	pub fn collect_stats(&self, writer: &mut impl std::io::Write, column: Option<u8>) {
		self.inner.collect_stats(writer, column)
	}

	pub fn clear_stats(&self, column: Option<u8>) {
		self.inner.clear_stats(column)
	}

	pub fn check_from_index(&self, check_param: check::CheckOptions) -> Result<()> {
		if let Some(col) = check_param.column.clone() {
			self.inner.columns[col as usize].check_from_index(&self.inner.log, &check_param, col)?;
		} else {
			for (ix, c) in self.inner.columns.iter().enumerate() {
				c.check_from_index(&self.inner.log, &check_param, ix as ColId)?;
			}
		}
		Ok(())
	}
}

impl Drop for Db {
	fn drop(&mut self) {
		if self.do_drop {
			self.inner.shutdown();
			self.log_thread.take().map(|t| t.join());
			self.flush_thread.take().map(|t| t.join());
			self.commit_thread.take().map(|t| t.join());
			self.cleanup_thread.take().map(|t| t.join());
			if let Err(e) = self.inner.kill_logs() {
				log::warn!(target: "axia-db", "Shutdown error: {:?}", e);
			}
		}
	}
}

/// Verification operation utilities.
pub mod check {
	pub enum CheckDisplay {
		None,
		Full,
		Short(u64),
	}

	pub struct CheckOptions {
		pub column: Option<u8>,
		pub from: Option<u64>,
		pub bound: Option<u64>,
		pub display: CheckDisplay,
	}

	impl CheckOptions {
		pub fn new(
			column: Option<u8>,
			from: Option<u64>,
			bound: Option<u64>,
			display_content: bool,
			truncate_value_display: Option<u64>,
		) -> Self {
			let display = if display_content {
				match truncate_value_display {
					Some(t) => CheckDisplay::Short(t),
					None => CheckDisplay::Full,
				}
			} else {
				CheckDisplay::None
			};
			CheckOptions {
				column,
				from,
				bound,
				display,
			}
		}
	}
}

#[derive(Default)]
struct InternalOptions {
	create: bool,
	read_only: bool,
	commit_stages: EnableCommitPipelineStages,
	skip_check_lock: bool,
}

#[derive(Debug, Clone, Copy)]
enum EnableCommitPipelineStages {
	// No threads started, data stays in commit overlay.
	#[allow(dead_code)]
	CommitOverlay,
	// Log worker run, data processed up to the log overlay.
	#[allow(dead_code)]
	LogOverlay,
	// Runing all.
	#[allow(dead_code)]
	DbFile,
	// Default run mode.
	Standard,
}

impl Default for EnableCommitPipelineStages {
	fn default() -> Self {
		EnableCommitPipelineStages::Standard
	}
}

impl EnableCommitPipelineStages {
	fn spawn_commit_thread(&self) -> bool {
		match self {
			EnableCommitPipelineStages::CommitOverlay
			| EnableCommitPipelineStages::LogOverlay => false,
			EnableCommitPipelineStages::DbFile
			| EnableCommitPipelineStages::Standard => true,
		}
	}

	fn spawn_log_thread(&self) -> bool {
		match self {
			EnableCommitPipelineStages::CommitOverlay => false,
			EnableCommitPipelineStages::LogOverlay
			| EnableCommitPipelineStages::DbFile
			| EnableCommitPipelineStages::Standard => true,
		}
	}

	fn spawn_flush_thread(&self) -> bool {
		self.spawn_log_thread()
	}

	fn spawn_cleanup_thread(&self) -> bool {
		self.spawn_commit_thread()
	}

	#[cfg(test)]
	fn check_empty_overlay(&self, db: &Db, col: ColId) -> bool {
		match self {
			EnableCommitPipelineStages::LogOverlay => {
			 if let Some(overlay) = db.inner.commit_overlay.read().get(col as usize) {
				if !overlay.is_empty() {
					let mut replayed = 5;
					while !overlay.is_empty() {
						if replayed > 0 {
							replayed -= 1;
							// the signal is triggered just before cleaning the overlay, so
							// we wait a bit.
							std::thread::sleep(std::time::Duration::from_millis(10));
						} else {
							return false;
						}
					}
				}
			 }
			},
			EnableCommitPipelineStages::DbFile => {
				 if let Some(overlay) = db.inner.commit_overlay.read().get(col as usize) {
					if !overlay.is_empty() { return false; }
				 }
			 }
			_ => (),
		}
		true
	}

	fn do_drop(&self) -> bool {
		matches!(self, EnableCommitPipelineStages::Standard)
	}
}

#[cfg(test)]
mod tests {
	use super::{Db, Options, EnableCommitPipelineStages, InternalOptions};
	use tempfile::tempdir;

	#[test]
	fn test_db_open_should_fail() {
		let tmp = tempdir().unwrap();
		let options = Options::with_columns(tmp.path(), 5);
		assert!(
			Db::open(&options).is_err(),
			"Database does not exist, so it should fail to open"
		);
		assert!(Db::open(&options).map(|_| ()).unwrap_err().to_string().contains("use open_or_create"));
	}

	#[test]
	fn test_db_open_or_create() {
		let tmp = tempdir().unwrap();
		let options = Options::with_columns(tmp.path(), 5);
		assert!(
			Db::open_or_create(&options).is_ok(),
			"New database should be created"
		);
		assert!(
			Db::open(&options).is_ok(),
			"Existing database should be reopened"
		);
	}

	#[test]
	fn test_indexed_keyvalues() {
		test_indexed_keyvalues_inner(EnableCommitPipelineStages::CommitOverlay);
		test_indexed_keyvalues_inner(EnableCommitPipelineStages::LogOverlay);
		test_indexed_keyvalues_inner(EnableCommitPipelineStages::DbFile);
		test_indexed_keyvalues_inner(EnableCommitPipelineStages::Standard);
	}
	fn test_indexed_keyvalues_inner(db_test: EnableCommitPipelineStages) {
		let tmp = tempdir().unwrap();
		let options = Options::with_columns(tmp.path(), 5);
		let col_nb = 0;

		let key1 = b"key1".to_vec();
		let key2 = b"key2".to_vec();
		let key3 = b"key3".to_vec();

		let mut inner_options = InternalOptions::default();
		inner_options.create = true;
		inner_options.commit_stages = db_test;
		let (db, wait_on) = Db::open_inner(&options, &inner_options).unwrap();
		assert!(db.inner.get(col_nb, key1.as_slice()).unwrap().is_none());

		db.commit(vec![
			(col_nb, key1.clone(), Some(b"value1".to_vec())),
		]).unwrap();
		wait_on.as_ref().map(|w| w.wait_notify());
		assert!(db_test.check_empty_overlay(&db, col_nb));

		assert_eq!(db.inner.get(col_nb, key1.as_slice()).unwrap(), Some(b"value1".to_vec()));

		db.commit(vec![
			(col_nb, key1.clone(), None),
			(col_nb, key2.clone(), Some(b"value2".to_vec())),
			(col_nb, key3.clone(), Some(b"value3".to_vec())),
		]).unwrap();
		wait_on.as_ref().map(|w| w.wait_notify());
		assert!(db_test.check_empty_overlay(&db, col_nb));

		assert!(db.inner.get(col_nb, key1.as_slice()).unwrap().is_none());
		assert_eq!(db.inner.get(col_nb, key2.as_slice()).unwrap(), Some(b"value2".to_vec()));
		assert_eq!(db.inner.get(col_nb, key3.as_slice()).unwrap(), Some(b"value3".to_vec()));

		db.commit(vec![
			(col_nb, key2.clone(), Some(b"value2b".to_vec())),
			(col_nb, key3.clone(), None),
		]).unwrap();
		wait_on.as_ref().map(|w| w.wait_notify());
		assert!(db_test.check_empty_overlay(&db, col_nb));

		assert!(db.inner.get(col_nb, key1.as_slice()).unwrap().is_none());
		assert_eq!(db.inner.get(col_nb, key2.as_slice()).unwrap(), Some(b"value2b".to_vec()));
		assert_eq!(db.inner.get(col_nb, key3.as_slice()).unwrap(), None);
	}

	#[test]
	fn test_indexed_overlay_against_backend() {
		let tmp = tempdir().unwrap();
		let options = Options::with_columns(tmp.path(), 5);
		let col_nb = 0;

		let key1 = b"key1".to_vec();
		let key2 = b"key2".to_vec();
		let key3 = b"key3".to_vec();

		let db_test = EnableCommitPipelineStages::DbFile;
		let mut inner_options = InternalOptions::default();
		inner_options.create = true;
		inner_options.commit_stages = db_test;
		let (db, wait_on) = Db::open_inner(&options, &inner_options).unwrap();

		db.commit(vec![
			(col_nb, key1.clone(), Some(b"value1".to_vec())),
			(col_nb, key2.clone(), Some(b"value2".to_vec())),
			(col_nb, key3.clone(), Some(b"value3".to_vec())),
		]).unwrap();
		wait_on.as_ref().map(|w| w.wait_notify());
		std::mem::drop(db);

		let mut inner_options = InternalOptions::default();
		inner_options.create = false;
		inner_options.commit_stages = EnableCommitPipelineStages::CommitOverlay;
		inner_options.skip_check_lock = true;
		let (db, wait_on) = Db::open_inner(&options, &inner_options).unwrap();
		assert_eq!(db.inner.get(col_nb, key1.as_slice()).unwrap(), Some(b"value1".to_vec()));
		assert_eq!(db.inner.get(col_nb, key2.as_slice()).unwrap(), Some(b"value2".to_vec()));
		assert_eq!(db.inner.get(col_nb, key3.as_slice()).unwrap(), Some(b"value3".to_vec()));
		db.commit(vec![
			(col_nb, key2.clone(), Some(b"value2b".to_vec())),
			(col_nb, key3.clone(), None),
		]).unwrap();
		wait_on.as_ref().map(|w| w.wait_notify());

		assert_eq!(db.inner.get(col_nb, key1.as_slice()).unwrap(), Some(b"value1".to_vec()));
		assert_eq!(db.inner.get(col_nb, key2.as_slice()).unwrap(), Some(b"value2b".to_vec()));
		assert_eq!(db.inner.get(col_nb, key3.as_slice()).unwrap(), None);
	}
}
