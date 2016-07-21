// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Snapshot network service implementation.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::fs;
use std::path::{Path, PathBuf};

use super::{ManifestData, StateRebuilder, BlockRebuilder};
use super::io::{SnapshotReader, LooseReader};

use blockchain::BlockChain;
use client::get_db_path;
use engine::Engine;
use error::Error;
use service::ClientIoMessage;
use spec::Spec;

use util::{Bytes, H256, Mutex, UtilError};
use util::io::IoChannel;
use util::journaldb::{self, Algorithm};
use util::kvdb::Database;
use util::snappy;

/// Statuses for restorations.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum RestorationStatus {
	///	No restoration.
	Inactive,
	/// Ongoing restoration.
	Ongoing,
	/// Failed restoration.
	Failed,
}

/// The interface for a snapshot network service.
/// This handles:
///    - restoration of snapshots to temporary databases.
///    - responding to queries for snapshot manifests and chunks
pub trait SnapshotService {
	/// Query the most recent manifest data.
	fn manifest(&self) -> Option<ManifestData>;

	/// Get raw chunk for a given hash.
	fn chunk(&self, hash: H256) -> Option<Bytes>;

	/// Ask the snapshot service for the restoration status.
	fn status(&self) -> RestorationStatus;

	/// Begin snapshot restoration.
	/// If restoration in-progress, this will reset it.
	/// From this point on, any previous snapshot may become unavailable.
	/// Returns true if successful, false otherwise.
	fn begin_restore(&self, manifest: ManifestData) -> bool;

	/// Feed a raw state chunk to the service to be processed asynchronously.
	/// no-op if not currently restoring.
	fn restore_state_chunk(&self, hash: H256, chunk: Bytes);

	/// Feed a raw block chunk to the service to be processed asynchronously.
	/// no-op if currently restoring.
	fn restore_block_chunk(&self, hash: H256, chunk: Bytes);
}

/// State restoration manager.
struct Restoration {
	state_chunks_left: HashSet<H256>,
	block_chunks_left: HashSet<H256>,
	state: StateRebuilder,
	blocks: BlockRebuilder,
	snappy_buffer: Bytes,
}

impl Restoration {
	// make a new restoration, building databases in the given path.
	fn new(manifest: &ManifestData, pruning: Algorithm, path: &Path, spec: &Spec) -> Result<Self, Error> {
		// try something that outputs a string as error. used here for DB stuff
		macro_rules! try_string {
			($($t: tt)*) => {
				try!(($($t)*).map_err(UtilError::SimpleString))
			}
		}

		let mut state_db_path = path.to_owned();
		state_db_path.push("state");

		let raw_db =
			try_string!(Database::open_default(&*state_db_path.to_string_lossy()));

		let version = ::util::rlp::encode(&journaldb::version(pruning));
		try_string!(raw_db.put(&journaldb::VERSION_KEY[..], &version[..]));

		let blocks = try!(BlockRebuilder::new(BlockChain::new(Default::default(), &spec.genesis_block(), path)));

		Ok(Restoration {
			state_chunks_left: manifest.state_hashes.iter().cloned().collect(),
			block_chunks_left: manifest.block_hashes.iter().cloned().collect(),
			state: StateRebuilder::new(raw_db),
			blocks: blocks,
			snappy_buffer: Vec::new(),
		})
	}

	// feeds a state chunk
	fn feed_state(&mut self, hash: H256, chunk: &[u8]) -> Result<(), Error> {
		if self.state_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(&chunk, &mut self.snappy_buffer));
			try!(self.state.feed(&self.snappy_buffer[..len]));

			// TODO: verify state root when done.
		}

		Ok(())
	}

	// feeds a block chunk
	fn feed_blocks(&mut self, hash: H256, chunk: &[u8], engine: &Engine) -> Result<(), Error> {
		if self.block_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(&chunk, &mut self.snappy_buffer));
			try!(self.blocks.feed(&self.snappy_buffer[..len], engine));

			if self.block_chunks_left.is_empty() {
				// connect out-of-order chunks.
				self.blocks.glue_chunks();
			}
		}

		Ok(())
	}

	// is everything done?
	fn is_done(&self) -> bool {
		self.block_chunks_left.is_empty() && self.state_chunks_left.is_empty()
	}
}

/// Type alias for client io channel.
pub type Channel = IoChannel<ClientIoMessage>;

/// Service implementation.
///
/// This will replace the client's state DB as soon as the last state chunk
/// is fed, and will replace the client's blocks DB when the last block chunk
/// is fed.
pub struct Service {
	restoration: Mutex<Option<Restoration>>,
	db_path: PathBuf,
	io_channel: Channel,
	pruning: Algorithm,
	status: Mutex<RestorationStatus>,
	reader: Option<LooseReader>,
	spec: Spec,
}

impl Service {
	/// Create a new snapshot service.
	pub fn new(spec: Spec, pruning: Algorithm, db_path: PathBuf, io_channel: Channel) -> Result<Self, Error> {
		let reader = {
			let mut snapshot_path = db_path.clone();
			snapshot_path.push("snapshot");

			LooseReader::new(snapshot_path).ok()
		};

		let service = Service {
			restoration: Mutex::new(None),
			db_path: db_path,
			io_channel: io_channel,
			pruning: pruning,
			status: Mutex::new(RestorationStatus::Inactive),
			reader: reader,
			spec: spec,
		};

		// create the snapshot dir if it doesn't exist.
		match fs::create_dir_all(service.snapshot_dir()) {
			Err(e) => {
				if e.kind() != ErrorKind::AlreadyExists {
					return Err(e.into())
				}
			}
			_ => {}
		}

		// delete the temporary restoration dir if it does exist.
		match fs::remove_dir_all(service.restoration_dir()) {
			Err(e) => {
				if e.kind() != ErrorKind::NotFound {
					return Err(e.into())
				}
			}
			_ => {}
		}

		Ok(service)
	}

	// Get the client db root.
	fn client_db_root(&self) -> PathBuf {
		get_db_path(&self.db_path, self.pruning)
	}

	// Get the snapshot directory path.
	fn snapshot_dir(&self) -> PathBuf {
		let mut path = self.db_path.clone();
		path.push("snapshot");
		path
	}

	// Get the restoration directory path.
	fn restoration_dir(&self) -> PathBuf {
		let mut path = self.snapshot_dir();
		path.push("restoration");
		path
	}

	// replace one of the client's databases with our own.
	// the database handle must be closed before doing this.
	fn replace_client_db(&self, name: &str) -> Result<(), Error> {
		let mut client_db = self.client_db_root();
		client_db.push(name);

		let mut our_db = self.restoration_dir();
		our_db.push(name);

		trace!(target: "snapshot", "replacing {:?} with {:?}", client_db, our_db);

		let mut backup_db = self.db_path.clone();
		backup_db.push(format!("backup_{}", name));

		let _ = fs::remove_dir_all(&backup_db);

		let existed = match fs::rename(&client_db, &backup_db) {
			Ok(_) => true,
			Err(e) => if let ErrorKind::NotFound = e.kind() {
				false
			} else {
				return Err(e.into());
			}
		};

		match fs::rename(&our_db, &client_db) {
			Ok(_) => {
				// clean up the backup.
				if existed {
					try!(fs::remove_dir_all(&backup_db));
				}
				Ok(())
			}
			Err(e) => {
				// restore the backup.
				if existed {
					try!(fs::rename(&backup_db, client_db));
				}
				Err(e.into())
			}
		}
	}

	// finalize the restoration. this accepts an already-locked
	// restoration as an argument -- so acquiring it again _will_
	// lead to deadlock.
	fn finalize_restoration(&self, rest: &mut Option<Restoration>) -> Result<(), Error> {
		trace!(target: "snapshot", "finalizing restoration");

		// destroy the restoration before replacing databases.
		*rest = None;

		try!(self.replace_client_db("state"));
		try!(self.replace_client_db("blocks"));
		try!(self.replace_client_db("extras"));

		*self.status.lock() = RestorationStatus::Inactive;

		// TODO: take control of restored snapshot.
		let _ = fs::remove_dir_all(self.restoration_dir());

		Ok(())
	}

	/// Feed a chunk of either kind. no-op if no restoration or status is wrong.
	fn feed_chunk(&self, hash: H256, chunk: &[u8], is_state: bool) -> Result<(), Error> {
		match self.status() {
			RestorationStatus::Inactive | RestorationStatus::Failed => Ok(()),
			RestorationStatus::Ongoing => {
				// TODO: be able to process block chunks and state chunks at same time?
				let mut restoration = self.restoration.lock();

				let res = {
					let rest = match *restoration {
						Some(ref mut r) => r,
						None => return Ok(()),
					};

					match is_state {
						true => rest.feed_state(hash, chunk),
						false => rest.feed_blocks(hash, chunk, &*self.spec.engine),
					}.map(|_| rest.is_done())
				};

				match res {
					Ok(true) => self.finalize_restoration(&mut *restoration),
					other => other.map(drop),
				}
			}
		}
	}

	/// Feed a state chunk to be processed synchronously.
	pub fn feed_state_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, true) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during state restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}

	/// Feed a block chunk to be processed synchronously.
	pub fn feed_block_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, false) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during block restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}
}

impl SnapshotService for Service {
	fn manifest(&self) -> Option<ManifestData> {
		self.reader.as_ref().map(|r| r.manifest().clone())
	}

	fn chunk(&self, hash: H256) -> Option<Bytes> {
		self.reader.as_ref().and_then(|r| r.chunk(hash).ok())
	}

	fn status(&self) -> RestorationStatus {
		*self.status.lock()
	}

	fn begin_restore(&self, manifest: ManifestData) -> bool {
		let rest_dir = self.restoration_dir();

		let mut res = self.restoration.lock();

		// tear down existing restoration.
		*res = None;

		// delete and restore the restoration dir.
		if let Err(e) = fs::remove_dir_all(&rest_dir).and_then(|_| fs::create_dir_all(&rest_dir)) {
			match e.kind() {
				ErrorKind::NotFound => {},
				_ => {
					warn!("encountered error {} while beginning snapshot restoration.", e);
					return false;
				}
			}
		}

		// make new restoration.
		*res = match Restoration::new(&manifest, self.pruning, &rest_dir, &self.spec) {
				Ok(b) => Some(b),
				Err(e) => {
					warn!("encountered error {} while beginning snapshot restoration.", e);
					return false;
				}
		};

		*self.status.lock() = RestorationStatus::Ongoing;
		true
	}

	fn restore_state_chunk(&self, hash: H256, chunk: Bytes) {
		self.io_channel.send(ClientIoMessage::FeedStateChunk(hash, chunk))
			.expect("snapshot service and io service are kept alive by client service; qed");
	}

	fn restore_block_chunk(&self, hash: H256, chunk: Bytes) {
		self.io_channel.send(ClientIoMessage::FeedBlockChunk(hash, chunk))
			.expect("snapshot service and io service are kept alive by client service; qed");
	}
}