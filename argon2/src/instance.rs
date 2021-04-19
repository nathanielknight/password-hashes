//! Argon2 instance (i.e. state)

use crate::{
    Algorithm, Argon2, Block, Error, Memory, Version, BLOCK_SIZE, MAX_OUTLEN, MIN_OUTLEN,
    SYNC_POINTS,
};
use blake2::{
    digest::{self, VariableOutput},
    Blake2b, Digest, VarBlake2b,
};

#[cfg(feature = "parallel")]
use {
    alloc::vec::Vec,
    core::mem,
    rayon::iter::{ParallelBridge, ParallelIterator},
};

#[cfg(feature = "zeroize")]
use zeroize::Zeroize;

/// Number of pseudo-random values generated by one call to Blake in Argon2i
/// to generate reference block positions
const ADDRESSES_IN_BLOCK: u32 = 128;

/// Output size of BLAKE2b in bytes
const BLAKE2B_OUTBYTES: usize = 64;

/// Argon2 position: where we construct the block right now.
///
/// Used to distribute work between threads.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Position {
    pass: u32,
    lane: u32,
    slice: u32,
    index: u32,
}

/// Argon2 instance: memory pointer, number of passes, amount of memory, type,
/// and derived values.
///
/// Used to evaluate the number and location of blocks to construct in each
/// thread.
pub(crate) struct Instance<'a> {
    /// Memory blocks
    memory: Memory<'a>,

    /// Version
    version: Version,

    /// Number of passes
    passes: u32,

    /// Lane length
    lane_length: u32,

    /// Number of lanes
    lanes: u32,

    /// Number of threads
    threads: u32,

    /// Argon2 type
    alg: Algorithm,
}

impl<'a> Instance<'a> {
    /// Hash the given inputs with Argon2, writing the output into the
    /// provided buffer.
    pub fn hash(
        context: &Argon2<'_>,
        alg: Algorithm,
        initial_hash: digest::Output<Blake2b>,
        memory: Memory<'a>,
        out: &mut [u8],
    ) -> Result<(), Error> {
        let mut instance = Self::new(context, alg, initial_hash, memory)?;

        // Filling memory
        instance.fill_memory_blocks();

        // Finalization
        instance.finalize(out)
    }

    /// Hashes the inputs with BLAKE2b and creates first two blocks.
    ///
    /// Returns struct containing main memory with 2 blocks per lane initialized.
    #[allow(unused_mut)]
    fn new(
        context: &Argon2<'_>,
        alg: Algorithm,
        mut initial_hash: digest::Output<Blake2b>,
        memory: Memory<'a>,
    ) -> Result<Self, Error> {
        let lane_length = memory.segment_length() * SYNC_POINTS;

        let mut instance = Instance {
            version: context.version,
            memory,
            passes: context.t_cost,
            lane_length,
            lanes: context.lanes,
            threads: context.threads,
            alg,
        };

        if instance.threads > instance.lanes {
            instance.threads = instance.lanes;
        }

        // GENKAT note: this is where `initial_kat` would be called

        // Creating first blocks, we always have at least two blocks in a slice
        instance.fill_first_blocks(&initial_hash)?;

        #[cfg(feature = "zeroize")]
        initial_hash.zeroize();

        Ok(instance)
    }

    /// Create multiple mutable references for the current instance, one for every lane
    #[cfg(feature = "parallel")]
    #[allow(unsafe_code)]
    unsafe fn mut_self_refs(&mut self) -> Vec<usize> {
        let lanes = self.lanes;
        // This transmute can be skipped when a scoped threadpool is used (or when `spawn_unchecked()` gets stabilised)
        let this = mem::transmute::<_, &mut Instance<'static>>(self);
        let this: *mut Instance<'static> = this;
        let this = this as usize;

        // Dereference the raw pointer multiple times to create multiple mutable references
        core::iter::repeat(this).take(lanes as usize).collect()
    }

    #[cfg(feature = "parallel")]
    fn fill_memory_blocks_par(&mut self) {
        for r in 0..self.passes {
            for s in 0..SYNC_POINTS {
                // Safety: - All threads that receive a references will be joined before the item gets dropped
                //         - All the read and write operations *shouldn't* overlap
                #[allow(unsafe_code)]
                let self_refs = unsafe { self.mut_self_refs() };

                (0..self.lanes)
                    .zip(self_refs)
                    .par_bridge()
                    .for_each(|(l, self_ref)| {
                        #[allow(unsafe_code)]
                        let self_ref = unsafe { &mut *(self_ref as *mut Instance<'static>) };

                        self_ref.fill_segment(Position {
                            pass: r,
                            lane: l,
                            slice: s,
                            index: 0,
                        });
                    });
            }

            // GENKAT note: this is where `internal_kat` would be called
        }
    }

    /// Function that fills the entire memory t_cost times based on the first two
    /// blocks in each lane
    fn fill_memory_blocks(&mut self) {
        #[cfg(feature = "parallel")]
        if self.threads > 1 {
            self.fill_memory_blocks_par();
            return;
        }

        // Single-threaded version for p=1 case
        for r in 0..self.passes {
            for s in 0..SYNC_POINTS {
                for l in 0..self.lanes {
                    self.fill_segment(Position {
                        pass: r,
                        lane: l,
                        slice: s,
                        index: 0,
                    });
                }
            }

            // GENKAT note: this is where `internal_kat` would be called
        }
    }

    /// XORing the last block of each lane, hashing it, making the tag.
    fn finalize(&mut self, out: &mut [u8]) -> Result<(), Error> {
        let mut blockhash = self.memory.get_block((self.lane_length - 1) as usize);

        // XOR the last blocks
        for l in 1..self.lanes {
            let last_block_in_lane = l * self.lane_length + (self.lane_length - 1);
            blockhash ^= self.memory.get_block(last_block_in_lane as usize);
        }

        // Hash the result
        let mut blockhash_bytes = [0u8; BLOCK_SIZE];

        for (chunk, v) in blockhash_bytes.chunks_mut(8).zip(blockhash.iter()) {
            chunk.copy_from_slice(&v.to_le_bytes())
        }

        blake2b_long(&[&blockhash_bytes], out)?;

        #[cfg(feature = "zeroize")]
        blockhash.zeroize();

        #[cfg(feature = "zeroize")]
        blockhash_bytes.zeroize();

        Ok(())
    }

    /// Function creates first 2 blocks per lane
    fn fill_first_blocks(&mut self, blockhash: &[u8]) -> Result<(), Error> {
        let mut hash = [0u8; BLOCK_SIZE];

        for l in 0..self.lanes {
            // Make the first and second block in each lane as G(H0||0||i) or
            // G(H0||1||i)
            for i in 0u32..2u32 {
                blake2b_long(&[blockhash, &i.to_le_bytes(), &l.to_le_bytes()], &mut hash)?;
                self.memory
                    .get_block_mut((l * self.lane_length + i) as usize)
                    .load(&hash);
            }
        }

        Ok(())
    }

    /// Function that fills the segment using previous segments
    // TODO(tarcieri): optimized implementation (i.e. from opt.c instead of ref.c)
    fn fill_segment(&mut self, mut position: Position) {
        let mut address_block = Block::default();
        let mut input_block = Block::default();
        let zero_block = Block::default();

        let data_independent_addressing = (self.alg == Algorithm::Argon2i)
            || (self.alg == Algorithm::Argon2id
                && (position.pass == 0)
                && (position.slice < SYNC_POINTS / 2));

        if data_independent_addressing {
            input_block[0] = position.pass as u64;
            input_block[1] = position.lane as u64;
            input_block[2] = position.slice as u64;
            input_block[3] = self.memory.len() as u64;
            input_block[4] = self.passes as u64;
            input_block[5] = self.alg as u64;
        }

        let mut starting_index = 0;

        if position.pass == 0 && position.slice == 0 {
            starting_index = 2; // we have already generated the first two blocks

            // Don't forget to generate the first block of addresses
            if data_independent_addressing {
                next_addresses(&mut address_block, &mut input_block, &zero_block);
            }
        }

        // Offset of the current block
        let mut curr_offset = position.lane * self.lane_length
            + position.slice * self.memory.segment_length()
            + starting_index;

        let mut prev_offset = if 0 == curr_offset % self.lane_length {
            // Last block in this lane
            curr_offset + self.lane_length - 1
        } else {
            // Previous block
            curr_offset - 1
        };

        for i in starting_index..self.memory.segment_length() {
            // 1.1 Rotating prev_offset if needed
            if curr_offset % self.lane_length == 1 {
                prev_offset = curr_offset - 1;
            }

            // 1.2 Computing the index of the reference block
            // 1.2.1 Taking pseudo-random value from the previous block
            let pseudo_rand = if data_independent_addressing {
                if i % ADDRESSES_IN_BLOCK == 0 {
                    next_addresses(&mut address_block, &mut input_block, &zero_block);
                }
                address_block[(i % ADDRESSES_IN_BLOCK) as usize]
            } else {
                self.memory.get_block(prev_offset as usize)[0]
            };

            // 1.2.2 Computing the lane of the reference block
            let mut ref_lane = (pseudo_rand >> 32) as u32 % self.lanes;

            if position.pass == 0 && position.slice == 0 {
                // Can not reference other lanes yet
                ref_lane = position.lane;
            }

            // 1.2.3 Computing the number of possible reference block within the lane.
            position.index = i;

            let ref_index = self.index_alpha(
                position,
                (pseudo_rand & 0xFFFFFFFF) as u32,
                ref_lane == position.lane,
            );

            // 2 Creating a new block
            let ref_block = self
                .memory
                .get_block((self.lane_length * ref_lane + ref_index) as usize);
            let prev_block = self.memory.get_block(prev_offset as usize);

            // version 1.2.1 and earlier: overwrite, not XOR
            let without_xor = self.version == Version::V0x10 || position.pass == 0;
            self.memory.get_block_mut(curr_offset as usize).fill_block(
                prev_block,
                ref_block,
                !without_xor,
            );

            curr_offset += 1;
            prev_offset += 1;
        }
    }

    /// Computes absolute position of reference block in the lane following a skewed
    /// distribution and using a pseudo-random value as input.
    ///
    /// # Params
    /// - `position`: Pointer to the current position
    /// - `pseudo_rand`: 32-bit pseudo-random value used to determine the position
    /// - `same_lane`: Indicates if the block will be taken from the current lane.
    ///                If so we can reference the current segment.
    fn index_alpha(&self, position: Position, pseudo_rand: u32, same_lane: bool) -> u32 {
        // Pass 0:
        // - This lane: all already finished segments plus already constructed
        //   blocks in this segment
        // - Other lanes: all already finished segments
        //
        // Pass 1+:
        // - This lane: (SYNC_POINTS - 1) last segments plus already constructed
        //   blocks in this segment
        // - Other lanes : (SYNC_POINTS - 1) last segments
        let reference_area_size = if 0 == position.pass {
            // First pass
            if position.slice == 0 {
                // First slice
                position.index - 1 // all but the previous
            } else if same_lane {
                // The same lane => add current segment
                position.slice * self.memory.segment_length() + position.index - 1
            } else {
                position.slice * self.memory.segment_length()
                    - if position.index == 0 { 1 } else { 0 }
            }
        } else {
            // Second pass
            if same_lane {
                self.lane_length - self.memory.segment_length() + position.index - 1
            } else {
                self.lane_length
                    - self.memory.segment_length()
                    - if position.index == 0 { 1 } else { 0 }
            }
        };

        // 1.2.4. Mapping pseudo_rand to 0..<reference_area_size-1> and produce
        // relative position
        let mut relative_position = pseudo_rand as u64;
        relative_position = (relative_position * relative_position) >> 32;
        let relative_position = reference_area_size
            - 1
            - (((reference_area_size as u64 * relative_position) >> 32) as u32);

        // 1.2.5 Computing starting position
        let mut start_position = 0;

        if position.pass != 0 {
            start_position = if position.slice == SYNC_POINTS - 1 {
                0
            } else {
                (position.slice + 1) * self.memory.segment_length()
            }
        }

        // 1.2.6. Computing absolute position
        (start_position + relative_position as u32) % self.lane_length
    }
}

/// Compute next addresses
fn next_addresses(address_block: &mut Block, input_block: &mut Block, zero_block: &Block) {
    input_block[6] += 1;
    address_block.fill_block(*zero_block, *input_block, false);
    address_block.fill_block(*zero_block, *address_block, false);
}

/// BLAKE2b with an extended output
fn blake2b_long(inputs: &[&[u8]], mut out: &mut [u8]) -> Result<(), Error> {
    if out.len() < MIN_OUTLEN as usize {
        return Err(Error::OutputTooLong);
    }

    if out.len() > MAX_OUTLEN as usize {
        return Err(Error::OutputTooLong);
    }

    let outlen_bytes = (out.len() as u32).to_le_bytes();

    if out.len() <= BLAKE2B_OUTBYTES {
        let mut digest = VarBlake2b::new(out.len()).unwrap();
        digest::Update::update(&mut digest, &outlen_bytes);

        for input in inputs {
            digest::Update::update(&mut digest, input);
        }

        digest.finalize_variable(|hash| out.copy_from_slice(hash));
    } else {
        let mut digest = Blake2b::new();
        digest.update(&outlen_bytes);

        for input in inputs {
            digest.update(input);
        }

        let mut out_buffer = [0u8; BLAKE2B_OUTBYTES];
        out_buffer.copy_from_slice(&digest.finalize());

        out[..(BLAKE2B_OUTBYTES / 2)].copy_from_slice(&out_buffer[..(BLAKE2B_OUTBYTES / 2)]);
        out = &mut out[(BLAKE2B_OUTBYTES / 2)..];

        let mut in_buffer = [0u8; BLAKE2B_OUTBYTES];

        while out.len() > BLAKE2B_OUTBYTES {
            in_buffer.copy_from_slice(&out_buffer);
            out_buffer.copy_from_slice(&Blake2b::digest(&in_buffer));

            out[..(BLAKE2B_OUTBYTES / 2)].copy_from_slice(&out_buffer[..(BLAKE2B_OUTBYTES / 2)]);
            out = &mut out[(BLAKE2B_OUTBYTES / 2)..];
        }

        let mut digest = VarBlake2b::new(out.len()).unwrap();
        digest::Update::update(&mut digest, &out_buffer);
        digest.finalize_variable(|hash| out.copy_from_slice(hash));
    }

    Ok(())
}
