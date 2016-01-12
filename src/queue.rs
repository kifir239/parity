use util::*;
use blockchain::*;
use views::{BlockView};
use verification::*;
use error::*;
use engine::Engine;

/// A queue of blocks. Sits between network or other I/O and the BlockChain.
/// Sorts them ready for blockchain insertion.
pub struct BlockQueue {
	bc: Arc<RwLock<BlockChain>>,
	engine: Arc<Box<Engine>>,
}

impl BlockQueue {
	/// Creates a new queue instance.
	pub fn new(bc: Arc<RwLock<BlockChain>>, engine: Arc<Box<Engine>>) -> BlockQueue {
		BlockQueue {
			bc: bc,
			engine: engine,
		}
	}

	/// Clear the queue and stop verification activity.
	pub fn clear(&mut self) {
	}

	/// Add a block to the queue.
	pub fn import_block(&mut self, bytes: &[u8]) -> ImportResult {
		let header = BlockView::new(bytes).header();
		if self.bc.read().unwrap().is_known(&header.hash()) {
			return Err(ImportError::AlreadyInChain);
		}
		try!(verify_block_basic(bytes, self.engine.deref().deref()));
		try!(verify_block_unordered(bytes, self.engine.deref().deref()));
		try!(verify_block_final(bytes, self.engine.deref().deref(), self.bc.read().unwrap().deref()));
		self.bc.write().unwrap().insert_block(bytes);
		Ok(())
	}
}
