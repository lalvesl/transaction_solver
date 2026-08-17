//! The sharded pipeline: one reader, N engines, one writer.
//!
//! ```text
//!                     ┌──────────► shard 0 ─┐
//!   input ── reader ──┼──────────► shard 1 ─┼──► writer ──► stdout
//!  (blocking) parse   └──────────► shard N ─┘   (single)    stderr
//!             route          mpsc<Batch>          mpsc<Out>
//! ```
//!
//! # Why this is safe to shard at all
//!
//! No transaction can reach across clients: a dispute, resolve or chargeback must name a
//! transaction belonging to the same client that sent it ([D4]). Account state is
//! therefore perfectly partitionable by client, and a shard needs to synchronise with no
//! other shard for anything. Routing is a pure function of the client ID, so every record
//! for an account reaches the same shard, and a single reader feeding a FIFO channel keeps
//! them in input order. Per-account ordering — the only ordering the semantics depend on —
//! survives.
//!
//! # Why the reader is a thread and the shards are tasks
//!
//! Reading and parsing is blocking, CPU-bound work on a file or a pipe, so it runs on its
//! own thread and pushes into the channels with `blocking_send`. The shards are ordinary
//! tasks: their work is CPU-bound too, but it arrives in bounded batches, which keeps any
//! one of them from occupying a runtime worker for long. The channels are bounded, so a
//! shard that falls behind applies backpressure to the reader rather than letting the
//! queue grow without limit.
//!
//! # Shutdown, one shard at a time
//!
//! When the input ends the reader sends [`Batch::Finish`] to each shard in turn and waits
//! for its acknowledgement before moving to the next. Shards could all dump their accounts
//! at once, but they would then be queueing behind a single writer that can only serialise
//! them anyway; draining one at a time keeps the writer's queue shallow and the peak memory
//! of the shutdown bounded by one shard rather than all of them.
//!
//! [D4]: ../README.md

use std::{
    io::{Read, Write},
    thread,
};

use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};

use crate::{
    account::AccountRow,
    engine::Engine,
    error::{FatalError, RecordError},
    output::RowWriter,
    pipeline::{Entry, Reader, Reading},
    route::Router,
};

/// How many records a shard receives per message.
///
/// Large enough that the per-message cost of the channel disappears against the work, small
/// enough that a shard yields to the runtime often and that an in-flight batch is a
/// negligible amount of memory.
const BATCH: usize = 1024;

/// How many records the reader stages before routing them.
///
/// Routing a chunk at a time amortises the channel send and keeps the routing loop in one
/// place; see `route` for what that did and did not buy.
const CHUNK: usize = 4096;

/// How many batches may be queued to one shard before the reader blocks.
const QUEUE: usize = 16;

/// What a shard receives.
#[derive(Debug)]
enum Batch {
    Apply(Vec<Entry>),
    /// Drain everything and acknowledge. Sent to one shard at a time.
    Finish(oneshot::Sender<()>),
}

/// What the writer receives.
#[derive(Debug)]
enum Out {
    Row(AccountRow),
    Rejected { line: u64, message: String },
}

/// Chooses a shard count for this machine.
///
/// Two threads are reserved before sharding starts: one reads and parses, one writes. Both
/// are real, permanently busy stages, and taking them out of the count keeps the shards
/// from contending with the two stages that feed and drain them. Below four cores that
/// subtraction would leave nothing, so the floor is two shards — still worth having, since
/// a shard blocked on a channel is not using a core. A single-core machine gets one shard
/// and one channel, which is the sharding turned off rather than a special case in the
/// code.
pub fn shards_for(parallelism: usize) -> usize {
    match parallelism {
        0 | 1 => 1,
        n => n.saturating_sub(2).max(2),
    }
}

/// Runs the whole pipeline, returning the number of rejected records.
pub fn run(
    input: Box<dyn Read + Send>,
    stdout: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
    shards: usize,
) -> Result<u64, FatalError> {
    let router = Router::new(shards);

    // One runtime worker per shard. The reader and the writer are counted for separately:
    // the reader is its own thread, and the writer is a task that spends nearly all its
    // time blocked on a channel.
    // No timers and no reactor: nothing here waits on the clock or on a socket, so the
    // runtime is only being used for its scheduler and its channels.
    let runtime = runtime::Builder::new_multi_thread()
        .worker_threads(shards)
        .build()
        .map_err(FatalError::Runtime)?;

    runtime.block_on(async move {
        let (out_tx, out_rx) = mpsc::channel::<Out>(BATCH);
        let writer = tokio::spawn(write_out(out_rx, stdout, stderr));
        let writer_handle = out_tx.clone();

        let mut senders = Vec::with_capacity(shards);
        let mut workers = Vec::with_capacity(shards);
        for _ in 0..shards {
            let (tx, rx) = mpsc::channel::<Batch>(QUEUE);
            senders.push(tx);
            workers.push(tokio::spawn(shard(rx, out_tx.clone(), shards)));
        }
        // The writer must see the channel close once everyone is done, so this end of it
        // is dropped here rather than lingering for the length of the run.
        drop(out_tx);

        // Parsing is blocking work; it gets a thread of its own rather than a runtime
        // worker, and reaches the shards through `blocking_send`.
        let reader_out = writer_handle.clone();
        let read = thread::spawn(move || read_into(input, &senders, router, &reader_out));

        let fatal = read.join().unwrap_or(Err(FatalError::ReaderPanicked));
        drop(writer_handle);

        for worker in workers {
            let _ = worker.await;
        }

        let rejected = writer.await.unwrap_or(Ok(0))?;
        fatal?;
        Ok(rejected)
    })
}

/// Reads, parses, routes, and finally shuts the shards down one by one.
fn read_into(
    input: Box<dyn Read + Send>,
    senders: &[mpsc::Sender<Batch>],
    router: Router,
    out: &mpsc::Sender<Out>,
) -> Result<(), FatalError> {
    let shards = senders.len();

    let mut staged: Vec<Entry> = Vec::with_capacity(CHUNK);
    let mut clients: Vec<u32> = Vec::with_capacity(CHUNK);
    let mut targets: Vec<u32> = vec![0; CHUNK];
    let mut batches: Vec<Vec<Entry>> = (0..shards).map(|_| Vec::with_capacity(BATCH)).collect();

    // Even a stream that cannot be opened has to fall through to the shutdown below, or
    // the shards would wait for a `Finish` that never comes.
    let mut fatal = None;
    let mut reader = match Reader::new(input) {
        Ok(reader) => Some(reader),
        Err(error) => {
            fatal = Some(error);
            None
        }
    };

    while let Some(reader) = reader.as_mut() {
        match reader.next() {
            Ok(None) => break,
            // A record the parser refused never reaches a shard, so the reader reports it.
            Ok(Some(Reading::Rejected { line, error })) => {
                let _ = out.blocking_send(rejected(line, &error));
            }
            Ok(Some(Reading::Ready(entry))) => staged.push(Entry {
                line: entry.line,
                transaction: entry.transaction,
            }),
            Err(error) => {
                fatal = Some(error);
                break;
            }
        }

        if staged.len() == CHUNK {
            flush_chunk(
                &mut staged,
                &mut clients,
                &mut targets,
                &mut batches,
                senders,
                router,
            );
        }
    }

    flush_chunk(
        &mut staged,
        &mut clients,
        &mut targets,
        &mut batches,
        senders,
        router,
    );
    for (shard, batch) in batches.iter_mut().enumerate() {
        send(&senders[shard], std::mem::take(batch));
    }

    // One shard at a time: see the module docs.
    for sender in senders {
        let (ack, wait) = oneshot::channel();
        if sender.blocking_send(Batch::Finish(ack)).is_err() {
            continue;
        }
        let _ = wait.blocking_recv();
    }

    fatal.map_or(Ok(()), Err)
}

/// Routes a staged chunk into per-shard batches, sending any batch that filled up.
fn flush_chunk(
    staged: &mut Vec<Entry>,
    clients: &mut Vec<u32>,
    targets: &mut [u32],
    batches: &mut [Vec<Entry>],
    senders: &[mpsc::Sender<Batch>],
    router: Router,
) {
    if staged.is_empty() {
        return;
    }

    clients.clear();
    clients.extend(
        staged
            .iter()
            .map(|entry| u32::from(entry.transaction.client())),
    );
    router.route_into(clients, targets);

    for (entry, &target) in staged.drain(..).zip(targets.iter()) {
        let shard = target as usize;
        batches[shard].push(entry);
        if batches[shard].len() == BATCH {
            send(&senders[shard], std::mem::take(&mut batches[shard]));
            batches[shard] = Vec::with_capacity(BATCH);
        }
    }
}

/// Sends a batch, ignoring a shard that has already gone away.
fn send(sender: &mpsc::Sender<Batch>, batch: Vec<Entry>) {
    if !batch.is_empty() {
        let _ = sender.blocking_send(Batch::Apply(batch));
    }
}

/// One shard: an engine over the slice of the key space routed to it.
async fn shard(mut inbox: mpsc::Receiver<Batch>, out: mpsc::Sender<Out>, shards: usize) {
    // Each shard owns roughly its share of the 65,536 possible clients, so it pre-sizes for
    // that share rather than for all of them. A little headroom absorbs an uneven split.
    let capacity = (u16::MAX as usize / shards).saturating_add(64);
    let mut engine = Engine::with_capacity(capacity);

    while let Some(batch) = inbox.recv().await {
        match batch {
            Batch::Apply(entries) => {
                for entry in entries {
                    if let Err(error) = engine.apply(entry.transaction) {
                        if out.send(rejected(entry.line, &error)).await.is_err() {
                            return;
                        }
                    }
                }

                // Frozen accounts leave now rather than at the end of the run: this is
                // the whole point of evicting them.
                for row in engine.take_evicted() {
                    if out.send(Out::Row(row)).await.is_err() {
                        return;
                    }
                }
            }
            Batch::Finish(ack) => {
                for row in engine.drain_rows() {
                    if out.send(Out::Row(row)).await.is_err() {
                        break;
                    }
                }
                let _ = ack.send(());
                return;
            }
        }
    }
}

fn rejected(line: u64, error: &RecordError) -> Out {
    Out::Rejected {
        line,
        message: error.to_string(),
    }
}

/// The single writer. Owning both streams is what keeps a row and a diagnostic from
/// interleaving mid-line.
async fn write_out(
    mut inbox: mpsc::Receiver<Out>,
    stdout: Box<dyn Write + Send>,
    mut stderr: Box<dyn Write + Send>,
) -> Result<u64, FatalError> {
    let mut rows = RowWriter::new(stdout)?;
    let mut rejected = 0u64;

    while let Some(message) = inbox.recv().await {
        match message {
            Out::Row(row) => rows.write(row)?,
            Out::Rejected { line, message } => {
                rejected += 1;
                // A failing diagnostics stream must not take the run down with it.
                let _ = writeln!(stderr, "line {line}: rejected, {message}");
            }
        }
    }

    if rejected > 0 {
        let _ = writeln!(stderr, "{rejected} record(s) rejected");
    }
    let _ = stderr.flush();
    rows.finish()?;
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_threads_are_reserved_above_the_floor() {
        assert_eq!(shards_for(8), 6);
        assert_eq!(shards_for(16), 14);
    }

    #[test]
    fn small_machines_keep_at_least_two_shards() {
        assert_eq!(shards_for(2), 2);
        assert_eq!(shards_for(3), 2);
        assert_eq!(shards_for(4), 2);
    }

    #[test]
    fn a_single_core_turns_sharding_off() {
        assert_eq!(shards_for(1), 1);
        assert_eq!(shards_for(0), 1);
    }
}
