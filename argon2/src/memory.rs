//! Memory blocks

use crate::Block;

/// Number of synchronization points between lanes per pass
pub(crate) const SYNC_POINTS: u32 = 4;

/// Structure containing references to the memory blocks
pub(crate) struct Memory<'a> {
    /// Memory blocks
    data: &'a mut [Block],

    /// Size of a memory segment in blocks
    segment_length: u32,
}

impl<'a> Memory<'a> {
    /// Align memory size.
    ///
    /// Minimum memory_blocks = 8*`L` blocks, where `L` is the number of lanes.
    pub(crate) fn segment_length_for_params(m_cost: u32, lanes: u32) -> u32 {
        let memory_blocks = if m_cost < 2 * SYNC_POINTS * lanes {
            2 * SYNC_POINTS * lanes
        } else {
            m_cost
        };

        memory_blocks / (lanes * SYNC_POINTS)
    }

    /// Instantiate a new memory struct
    pub(crate) fn new(data: &'a mut [Block], segment_length: u32) -> Self {
        Self {
            data,
            segment_length,
        }
    }

    /// Get a copy of the block
    pub(crate) fn get_block(&self, idx: usize) -> Block {
        self.data[idx]
    }

    /// Get a mutable reference to the block
    pub(crate) fn get_block_mut(&mut self, idx: usize) -> &mut Block {
        &mut self.data[idx]
    }

    /// Size of the memory
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    /// Size of a memory segment
    #[inline]
    pub(crate) fn segment_length(&self) -> u32 {
        self.segment_length
    }
}
