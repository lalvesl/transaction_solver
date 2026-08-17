//! Deterministic generator for benchmark input.
//!
//! Writes a CSV transaction log to stdout. The same arguments always produce a
//! byte-identical file, on any machine and any platform: the generator carries its own
//! SplitMix64 rather than depending on a random-number crate, whose stream is free to
//! change between releases. That is what makes a benchmark reproducible — a number is
//! only comparable against another number measured on the same bytes.
//!
//! ```console
//! $ cargo run --release --example generate_transactions -- --bytes 1GiB > transactions.csv
//! ```
//!
//! The generator tracks enough per-client state to emit *valid* work: a dispute always
//! names a real, currently-undisputed deposit of that same client, and a resolve or
//! chargeback always names one of that client's open disputes. It also stops emitting for
//! a client once it has charged one back, since every later record for a frozen account
//! would only be rejected — which would benchmark the diagnostics, not the engine.

use std::{
    fmt::Write as _,
    io::{self, BufWriter, Write},
    process::ExitCode,
};

use clap::{Parser, ValueEnum};

/// How many of a client's recent deposits stay reachable for a dispute. A ring rather
/// than the single newest deposit, so lookups do not all hit the same key.
const RING: usize = 4;

/// Weights are out of 10,000 records.
const SCALE: u64 = 10_000;

#[derive(Debug, Parser)]
#[command(about = "Generate a reproducible transaction log for benchmarking")]
struct Args {
    /// Target size. Accepts a plain byte count or a suffix: 512MiB, 1GiB, 2GB.
    #[arg(long, value_parser = parse_size)]
    bytes: Option<u64>,

    /// Target number of records.
    #[arg(long)]
    records: Option<u64>,

    /// Seed. The same seed and arguments always produce the same bytes.
    #[arg(long, default_value_t = 0x5EED_C0DE_1234_5678)]
    seed: u64,

    /// Number of distinct clients. Capped at the `u16` key space.
    #[arg(long, default_value_t = 65_535, value_parser = clap::value_parser!(u32).range(1..=65_536))]
    clients: u32,

    /// Which blend of transaction types to emit.
    #[arg(long, value_enum, default_value_t = Mix::Balanced)]
    mix: Mix,

    /// Share of records, per thousand, deliberately made invalid.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=1000))]
    corrupt_per_mille: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mix {
    /// Deposit-heavy, the way a real log looks. Most deposits are never disputed, so they
    /// stay disputable to the end: this is the memory-hungry case.
    Balanced,
    /// Almost everything is disputed and then resolved, so records are released as fast
    /// as they are created and retention stays flat.
    Settled,
}

/// Cumulative thresholds out of [`SCALE`]: deposit, withdrawal, dispute, resolve, and the
/// remainder is chargeback.
struct Weights {
    deposit: u64,
    withdrawal: u64,
    dispute: u64,
    resolve: u64,
}

impl Mix {
    fn weights(self) -> Weights {
        match self {
            // 62% / 30% / 5% / 2.96%, leaving 0.04% chargebacks. Chargebacks are kept
            // rare on purpose: each one freezes a client for good, and a log that froze
            // every account early would stop measuring anything.
            Mix::Balanced => Weights {
                deposit: 6_200,
                withdrawal: 9_200,
                dispute: 9_700,
                resolve: 9_996,
            },
            Mix::Settled => Weights {
                deposit: 3_400,
                withdrawal: 3_400,
                dispute: 6_700,
                resolve: 10_000,
            },
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();

    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());

    match generate(&args, &mut out).and_then(|summary| out.flush().map(|()| summary)) {
        Ok(summary) => {
            eprintln!(
                "{} records, {} bytes, {} clients, mix={:?}, seed={:#x}",
                summary.records, summary.bytes, args.clients, args.mix, args.seed
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Summary {
    records: u64,
    bytes: u64,
}

fn generate<W: Write>(args: &Args, out: &mut W) -> io::Result<Summary> {
    let weights = args.mix.weights();
    let mut rng = SplitMix64::new(args.seed);
    let mut clients = vec![ClientState::default(); args.clients as usize];

    let (target_bytes, target_records) = match (args.bytes, args.records) {
        (None, None) => (Some(1 << 30), None),
        other => other,
    };

    let mut row = String::with_capacity(64);
    let mut next_tx: u32 = 1;
    let mut records = 0;
    let mut frozen = 0;

    const HEADER: &str = "type,client,tx,amount\n";
    out.write_all(HEADER.as_bytes())?;
    let mut bytes = HEADER.len() as u64;

    while target_bytes.map_or(true, |b| bytes < b) && target_records.map_or(true, |r| records < r) {
        // Every client charged back, or transaction IDs exhausted: nothing left to emit.
        if frozen == clients.len() || next_tx == u32::MAX {
            break;
        }

        let client = rng.below(args.clients as u64) as u32;
        let state = &mut clients[client as usize];
        if state.frozen {
            continue;
        }

        row.clear();
        if args.corrupt_per_mille > 0 && rng.below(1_000) < args.corrupt_per_mille {
            write_corrupt(&mut row, &mut rng, client, &mut next_tx);
        } else {
            let roll = rng.below(SCALE);
            let kind = if roll < weights.deposit {
                Kind::Deposit
            } else if roll < weights.withdrawal {
                Kind::Withdrawal
            } else if roll < weights.dispute {
                Kind::Dispute
            } else if roll < weights.resolve {
                Kind::Resolve
            } else {
                Kind::Chargeback
            };

            if write_record(&mut row, &mut rng, client, state, &mut next_tx, kind) {
                frozen += 1;
            }
        }

        out.write_all(row.as_bytes())?;
        bytes += row.len() as u64;
        records += 1;
    }

    Ok(Summary { records, bytes })
}

#[derive(Clone, Copy)]
enum Kind {
    Deposit,
    Withdrawal,
    Dispute,
    Resolve,
    Chargeback,
}

/// Writes one record. Returns true if it froze the client.
fn write_record(
    row: &mut String,
    rng: &mut SplitMix64,
    client: u32,
    state: &mut ClientState,
    next_tx: &mut u32,
    kind: Kind,
) -> bool {
    // A reference needs something to refer to; without it, fall back to a deposit so the
    // log stays valid rather than filling up with rejections.
    let kind = match kind {
        Kind::Dispute if state.deposits.is_empty() => Kind::Deposit,
        Kind::Resolve | Kind::Chargeback if state.disputed.is_empty() => Kind::Deposit,
        other => other,
    };

    match kind {
        Kind::Deposit => {
            let tx = *next_tx;
            *next_tx += 1;
            let units = rng.below(1_000) + 1;
            let cents = rng.below(10_000);
            let _ = writeln!(row, "deposit,{client},{tx},{units}.{cents:04}");
            state.deposits.push(tx);
            if state.deposits.len() > RING {
                state.deposits.remove(0);
            }
        }
        Kind::Withdrawal => {
            let tx = *next_tx;
            *next_tx += 1;
            // Small next to the deposits, so most withdrawals are covered by the balance.
            let units = rng.below(10);
            let cents = rng.below(10_000);
            let _ = writeln!(row, "withdrawal,{client},{tx},{units}.{cents:04}");
        }
        Kind::Dispute => {
            let index = rng.below(state.deposits.len() as u64) as usize;
            let tx = state.deposits.remove(index);
            let _ = writeln!(row, "dispute,{client},{tx},");
            state.disputed.push(tx);
        }
        Kind::Resolve => {
            let index = rng.below(state.disputed.len() as u64) as usize;
            let tx = state.disputed.remove(index);
            let _ = writeln!(row, "resolve,{client},{tx},");
        }
        Kind::Chargeback => {
            let index = rng.below(state.disputed.len() as u64) as usize;
            let tx = state.disputed.remove(index);
            let _ = writeln!(row, "chargeback,{client},{tx},");
            state.frozen = true;
            return true;
        }
    }

    false
}

/// One of the malformed shapes a partner actually sends.
fn write_corrupt(row: &mut String, rng: &mut SplitMix64, client: u32, next_tx: &mut u32) {
    let tx = *next_tx;
    *next_tx += 1;

    match rng.below(5) {
        0 => {
            let _ = writeln!(row, "transfer,{client},{tx},1.0000");
        }
        1 => {
            let _ = writeln!(row, "deposit,{client},{tx},1.00001");
        }
        2 => {
            let _ = writeln!(row, "deposit,{client},{tx},-1.0000");
        }
        3 => {
            let _ = writeln!(row, "deposit,{client},{tx},");
        }
        _ => {
            let _ = writeln!(row, "withdrawal,{client},{tx},not-a-number");
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ClientState {
    /// Deposits that could still be disputed.
    deposits: Vec<u32>,
    /// Disputes that are currently open.
    disputed: Vec<u32>,
    frozen: bool,
}

/// SplitMix64. Small, fast, and fixed forever, which is the only property that matters
/// here: the same seed must produce the same file in five years' time.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform enough for generating a workload; the modulo bias is irrelevant here.
    fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        self.next_u64() % bound
    }
}

fn parse_size(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw {
        _ if raw.ends_with("GiB") => (&raw[..raw.len() - 3], 1 << 30),
        _ if raw.ends_with("MiB") => (&raw[..raw.len() - 3], 1 << 20),
        _ if raw.ends_with("KiB") => (&raw[..raw.len() - 3], 1 << 10),
        _ if raw.ends_with("GB") => (&raw[..raw.len() - 2], 1_000_000_000),
        _ if raw.ends_with("MB") => (&raw[..raw.len() - 2], 1_000_000),
        _ if raw.ends_with("KB") => (&raw[..raw.len() - 2], 1_000),
        _ => (raw, 1),
    };

    digits
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("`{raw}` is not a size"))?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("`{raw}` overflows"))
}
