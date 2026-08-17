//! Deciding which shard owns a client.
//!
//! Routing has to satisfy three things at once. It must be a pure function of the client
//! ID, or two records for one account could land on different shards and race. It must
//! spread 65,536 possible clients evenly, or one shard does all the work. And it must be
//! cheap enough to disappear next to the CSV parse that produced the record.
//!
//! # Why this hash and not the standard library's
//!
//! `HashMap`'s default `SipHash` is the right choice where an attacker picks the keys, and
//! it is kept for exactly that map — see [`crate::account`]. Here it would be the wrong
//! trade: routing runs once per record on a two-byte key, and SipHash costs more than the
//! account update it is routing to.
//!
//! The mixer below is four 32-bit operations. It is seeded from
//! [`RandomState`](std::collections::hash_map::RandomState), so the client-to-shard map
//! differs on every process and cannot be precomputed by whoever is sending the records —
//! an attacker who could predict it could aim every record at one shard and serialise the
//! whole pipeline. That is the realistic attack on a router; collision-flooding a table is
//! not, because there is no table here.
//!
//! # Batching, and what it did not buy
//!
//! [`Router::route_into`] takes a slice of clients rather than one at a time, and the body
//! is branch-free 32-bit integer arithmetic with no dependency between lanes — the shape
//! an auto-vectoriser is supposed to take.
//!
//! It does not take it. Measured on rustc 1.74 by reading the emitted assembly, the loop
//! stays scalar at `-C target-cpu=x86-64`, `x86-64-v2` and `x86-64-v3`, with and without
//! LTO, both fused and split into a mixing pass and a reducing pass, and with the input
//! widened to `u32` so that the loads and stores share a lane width. Four formulations,
//! no vector register in any of them. Hand-written intrinsics would settle it, but they
//! need `unsafe`, and this crate forbids it outright.
//!
//! The batching is kept regardless, because it pays for itself elsewhere: it amortises the
//! channel send across a thousand records rather than paying per record, and it is the
//! precondition for vectorising this later, whether by a newer LLVM or by intrinsics behind
//! a feature flag.
//!
//! It is also worth keeping the cost in proportion. Routing is four arithmetic operations
//! per record. The engine spends on the order of 750 ns per record; this is nanoseconds.
//! Vectorising it would be optimising the wrong three orders of magnitude.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
};

/// Maps clients onto shards.
#[derive(Debug, Clone, Copy)]
pub struct Router {
    seed: u32,
    shards: u32,
}

impl Router {
    /// A router for `shards` shards, seeded for this process only.
    ///
    /// # Panics
    ///
    /// If `shards` is zero or exceeds `u32::MAX`.
    pub fn new(shards: usize) -> Self {
        assert!(shards > 0, "a router needs at least one shard");
        let shards = u32::try_from(shards).expect("shard count fits in u32");
        Self {
            seed: Self::random_seed(),
            shards,
        }
    }

    /// A router with a caller-chosen seed. Tests use this to make routing repeatable.
    pub fn with_seed(shards: usize, seed: u32) -> Self {
        assert!(shards > 0, "a router needs at least one shard");
        Self {
            seed,
            shards: u32::try_from(shards).expect("shard count fits in u32"),
        }
    }

    /// How many shards this router routes to.
    pub fn shards(&self) -> usize {
        self.shards as usize
    }

    /// The shard that owns `client`.
    #[inline]
    pub fn shard(&self, client: u16) -> usize {
        Self::reduce(Self::mix(client, self.seed), self.shards) as usize
    }

    /// Routes a whole chunk of clients, writing one shard index per client.
    ///
    /// `out` is truncated to the length of `clients`, or the other way round, so a short
    /// slice cannot silently leave stale indices behind.
    pub fn route_into(&self, clients: &[u32], out: &mut [u32]) {
        let seed = self.seed;
        let shards = self.shards;

        let n = clients.len().min(out.len());
        let clients = &clients[..n];
        let out = &mut out[..n];

        // Two passes rather than one, so the mixing pass is uniformly 32-bit and has the
        // best chance of being vectorised. On rustc 1.74 it is not — see the module docs
        // for what was tried. The split costs nothing either way.
        for (slot, &client) in out.iter_mut().zip(clients) {
            *slot = Self::mix32(client, seed);
        }
        for slot in out.iter_mut() {
            *slot = Self::reduce(*slot, shards);
        }
    }

    /// A 32-bit avalanche mixer over the client ID.
    ///
    /// The multiply-xorshift pair is the finaliser from MurmurHash3, which spreads two
    /// bytes of input across all 32 output bits.
    #[inline]
    fn mix(client: u16, seed: u32) -> u32 {
        Self::mix32(u32::from(client), seed)
    }

    /// The mixer over an already-widened client ID.
    ///
    /// Taking `u32` rather than `u16` is what makes the batch loop vectorisable: a 2-byte
    /// load feeding a 4-byte store gives the two ends of the loop different vector
    /// factors, and LLVM declines the whole loop rather than reconciling them. Widening in
    /// the caller, where the IDs are being collected anyway, costs nothing.
    #[inline]
    fn mix32(client: u32, seed: u32) -> u32 {
        let mut h = client ^ seed;
        h = h.wrapping_mul(0x85eb_ca6b);
        h ^= h >> 13;
        h = h.wrapping_mul(0xc2b2_ae35);
        h ^ (h >> 16)
    }

    /// Maps a hash uniformly onto `0..shards` without a division.
    ///
    /// `hash % shards` would need an integer division per record, which neither pipelines
    /// nor vectorises. Multiplying into the high half instead — Lemire's alternative to
    /// modulo — is one widening multiply and a shift, and it stays uniform as long as the
    /// hash is.
    #[inline]
    fn reduce(hash: u32, shards: u32) -> u32 {
        ((u64::from(hash) * u64::from(shards)) >> 32) as u32
    }

    /// A per-process seed, without a random-number dependency.
    ///
    /// `RandomState` is seeded by the standard library from the operating system, so
    /// hashing a fixed value with a fresh one yields a value an outsider cannot predict.
    fn random_seed() -> u32 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(0x5f37_1cb4_9ce2_a17d);
        (hasher.finish() >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_always_routes_to_the_same_shard() {
        let router = Router::new(7);
        for client in 0..=u16::MAX {
            assert_eq!(router.shard(client), router.shard(client));
        }
    }

    #[test]
    fn every_shard_index_is_in_range() {
        for shards in [1usize, 2, 3, 5, 8, 13, 64] {
            let router = Router::new(shards);
            for client in 0..=u16::MAX {
                assert!(
                    router.shard(client) < shards,
                    "{client} with {shards} shards"
                );
            }
        }
    }

    #[test]
    fn one_shard_takes_everything() {
        let router = Router::new(1);
        for client in [0, 1, 999, u16::MAX] {
            assert_eq!(router.shard(client), 0);
        }
    }

    #[test]
    fn the_batch_path_agrees_with_the_single_one() {
        let router = Router::new(6);
        let clients: Vec<u32> = (0..=u16::MAX).map(u32::from).collect();
        let mut out = vec![0u32; clients.len()];

        router.route_into(&clients, &mut out);

        for (&client, &shard) in clients.iter().zip(&out) {
            assert_eq!(
                shard as usize,
                router.shard(client as u16),
                "client {client}"
            );
        }
    }

    #[test]
    fn a_short_output_slice_routes_only_what_fits() {
        let router = Router::new(4);
        let clients = [1u32, 2, 3, 4, 5];
        let mut out = [u32::MAX; 3];

        router.route_into(&clients, &mut out);

        for (index, &shard) in out.iter().enumerate() {
            assert_eq!(shard as usize, router.shard(clients[index] as u16));
        }
    }

    /// The whole point of hashing rather than using `client % shards`: a partner that
    /// sends only even client IDs must not leave half the shards idle.
    #[test]
    fn the_load_is_spread_evenly() {
        const SHARDS: usize = 8;
        let router = Router::new(SHARDS);

        let mut counts = [0usize; SHARDS];
        for client in (0..=u16::MAX).step_by(2) {
            counts[router.shard(client)] += 1;
        }

        let total: usize = counts.iter().sum();
        let ideal = total / SHARDS;
        for (shard, &count) in counts.iter().enumerate() {
            let drift = (count as isize - ideal as isize).unsigned_abs();
            assert!(
                drift * 10 < ideal,
                "shard {shard} took {count}, expected about {ideal}"
            );
        }
    }

    #[test]
    fn different_seeds_route_differently() {
        let a = Router::with_seed(8, 1);
        let b = Router::with_seed(8, 2);

        let differing = (0..=u16::MAX).filter(|&c| a.shard(c) != b.shard(c)).count();
        assert!(
            differing > 40_000,
            "only {differing} of 65,536 clients moved between seeds"
        );
    }
}
