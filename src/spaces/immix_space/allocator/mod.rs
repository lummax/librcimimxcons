// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

mod normal_allocator;
mod overflow_allocator;
mod evac_allocator;

pub use self::normal_allocator::NormalAllocator;
pub use self::overflow_allocator::OverflowAllocator;
pub use self::evac_allocator::EvacAllocator;
use spaces::immix_space::block_info::BlockInfo;

use constants::LINE_SIZE;
use gc_object::GCObjectRef;

/// A type alias for the block, the current low and high offset.
type BlockTuple = (*mut BlockInfo, u16, u16);

/// Trait for the allocators in the immix space.
///
/// Only use `get_all_blocks()` and `allocate()` from outside.
pub trait Allocator {

    /// Get all block managed by the allocator, draining any local
    /// collections.
    fn get_all_blocks(&mut self) -> Vec<*mut BlockInfo>;

    /// Get the current block to allocate from.
    fn take_current_block(&mut self) -> Option<BlockTuple>;

    /// Set the current block to allocate from.
    fn put_current_block(&mut self, block_tuple: BlockTuple);

    /// Get a new block from a block resource.
    fn get_new_block(&mut self) -> Option<BlockTuple>;

    /// Callback if no hole of `size` bytes was found in the current block.
    fn handle_no_hole(&mut self, size: usize) -> Option<BlockTuple>;

    /// Callback if the given `block` has no holes left.
    fn handle_full_block(&mut self, block: *mut BlockInfo);

    /// Allocate an object of `size` bytes or return `None`.
    ///
    /// This allocation will be aligned (see `GCObject.object_size()`). This
    /// object is not initialized, just the memory chunk is allocated.
    ///
    /// This will try to find a hole in the `take_current_block()`. If there
    /// Is no hole `handle_no_hole()` will be called. If this function returns
    /// `None` a 'get_new_block()' is requested.
    fn allocate(&mut self, size: usize) -> Option<GCObjectRef> {
        debug!("Request to allocate an object of size {}", size);
        self.take_current_block()
            .and_then(|tp| self.scan_for_hole(size, tp))
            .or_else(|| self.handle_no_hole(size))
            .or_else(|| self.get_new_block())
            .map(|tp| self.allocate_from_block(size, tp))
            .map(|(tp, object)| {
                self.put_current_block(tp);
                valgrind_malloclike!(object, size);
                object
            })
    }

    /// Scan a block tuple for a hole of `size` bytes and return a matching
    /// hole.
    ///
    /// If no hole was found `handle_full_block()` is called and None
    /// returned.
    fn scan_for_hole(&mut self, size: usize, block_tuple: BlockTuple) -> Option<BlockTuple> {
        let (block, low, high) = block_tuple;
        match (high - low) as usize >= size {
            true => {
                debug!("Found hole in block {:p}", block);
                Some(block_tuple)
            },
            false => match unsafe{ (*block).scan_block(high) } {
                None => {
                    self.handle_full_block(block);
                    None
                },
                Some((low, high)) => self.scan_for_hole(size, (block, low, high)),
            }
        }
    }

    /// Allocate an uninitialized object of `size` bytes from the block tuple.
    ///
    /// Returns the block tuple with a modified low offset and the allocated
    /// object pointer.
    ///
    /// _Note_: This must only be called if there is a hole of `size` bytes
    /// starting at low offset!
    fn allocate_from_block(&self, size: usize, block_tuple: BlockTuple)
        -> (BlockTuple, GCObjectRef) {
            let (block, low, high) = block_tuple;
            let object = unsafe { (*block).offset(low as usize) };
            debug!("Allocated object {:p} of size {} in {:p} (object={})",
                   object, size, block, size >= LINE_SIZE);
            ((block, low + size as u16, high), object)
        }
}
