// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Offline replay of a C++ node's database (spec/12-roadmap.md, stage 3 step 2).
//!
//! ```text
//! wrkz-replay --db <copy of the C++ DB> --state <dir for our state> [mode] [options]
//!
//! modes (pick one; the default is the linear pass)
//!   (default)            genesis to the top, in order, checkpoints exactly as
//!                        the C++ has them. Resumable, dominated by decoding
//!                        and state writes. Hours against a 40 GB database.
//!   --windows forks      only the blocks around every height where a rule
//!                        changes, checkpoints OFF. Minutes.
//!   --sample N           N random windows, checkpoints OFF.
//!
//! options
//!   --window W           window size, default 2000 (windowed modes)
//!   --seed S             the sampling seed; printed either way, so a run can
//!                        be repeated exactly
//!   --from H --to H      restrict the linear pass
//!   --no-checkpoints-from H   linear pass only: turn the checkpoint zone off
//!                        from H so proof of work and signatures run below it
//!   --progress N         a progress line every N blocks (default 10000)
//!   --store-raw          keep block bodies in our state (needed to follow
//!                        forks; an offline replay does not)
//!   --threads N          verify a transaction's ring signatures on N threads,
//!                        default the detected parallelism (capped at 32).
//!                        `--threads 1` is the old sequential path. Ring
//!                        signature verification is where a transaction-heavy
//!                        window spends essentially all of its time, and it is
//!                        a pure function, so this changes nothing but the
//!                        clock: the same blocks are accepted and rejected and
//!                        a rejection names the same rule and input index at
//!                        every value. A transaction with fewer than four key
//!                        inputs takes the sequential path regardless.
//!   --legacy-transaction-list   below 600,000, check only that a block carries
//!                        as many transactions as it names, as the C++ does,
//!                        instead of the transactions it names. The strict
//!                        check accepts the real chain; this exists so an import
//!                        that ever meets an old block failing it can go on,
//!                        and the block should be reported.
//!
//! import throughput. Every one of these is a performance knob; none of them
//! can change what is accepted, what is rejected, or which rule a rejection
//! names.
//!   --batch-blocks N     commit the state every N blocks in one write batch
//!                        rather than one batch per block, default 1000. `1`
//!                        is the old behaviour. A block inside a batch sees the
//!                        state the blocks before it wrote: the batch keeps an
//!                        in-memory overlay that every read consults before the
//!                        engine. The resume height is written inside the same
//!                        atomic batch as the records it describes, so a crash
//!                        resumes at a batch boundary and never inside one.
//!   --batch-bytes MB     commit early when a batch reaches this many
//!                        megabytes, default 64. Blocks are not uniform; this
//!                        bounds the memory the overlay holds and, with it, how
//!                        much work a crash re-does.
//!   --sync-every N       make the state durable every N blocks, default
//!                        250000. Only matters with the write-ahead log off.
//!   --wal                keep the state's write-ahead log ON during the
//!                        import. The import profile turns it off, because the
//!                        resume height makes it redundant; this trades
//!                        throughput back for a smaller loss window.
//!   --no-read-ahead      read the source one record at a time rather than a
//!                        window at a time.
//!   --compact            compact the state database once the import is done.
//!   --db-threads N       background threads and jobs for the state database,
//!                        default the validation thread count.
//!   --source-cache MB    block cache for the source database, default 512.
//! ```
//!
//! The C++ database is opened **read-only** and never written; point this at a
//! copy, or stop `Wrkzd` first.
//!
//! After each block, our derived values are checked against the C++ records for
//! that index — block hash, cumulative difficulty, already-generated coins,
//! cumulative block size, transaction count and timestamp — and the run stops at
//! the first mismatch, naming the index, the hash and what disagreed. A rejected
//! block prints the index, the hash and the rule. Either way the exit status is
//! non-zero.
//!
//! The linear run is resumable: the applied height is part of the state, so a
//! second run continues from it and the whole chain can be done in pieces
//! overnight. A windowed run reseeds the state at each window, so it is not
//! resumable and does not need to be.
//!
//! Ctrl-C (or `SIGTERM`) stops a linear run **cleanly**: it finishes the block
//! it is on, commits the batch, makes the state durable, prints where it got to
//! and exits 0, and a later run resumes from there. Killing it outright is safe
//! too — the state is left at the last committed batch — it just re-does that
//! batch.
//!
//! The two modes leave incompatible states behind — a windowed run seeds block
//! infos it never validated — so the state carries a mode tag and a run of one
//! mode refuses a directory written by the other. Use one `--state` directory
//! per mode.
//!
//! All the work is in `wrkz_chain::replay` and `wrkz_chain::windows`; this is
//! argument parsing and the two RocksDB opens.

use std::path::PathBuf;
use wrkz_chain::replay::{replay, replay_windows, ReplayOptions};
use wrkz_chain::windows::{fork_windows, sample_windows, seed_from_clock};
use wrkz_chain::{interrupt, ChainState, Checkpoints, Config};
use wrkz_storage::batch::BatchStore;
use wrkz_storage::reader::ChainReader;
use wrkz_storage::rocks::{DbConfig, RocksStore};

const USAGE: &str = "usage: wrkz-replay --db <cpp-db-dir> --state <state-dir> \
[--windows forks | --sample N] [--window W] [--seed S] \
[--from H] [--to H] [--no-checkpoints-from H] [--progress N] [--store-raw] [--threads N] \
[--legacy-transaction-list] \
[--batch-blocks N] [--batch-bytes MB] [--sync-every N] [--wal] [--no-read-ahead] [--compact] \
[--db-threads N] [--source-cache MB]";

fn die(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The value of a `--flag VALUE` pair. A tool whose job is to prove a consensus
/// implementation must never quietly check something other than what it was
/// asked to check, so a missing or unparsable value is fatal.
fn value<T: std::str::FromStr>(args: &[String], i: usize) -> T {
    let Some(raw) = args.get(i + 1) else { die(&format!("{}: missing value", args[i])) };
    match raw.parse() {
        Ok(v) => v,
        Err(_) => die(&format!("{}: {raw:?} is not valid", args[i])),
    }
}

/// Which slices of the chain to replay.
enum Mode {
    /// Genesis to the top, checkpoints as configured.
    Linear,
    /// The blocks around every rule change.
    Forks,
    /// `N` random windows.
    Sample(u32),
}

struct Args {
    db: PathBuf,
    state: PathBuf,
    mode: Mode,
    window: u32,
    seed: Option<u64>,
    no_checkpoints_from: Option<u64>,
    store_raw: bool,
    threads: usize,
    /// Check only the transaction count below 600,000, as the C++ does.
    legacy_transaction_list: bool,
    /// Blocks per state write batch; `1` is one batch per block.
    batch_blocks: u32,
    /// Megabytes of accumulated writes that force a batch to commit early.
    batch_bytes: u64,
    /// Keep the state's write-ahead log on during the import.
    wal: bool,
    /// Compact the state database once the import is done.
    compact: bool,
    /// Background threads and jobs for the state database.
    db_threads: Option<i32>,
    /// Block cache for the source database, in megabytes.
    source_cache_mb: u64,
    options: ReplayOptions,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let mut db = None;
    let mut state = None;
    let mut mode = Mode::Linear;
    let mut mode_named = false;
    let mut window = 2000u32;
    let mut seed = None;
    let mut no_checkpoints_from = None;
    let mut store_raw = false;
    let mut threads = wrkz_pow::parallel::default_threads();
    let mut legacy_transaction_list = false;
    let mut batch_blocks = wrkz_storage::batch::DEFAULT_BATCH_POINTS;
    let mut batch_bytes = (wrkz_storage::batch::DEFAULT_BATCH_BYTES >> 20) as u64;
    let mut wal = false;
    let mut compact = false;
    let mut db_threads: Option<i32> = None;
    let mut source_cache_mb = 512u64;
    let mut options = ReplayOptions { sync_every: 250_000, ..ReplayOptions::default() };
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--db" => {
                db = Some(PathBuf::from(argv.get(i + 1).unwrap_or_else(|| die("--db: missing value"))));
                i += 1;
            }
            "--state" => {
                state = Some(PathBuf::from(argv.get(i + 1).unwrap_or_else(|| die("--state: missing value"))));
                i += 1;
            }
            "--windows" => {
                let which = argv.get(i + 1).unwrap_or_else(|| die("--windows: missing value"));
                if which != "forks" {
                    die(&format!("--windows: {which:?} is not a window set; the only one is `forks`"));
                }
                if mode_named {
                    die("--windows and --sample are two modes; pick one");
                }
                mode = Mode::Forks;
                mode_named = true;
                i += 1;
            }
            "--sample" => {
                if mode_named {
                    die("--windows and --sample are two modes; pick one");
                }
                mode = Mode::Sample(value(&argv, i));
                mode_named = true;
                i += 1;
            }
            "--window" => {
                window = value(&argv, i);
                i += 1;
            }
            "--seed" => {
                seed = Some(value(&argv, i));
                i += 1;
            }
            "--from" => {
                options.from = Some(value(&argv, i));
                i += 1;
            }
            "--to" => {
                options.to = Some(value(&argv, i));
                i += 1;
            }
            "--no-checkpoints-from" => {
                no_checkpoints_from = Some(value(&argv, i));
                i += 1;
            }
            "--progress" => {
                options.progress = value(&argv, i);
                i += 1;
            }
            "--threads" => {
                threads = value(&argv, i);
                i += 1;
            }
            "--batch-blocks" => {
                batch_blocks = value(&argv, i);
                i += 1;
            }
            "--batch-bytes" => {
                batch_bytes = value(&argv, i);
                i += 1;
            }
            "--sync-every" => {
                options.sync_every = value(&argv, i);
                i += 1;
            }
            "--db-threads" => {
                db_threads = Some(value(&argv, i));
                i += 1;
            }
            "--source-cache" => {
                source_cache_mb = value(&argv, i);
                i += 1;
            }
            "--wal" => wal = true,
            "--compact" => compact = true,
            "--no-read-ahead" => options.read_ahead = false,
            "--store-raw" => store_raw = true,
            "--legacy-transaction-list" => legacy_transaction_list = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => die(&format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    if options.progress == 0 {
        die("--progress must be at least 1");
    }
    if window == 0 {
        die("--window must be at least 1");
    }
    if threads == 0 {
        die("--threads must be at least 1 (1 is the sequential path)");
    }
    if batch_blocks == 0 {
        die("--batch-blocks must be at least 1 (1 is one write batch per block)");
    }
    if batch_bytes == 0 {
        die("--batch-bytes must be at least 1");
    }
    if source_cache_mb == 0 {
        die("--source-cache must be at least 1");
    }
    if db_threads.is_some_and(|n| n < 1) {
        die("--db-threads must be at least 1");
    }
    if matches!(mode, Mode::Sample(0)) {
        die("--sample must be at least 1");
    }
    // Silence is worse than an error: a windowed run that quietly ignored
    // --from would look like it had honoured it.
    if !matches!(mode, Mode::Linear) {
        if options.from.is_some() || options.to.is_some() {
            die("--from and --to apply to the linear pass only; a windowed run picks its own ranges");
        }
        if no_checkpoints_from.is_some() {
            die("--no-checkpoints-from applies to the linear pass only; a windowed run always has checkpoints off");
        }
    }
    Args {
        db: db.unwrap_or_else(|| die("--db is required")),
        state: state.unwrap_or_else(|| die("--state is required")),
        mode,
        window,
        seed,
        no_checkpoints_from,
        store_raw,
        threads,
        legacy_transaction_list,
        batch_blocks,
        batch_bytes,
        wal,
        compact,
        db_threads,
        source_cache_mb,
        options,
    }
}

fn main() {
    let args = parse_args();
    match run(&args) {
        Ok(()) => println!("REPLAY OK"),
        Err(e) => {
            eprintln!("REPLAY FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    // Ctrl-C stops the loop between two blocks rather than killing the process
    // mid-batch. Installed before anything is opened, so that an interrupt
    // during the RocksDB opens still leaves the flag set for the loop to find.
    interrupt::install();
    let mut options = args.options;
    options.stop = Some(interrupt::flag());

    // The source is read-only and mostly random: the three records read per
    // block are point lookups into a 40 GB database, so what it wants is block
    // cache, not write buffer.
    let source_cfg = DbConfig::source(args.source_cache_mb);
    let source = RocksStore::open_read_only(&args.db, &source_cfg)
        .map_err(|e| format!("opening the C++ database at {}: {e}", args.db.display()))?;
    let source = ChainReader::new(source);

    let mut checkpoints = Checkpoints::mainnet();
    checkpoints.disable_from(args.no_checkpoints_from);
    if let Some(h) = args.no_checkpoints_from {
        println!(
            "checkpoints disabled from index {h}: proof of work, ring signatures and transaction \
             proof of work are verified from there"
        );
    }

    // The state database is being bulk-loaded, which wants the opposite of what
    // serving wants: big memtables, more background jobs, and no write-ahead log
    // (the resume height makes it recoverable — see `DbConfig::import`).
    let mut state_cfg = DbConfig::import(args.db_threads.unwrap_or(args.threads.min(32) as i32));
    if args.wal {
        state_cfg.disable_wal = false;
    }
    let state_store = RocksStore::open(&args.state, &state_cfg)
        .map_err(|e| format!("opening our state at {}: {e}", args.state.display()))?;
    let batch_bytes = (args.batch_bytes as usize).saturating_mul(1 << 20);
    let state_store = BatchStore::with_limits(state_store, args.batch_blocks, batch_bytes);
    println!(
        "state: {} MB write buffer x{}, {} background jobs, write-ahead log {}, \
         committing every {} blocks or {} MB",
        state_cfg.write_buffer_mb,
        state_cfg.max_write_buffer_number,
        state_cfg.background_jobs,
        if state_cfg.disable_wal { "OFF (--wal turns it on)" } else { "on" },
        args.batch_blocks,
        args.batch_bytes,
    );
    if state_cfg.disable_wal {
        println!(
            "  with the log off, a crash loses everything since the last durability sync \
             (--sync-every {}); the state is left at a whole batch either way and a re-run resumes from it",
            options.sync_every
        );
    }

    // A state imported with bodies is one a daemon will serve, and the daemon's
    // `/get_global_indexes_for_range` reads the per-block output records at any
    // height a wallet asks about — so keep them all, as the daemon itself does
    // (about 12 bytes an output). A verification-only import keeps the default.
    let unwind_history = if args.store_raw { u32::MAX } else { Config::default().unwind_history };
    let cfg = Config {
        store_raw_blocks: args.store_raw,
        validate_threads: args.threads,
        unwind_history,
        ..Config::default()
    };
    if args.threads == 1 {
        println!("ring signatures verified on 1 thread (--threads 1: the sequential path)");
    } else {
        println!(
            "ring signatures verified on up to {} threads ({} logical cores); --threads 1 for the sequential path",
            args.threads,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        );
    }
    let mut chain =
        ChainState::open_or_genesis(state_store, cfg, checkpoints).map_err(|e| format!("opening our state: {e}"))?;
    if args.legacy_transaction_list {
        chain.set_strict_transaction_list(false);
        println!("below 600,000 only the transaction count is checked (--legacy-transaction-list)");
    }

    let mut log = |line: &str| println!("{line}");
    let mut interrupted = false;
    match args.mode {
        Mode::Linear => {
            let report = replay(&source, &mut chain, &options, &mut log)?;
            println!("top index {}", report.top);
            interrupted = report.stopped;
        }
        Mode::Forks | Mode::Sample(_) => {
            // The source's top has to be known before the windows can be
            // clamped to it, and `replay_windows` re-reads it for its own
            // checks; one extra record read, once.
            let top = source.last_block_index().map_err(|e| format!("reading last_block_index: {e}"))?;
            let windows = match args.mode {
                Mode::Forks => {
                    println!("mode: rule-change windows of {} blocks, checkpoints off", args.window);
                    fork_windows(args.window, top)
                }
                Mode::Sample(n) => {
                    let seed = args.seed.unwrap_or_else(seed_from_clock);
                    println!("mode: {n} random windows of {} blocks, checkpoints off, --seed {seed}", args.window);
                    sample_windows(n, args.window, seed, top)
                }
                Mode::Linear => unreachable!(),
            };
            let report = replay_windows(&source, &mut chain, &windows, &options, &mut log)?;
            println!(
                "{} windows, {} blocks, {} transactions, {} ring signatures, \
                 at most {} key inputs in one transaction",
                report.windows.len(),
                report.blocks(),
                report.transactions(),
                report.rings(),
                report.max_tx_inputs()
            );
        }
    }

    // Every entry point above has already flushed and synced the state, so the
    // store below holds nothing and the compaction sees a complete database.
    let store = chain.into_store();
    if args.compact {
        if interrupted {
            println!("--compact skipped: the run was interrupted, so the import is not finished");
        } else {
            println!("compacting the state database; this rewrites every level and is not quick");
            let began = std::time::Instant::now();
            store.base().compact();
            println!("compacted in {:.1}s", began.elapsed().as_secs_f64());
        }
    }
    if interrupted {
        println!("interrupted; re-run the same command to continue");
    }
    Ok(())
}
