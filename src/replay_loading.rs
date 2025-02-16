use crate::{
    args::{Args, StreamEncoding},
    dataflow::{
        constants::{IDLE_EXTRACTION_FUEL, TCP_READ_TIMEOUT},
        operators::{EventReader, Fuel, RkyvEventReader},
        utils::{self, DifferentialLogBundle, ProgressLogBundle, TimelyLogBundle},
        DataflowData, DataflowReceivers,
    },
};
use abomonation::Abomonation;
use anyhow::{Context, Result};
use bytecheck::CheckBytes;
use crossbeam_channel::Receiver;
use ddshow_sink::{DIFFERENTIAL_ARRANGEMENT_LOG_FILE, TIMELY_LOG_FILE, TIMELY_PROGRESS_LOG_FILE};
use ddshow_types::progress_logging::TimelyProgressEvent;
use differential_dataflow::logging::DifferentialEvent as RawDifferentialEvent;
use indicatif::{ProgressBar, ProgressStyle};
use rkyv::{
    de::deserializers::AllocDeserializer, validation::DefaultArchiveValidator, Archive, Deserialize,
};
use std::{
    ffi::OsStr,
    fmt::Debug,
    fs::{self, File},
    hint,
    io::{self, BufReader, Read, Write},
    iter,
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroUsize,
    path::Path,
    sync::{
        atomic::{self, AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier,
    },
    thread,
    time::Duration,
};
use timely::{
    communication::WorkerGuards, dataflow::operators::capture::Event,
    logging::TimelyEvent as RawTimelyEvent,
};

type AcquiredStreams<T, D1, D2> = EventReceivers<
    RkyvEventReader<T, D1, Box<dyn Read + Send + 'static>>,
    EventReader<T, D2, TcpStream>,
>;

pub(crate) type TimelyEventReceivers = Arc<[Receiver<TimelyReplaySource>]>;
pub(crate) type TimelyReplaySource = ReplaySource<
    RkyvEventReader<Duration, TimelyLogBundle, Box<dyn Read + Send + 'static>>,
    EventReader<Duration, (Duration, usize, RawTimelyEvent), TcpStream>,
>;

pub(crate) type DifferentialEventReceivers = Option<Arc<[Receiver<DifferentialReplaySource>]>>;
pub(crate) type DifferentialReplaySource = ReplaySource<
    RkyvEventReader<Duration, DifferentialLogBundle, Box<dyn Read + Send + 'static>>,
    EventReader<Duration, (Duration, usize, RawDifferentialEvent), TcpStream>,
>;

pub(crate) type ProgressEventReceivers = Option<Arc<[Receiver<ProgressReplaySource>]>>;
pub(crate) type ProgressReplaySource = ReplaySource<
    RkyvEventReader<Duration, ProgressLogBundle, Box<dyn Read + Send + 'static>>,
    EventReader<Duration, (Duration, usize, TimelyProgressEvent), TcpStream>,
>;

#[derive(Debug)]
pub enum ReplaySource<R, A> {
    Rkyv(Vec<R>),
    Abomonation(Vec<A>),
}

impl<R, A> ReplaySource<R, A> {
    pub fn len(&self) -> usize {
        match self {
            ReplaySource::Rkyv(rkyv) => rkyv.len(),
            ReplaySource::Abomonation(abomonation) => abomonation.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            ReplaySource::Rkyv(_) => "Rkyv",
            ReplaySource::Abomonation(_) => "Abomonation",
        }
    }

    /// Returns `true` if the replay_source is `Abomonation`.
    pub const fn is_abomonation(&self) -> bool {
        matches!(self, Self::Abomonation(..))
    }
}

#[tracing::instrument(skip(args))]
pub fn connect_to_sources(
    args: &Args,
) -> Result<
    Option<(
        TimelyEventReceivers,
        DifferentialEventReceivers,
        ProgressEventReceivers,
        usize,
    )>,
> {
    let mut total_sources = 0;

    let timely_listener = if !args.is_file_sourced() {
        Some(TcpListener::bind(args.timely_address).with_context(|| {
            anyhow::anyhow!("failed to bind to timely socket {}", args.timely_address)
        })?)
    } else {
        None
    };
    let differential_listener = if args.differential_enabled && !args.is_file_sourced() {
        Some(
            TcpListener::bind(args.differential_address).with_context(|| {
                anyhow::anyhow!(
                    "failed to bind to differential socket {}",
                    args.differential_address,
                )
            })?,
        )
    } else {
        None
    };
    let progress_listener = if args.progress_enabled && !args.is_file_sourced() {
        Some(TcpListener::bind(args.progress_address).with_context(|| {
            anyhow::anyhow!(
                "failed to bind to progress socket {}",
                args.progress_address,
            )
        })?)
    } else {
        None
    };

    // Connect to the timely sources
    let (timely_event_receivers, are_timely_sources, num_sources) = acquire_replay_sources(
        &args,
        args.timely_address,
        timely_listener,
        args.timely_connections,
        args.workers,
        args.replay_logs.as_deref(),
        TIMELY_LOG_FILE,
        "Timely",
    )?;
    total_sources += num_sources;

    // Connect to the differential sources
    let (differential_event_receivers, are_differential_sources) = if args.differential_enabled {
        let (receivers, are_sources, num_sources) = acquire_replay_sources(
            &args,
            args.differential_address,
            differential_listener,
            args.timely_connections,
            args.workers,
            args.replay_logs.as_deref(),
            DIFFERENTIAL_ARRANGEMENT_LOG_FILE,
            "Differential",
        )?;
        total_sources += num_sources;

        (Some(receivers), are_sources)
    } else {
        (None, true)
    };

    // Connect to progress sources
    let (progress_event_receivers, are_progress_sources) = if args.progress_enabled {
        let (receivers, are_sources, num_sources) = acquire_replay_sources(
            &args,
            args.progress_address,
            progress_listener,
            args.timely_connections,
            args.workers,
            args.replay_logs.as_deref(),
            TIMELY_PROGRESS_LOG_FILE,
            "Progress",
        )?;
        total_sources += num_sources;

        (Some(receivers), are_sources)
    } else {
        (None, true)
    };

    // If no replay sources were provided, exit early
    if !are_timely_sources || !are_differential_sources || !are_progress_sources {
        tracing::warn!(
            are_timely_sources = are_timely_sources,
            are_differential_sources = are_differential_sources,
            differential_enabled = args.differential_enabled,
            are_progress_sources = are_progress_sources,
            progress_enabled = args.progress_enabled,
            total_sources = total_sources,
            "no replay sources were provided",
        );
    }

    Ok(Some((
        timely_event_receivers,
        differential_event_receivers,
        progress_event_receivers,
        total_sources,
    )))
}

/// Connect to and prepare the replay sources
#[tracing::instrument(skip(args))]
#[allow(clippy::too_many_arguments)]
pub fn acquire_replay_sources<T, D1, D2>(
    args: &Args,
    address: SocketAddr,
    listener: Option<TcpListener>,
    connections: NonZeroUsize,
    workers: NonZeroUsize,
    log_dir: Option<&Path>,
    file_prefix: &str,
    target: &str,
) -> Result<(AcquiredStreams<T, D1, D2>, bool, usize)>
where
    Event<T, D2>: Clone,
    D2: Abomonation + Send + 'static,
    T: Archive + Abomonation + Send + 'static,
    T::Archived: Deserialize<T, AllocDeserializer> + CheckBytes<DefaultArchiveValidator>,
    D1: Archive + Send + 'static,
    D1::Archived: Deserialize<D1, AllocDeserializer> + CheckBytes<DefaultArchiveValidator>,
{
    let mut num_sources = 0;

    let plural = if connections.get() == 1 { "" } else { "s" };
    let prefix = if let Some(log_dir) = log_dir {
        format!("Loading {} replay from {}", target, log_dir.display(),)
    } else {
        tracing::info!(
            "started waiting for {} {} connections on {}",
            connections,
            target,
            address,
        );

        format!(
            "Waiting for {} connection{} on {}",
            connections, plural, address,
        )
    };

    let progress_style = ProgressStyle::default_spinner()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
        .template("[{elapsed}] {spinner} {prefix}: {wide_msg}");
    // Duplicate the style with the spinner removed so that it
    // looks nicer after the task finishes
    let finished_style =
        ProgressStyle::default_spinner().template("[{elapsed}] {prefix}: {wide_msg}");

    let progress = ProgressBar::new(connections.get() as u64)
        .with_style(progress_style)
        .with_prefix(prefix);

    utils::set_steady_tick(&progress, connections.get());

    let replay_sources = if let Some(log_dir) = log_dir {
        let mut replays = Vec::with_capacity(connections.get());

        // Load all files in the directory that have the `.ddshow` extension and a
        // prefix that matches `file_prefix`
        // TODO: Probably want some sort of method to allow distinguishing between
        //       different runs saved to the same folder
        // TODO: Add support for decompressing archived log files
        let dir = fs::read_dir(log_dir).context("failed to read log directory")?;
        for entry in dir.into_iter().filter_map(|entry| {
            entry.map_or_else(
                |err| {
                    tracing::error!(dir = ?log_dir, "failed to open file: {:?}", err);
                    eprintln!("failed to open file: {:?}", err);

                    None
                },
                Some,
            )
        }) {
            let replay_file = entry.path();

            let is_file = entry.file_type().map_or(false, |file| file.is_file());
            let ends_with_ddshow = replay_file.extension() == Some(OsStr::new("ddshow"));
            let starts_with_prefix = replay_file
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|file| file.split('.').next())
                .map_or(false, |prefix| prefix == file_prefix);

            if is_file && ends_with_ddshow && starts_with_prefix {
                progress.set_message(replay_file.display().to_string());
                progress.inc_length(1);

                tracing::debug!("loading {} replay from {}", target, replay_file.display());
                let timely_file = File::open(&replay_file).with_context(|| {
                    format!("failed to open {} log file within replay directory", target)
                })?;

                replays.push(RkyvEventReader::new(
                    Box::new(BufReader::new(timely_file)) as Box<dyn Read + Send + 'static>
                ));

                progress.inc(1);
                num_sources += 1;
            } else {
                tracing::warn!(
                    dir = ?log_dir,
                    file = ?replay_file,
                    is_file = is_file,
                    ends_with_ddshow = ends_with_ddshow,
                    starts_with_prefix = starts_with_prefix,
                    "the file {} didn't match replay file criteria",
                    replay_file.display(),
                );

                let reason = if is_file {
                    "replay files must be files".to_owned()
                } else if ends_with_ddshow {
                    "did not end with the `.ddshow` extension".to_owned()
                } else if starts_with_prefix {
                    format!("did not start with the prefix {}", file_prefix)
                } else {
                    "unknown error".to_owned()
                };

                progress.set_message(format!(
                    "skipped {}, reason: {}",
                    replay_file.display(),
                    reason,
                ));
            }
        }

        progress.set_style(finished_style);
        progress.finish_with_message(format!(
            "loaded {} replay file{}",
            progress.length(),
            if progress.length() == 1 { "" } else { "s" },
        ));

        ReplaySource::Rkyv(replays)
    } else {
        let listener = listener.expect("a listener must be supplied for stream sources");

        let source = match args.stream_encoding {
            StreamEncoding::Abomonation => {
                wait_for_abominated_connections(listener, &address, connections, &progress)?
            }
            StreamEncoding::Rkyv => {
                wait_for_rkyv_connections(listener, &address, connections, &progress)?
            }
        };

        num_sources += connections.get();

        progress.set_style(finished_style);
        progress.finish_with_message(format!(
            "connected to {} trace source{}",
            connections, plural,
        ));

        source
    };

    let are_replay_sources = !replay_sources.is_empty();
    let event_receivers = make_streams(workers.get(), replay_sources)?;

    Ok((event_receivers, are_replay_sources, num_sources))
}

pub type EventReceivers<R, A> = Arc<[Receiver<ReplaySource<R, A>>]>;

#[tracing::instrument(
    skip(sources),
    fields(
        num_sources = sources.len(),
        source_kind = sources.kind(),
    ),
)]
pub fn make_streams<R, A>(
    num_workers: usize,
    sources: ReplaySource<R, A>,
) -> Result<EventReceivers<R, A>> {
    let readers: Vec<_> = match sources {
        ReplaySource::Rkyv(rkyv) => {
            let mut readers = Vec::with_capacity(num_workers);
            readers.extend(iter::repeat_with(Vec::new).take(num_workers));

            tracing::info!(
                "received {} rkyv sources to distribute among {} workers (average of {} per worker)",
                rkyv.len(),
                num_workers,
                rkyv.len() / num_workers,
            );

            for (idx, source) in rkyv.into_iter().enumerate() {
                readers[idx % num_workers].push(source);
            }

            readers.into_iter().map(ReplaySource::Rkyv).collect()
        }

        ReplaySource::Abomonation(abomonation) => {
            let mut readers = Vec::with_capacity(num_workers);
            readers.extend(iter::repeat_with(Vec::new).take(num_workers));

            tracing::info!(
                "received {} abomonation sources to distribute among {} workers (average of {} per worker)",
                abomonation.len(),
                num_workers,
                abomonation.len() / num_workers,
            );

            for (idx, source) in abomonation.into_iter().enumerate() {
                readers[idx % num_workers].push(source);
            }

            readers.into_iter().map(ReplaySource::Abomonation).collect()
        }
    };

    let (senders, receivers): (Vec<_>, Vec<_>) = (0..num_workers)
        .map(|_| crossbeam_channel::bounded(1))
        .unzip();

    for (worker, (sender, bundle)) in senders.into_iter().zip(readers).enumerate() {
        tracing::debug!(
            "sending {} sources to worker {}/{}",
            bundle.len(),
            worker + 1,
            num_workers,
        );

        sender
            .send(bundle)
            .map_err(|_| anyhow::anyhow!("failed to send events to worker {}", worker + 1))?;
    }

    Ok(Arc::from(receivers))
}

/// Connect to the given address and collect `connections` streams, returning all of them
/// in non-blocking mode
#[tracing::instrument(skip(progress))]
pub fn wait_for_abominated_connections<T, D, R>(
    listener: TcpListener,
    addr: &SocketAddr,
    connections: NonZeroUsize,
    progress: &ProgressBar,
) -> Result<ReplaySource<R, EventReader<T, D, TcpStream>>>
where
    Event<T, D>: Clone,
    T: Abomonation + Send + 'static,
    D: Abomonation + Send + 'static,
{
    progress.set_message(format!(
        "connected to 0/{} socket{}",
        connections,
        if connections.get() == 1 { "" } else { "s" },
    ));
    progress.set_length(connections.get() as u64);

    let timely_conns = (0..connections.get())
        .zip(listener.incoming())
        .map(|(idx, socket)| {
            let socket = socket.context("failed to accept socket connection")?;

            socket
                .set_nonblocking(true)
                .context("failed to set socket to non-blocking mode")?;

            if let Err(err) = socket.set_read_timeout(TCP_READ_TIMEOUT) {
                tracing::error!(
                    "failed to set socket to a read timeout of {:?}: {:?}",
                    TCP_READ_TIMEOUT,
                    err,
                );
            };

            tracing::info!(
                socket = ?socket,
                "connected to socket {}/{}",
                idx + 1,
                connections,
            );

            progress.set_message(format!(
                "connected to {}/{} socket{}",
                idx + 1,
                connections,
                if connections.get() == 1 { "" } else { "s" },
            ));
            progress.inc(1);

            Ok(EventReader::new(socket))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(ReplaySource::Abomonation(timely_conns))
}

type ConnectedRkyvSource<T, D, A> =
    ReplaySource<RkyvEventReader<T, D, Box<dyn Read + Send + 'static>>, A>;

/// Connect to the given address and collect `connections` streams, returning all of them
/// in non-blocking mode
#[tracing::instrument(skip(progress))]
pub fn wait_for_rkyv_connections<T, D, A>(
    listener: TcpListener,
    addr: &SocketAddr,
    connections: NonZeroUsize,
    progress: &ProgressBar,
) -> Result<ConnectedRkyvSource<T, D, A>>
where
    T: Archive,
    T::Archived: Deserialize<T, AllocDeserializer> + CheckBytes<DefaultArchiveValidator>,
    D: Archive,
    D::Archived: Deserialize<D, AllocDeserializer> + CheckBytes<DefaultArchiveValidator>,
{
    progress.set_message(format!(
        "connected to 0/{} socket{}",
        connections,
        if connections.get() == 1 { "" } else { "s" },
    ));
    progress.set_length(connections.get() as u64);

    let timely_conns = (0..connections.get())
        .zip(listener.incoming())
        .map(|(idx, socket)| {
            let socket = socket.context("failed to accept socket connection")?;

            socket
                .set_nonblocking(true)
                .context("failed to set socket to non-blocking mode")?;

            if let Err(err) = socket.set_read_timeout(TCP_READ_TIMEOUT) {
                tracing::error!(
                    "failed to set socket to a read timeout of {:?}: {:?}",
                    TCP_READ_TIMEOUT,
                    err,
                );
            };

            tracing::info!(
                socket = ?socket,
                "connected to socket {}/{}",
                idx + 1,
                connections,
            );

            progress.set_message(format!(
                "connected to {}/{} socket{}",
                idx + 1,
                connections,
                if connections.get() == 1 { "" } else { "s" },
            ));
            progress.inc(1);

            Ok(RkyvEventReader::new(
                Box::new(socket) as Box<dyn Read + Send + 'static>
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(ReplaySource::Rkyv(timely_conns))
}

/// Wait for user input to terminate the trace replay and wait for all timely
/// workers to terminate
// TODO: Add a "haven't received updates in `n` seconds" thingy to tell the user
//       we're no longer getting data
#[tracing::instrument(
    skip(args, worker_guards, receivers),
    fields(workers = worker_guards.guards().len()),
)]
pub fn wait_for_input(
    args: &Args,
    running: &AtomicBool,
    workers_finished: &AtomicUsize,
    worker_guards: WorkerGuards<Result<()>>,
    receivers: DataflowReceivers,
) -> Result<DataflowData> {
    let mut stdin = io::stdin();

    let (send, recv) = crossbeam_channel::bounded(1);
    let barrier = Arc::new(Barrier::new(2));

    let thread_barrier = barrier.clone();
    thread::spawn(move || {
        thread_barrier.wait();

        // Wait for input
        let _ = stdin.read(&mut [0]);

        tracing::debug!("stdin thread got input from stdin");
        send.send(()).unwrap();
    });

    // Write a prompt to the terminal for the user
    let message = if args.is_file_sourced() {
        "Press enter to finish loading trace data (this will cause data to not be fully processed)..."
    } else {
        "Press enter to finish collecting trace data (this will crash the source computation \
            if it's currently running and cause data to not be fully processed)..."
    };
    println!("{}", message);

    // Sync up with the user input thread
    barrier.wait();

    let (mut fuel, mut extractor) = (
        Fuel::limited(IDLE_EXTRACTION_FUEL),
        receivers.into_extractor(),
    );
    let num_threads = worker_guards.guards().len();

    loop {
        hint::spin_loop();

        // If all workers finish their computations
        if workers_finished.load(Ordering::Acquire) == num_threads {
            tracing::info!(
                num_threads = num_threads,
                workers_finished = num_threads,
                running = running.load(Ordering::Acquire),
                "main thread got shutdown signal, all workers finished",
            );

            break;
        }

        // If an error is encountered and the dataflow shut down
        if !running.load(Ordering::Acquire) {
            tracing::info!(
                num_threads = num_threads,
                workers_finished = workers_finished.load(Ordering::Acquire),
                running = false,
                "main thread got shutdown signal, `running` was set to false",
            );

            break;
        }

        // If the user shuts down the dataflow
        if recv.recv_timeout(Duration::from_millis(500)).is_ok() {
            tracing::info!(
                num_threads = num_threads,
                workers_finished = workers_finished.load(Ordering::Acquire),
                running = running.load(Ordering::Acquire),
                "main thread got shutdown signal, received input from user",
            );

            break;
        }

        // After we've checked all of our exit conditions we can pull some
        // data from out of the target dataflow
        extractor.extract_with_fuel(&mut fuel);

        tracing::debug!(
            used = ?fuel.used(),
            remaining = ?fuel.remaining(),
            "spent {} fuel within the main thread's wait loop",
            fuel.used().unwrap_or(usize::MAX),
        );

        fuel.reset();
    }

    // Terminate the replay
    running.store(false, Ordering::Release);
    atomic::fence(Ordering::Acquire);

    {
        let mut stdout = io::stdout();
        write!(stdout, "Processing data...").context("failed to write to stdout")?;
        stdout.flush().context("failed to flush stdout")?;
    }

    tracing::debug!("unparking all worker threads");
    for thread in worker_guards.guards() {
        thread.thread().unpark();
    }

    // Join all timely worker threads
    tracing::debug!("joining all worker threads");
    for result in worker_guards.join() {
        result
            .map_err(|err| anyhow::anyhow!("failed to join timely worker threads: {}", err))??;
    }

    tracing::debug!("extracting all remaining data from the dataflow");
    let data = extractor.extract_all();

    Ok(data)
}
