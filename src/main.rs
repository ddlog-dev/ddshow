mod args;
mod colormap;
mod dataflow;
mod network;
mod ui;

use anyhow::{Context, Result};
use args::Args;
use colormap::{select_color, Color};
use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Table};
use dataflow::{
    operators::{make_streams, CrossbeamExtractor},
    Channel, DataflowSenders, OperatesEvent, OperatorStats, WorkerTimelineEvent,
};
use network::{wait_for_connections, wait_for_input};
use std::{
    collections::HashMap,
    env, fs,
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use structopt::StructOpt;
use timely::{
    communication::Config as ParallelConfig, dataflow::operators::capture::Extract, execute::Config,
};
use tracing_subscriber::{
    fmt::time::Uptime, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
    EnvFilter,
};
use ui::{ActivationDuration, EdgeKind};

// TODO: Library-ify a lot of this
fn main() -> Result<()> {
    let filter_layer = EnvFilter::from_env("DDSHOW_LOG");
    let fmt_layer = tracing_subscriber::fmt::layer()
        .pretty()
        .with_timer(Uptime::default())
        .with_thread_names(true)
        .with_ansi(true);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    // Grab the args from the user and build the required configs
    let args = Arc::new(Args::from_args());
    tracing::trace!("initialized and received cli args: {:?}", args);

    let config = {
        let communication = if args.workers.get() == 1 {
            ParallelConfig::Thread
        } else {
            ParallelConfig::Process(args.workers.get())
        };

        Config {
            communication,
            worker: Default::default(),
        }
    };
    tracing::trace!("created timely config");

    tracing::info!(
        "started waiting for {} timely connections on {}",
        args.timely_connections,
        args.address,
    );
    println!(
        "Waiting for {} Timely connection{} on {}...",
        args.timely_connections,
        if args.timely_connections.get() == 1 {
            ""
        } else {
            "s"
        },
        args.address,
    );
    let start_time = Instant::now();

    let timely_connections = wait_for_connections(args.address, args.timely_connections)?;
    let event_receivers = make_streams(args.workers.get(), timely_connections)?;

    let elapsed = start_time.elapsed();
    println!(
        "Connected to {} Timely trace{} in {:#?}",
        args.timely_connections,
        if args.timely_connections.get() == 1 {
            ""
        } else {
            "s"
        },
        elapsed,
    );

    let differential_receivers = if args.differential_enabled {
        tracing::info!(
            "differential dataflow logging is enabled, started waiting for {} differential connections on {}",
            args.timely_connections,
            args.differential_address,
        );

        println!(
            "Waiting for {} Differential connection{} on {}...",
            args.timely_connections,
            if args.timely_connections.get() == 1 {
                ""
            } else {
                "s"
            },
            args.differential_address,
        );
        let start_time = Instant::now();

        let differential_connections =
            wait_for_connections(args.differential_address, args.timely_connections)?;
        let event_receivers = make_streams(args.workers.get(), differential_connections)?;

        let elapsed = start_time.elapsed();
        println!(
            "Connected to {} Differential trace{} in {:#?}",
            args.timely_connections,
            if args.timely_connections.get() == 1 {
                ""
            } else {
                "s"
            },
            elapsed,
        );

        Some(event_receivers)
    } else {
        None
    };

    let (running, workers_finished) = (
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (replay_shutdown, moved_args, moved_workers_finished) =
        (running.clone(), args.clone(), workers_finished.clone());

    let (node_sender, node_receiver) = crossbeam_channel::unbounded();
    let (edge_sender, edge_receiver) = crossbeam_channel::unbounded();
    let (subgraph_sender, subgraph_receiver) = crossbeam_channel::unbounded();
    let (stats_sender, stats_receiver) = crossbeam_channel::unbounded();
    let (timeline_sender, timeline_receiver) = crossbeam_channel::unbounded();
    let (program_stats_sender, program_stats_receiver) = crossbeam_channel::unbounded();
    let (worker_stats_sender, worker_stats_receiver) = crossbeam_channel::unbounded();

    tracing::info!("starting compute dataflow");

    // Spin up the timely computation
    let worker_guards = timely::execute(config, move |worker| {
        let dataflow_name = concat!(env!("CARGO_CRATE_NAME"), " log processor");
        let args = moved_args.clone();
        tracing::info!(
            "spun up timely worker {}/{}",
            worker.index() + 1,
            args.workers,
        );

        if cfg!(debug_assertions) {
            if let Ok(addr) = env::var("DIFFERENTIAL_LOG_ADDR") {
                if let Ok(stream) = TcpStream::connect(&addr) {
                    differential_dataflow::logging::enable(worker, stream);
                    tracing::info!("connected to differential log stream at {}", addr);
                } else {
                    panic!("Could not connect to differential log address: {:?}", addr);
                }
            }
        }

        // Distribute the tcp streams across workers, converting each of them into an event reader
        let timely_traces = event_receivers[worker.index()]
            .recv()
            .expect("failed to receive timely event traces");
        let differential_traces = differential_receivers.as_ref().map(|recv| {
            recv[worker.index()]
                .recv()
                .expect("failed to receive differential event traces")
        });

        let senders = DataflowSenders::new(
            node_sender.clone(),
            edge_sender.clone(),
            subgraph_sender.clone(),
            stats_sender.clone(),
            timeline_sender.clone(),
            program_stats_sender.clone(),
            worker_stats_sender.clone(),
        );

        let dataflow_id = worker.next_dataflow_index();
        let probe = worker.dataflow_named(dataflow_name, |scope| {
            dataflow::dataflow(
                scope,
                &*args,
                timely_traces,
                differential_traces,
                replay_shutdown.clone(),
                senders,
            )
        })?;

        'work_loop: while !probe.done() {
            if !replay_shutdown.load(Ordering::Acquire) {
                tracing::info!(
                    worker_id = worker.index(),
                    dataflow_id = dataflow_id,
                    "forcibly shutting down dataflow {} on worker {}",
                    dataflow_id,
                    worker.index(),
                );

                worker.drop_dataflow(dataflow_id);
                break 'work_loop;
            }

            worker.step_or_park(Some(Duration::from_millis(500)));
        }

        let total_finished = moved_workers_finished.fetch_add(1, Ordering::Release);
        tracing::info!(
            "timely worker {}/{} finished ({}/{} workers have finished)",
            worker.index() + 1,
            args.workers,
            total_finished,
            args.workers,
        );

        Ok(())
    })
    .map_err(|err| anyhow::anyhow!("failed to start up timely computation: {}", err))?;

    // Wait for the user's prompt
    wait_for_input(&*running, &*workers_finished, worker_guards)?;

    let mut program_stats = CrossbeamExtractor::new(program_stats_receiver).extract_all();
    if let Some(stats) = program_stats.pop() {
        let mut table = Table::new();

        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            //.set_table_width(80)
            .set_header(vec!["Program Overview"])
            .add_row(vec![Cell::new("Workers"), Cell::new(stats.workers)])
            .add_row(vec![Cell::new("Dataflows"), Cell::new(stats.dataflows)])
            .add_row(vec![Cell::new("Operators"), Cell::new(stats.operators)])
            .add_row(vec![Cell::new("Subgraphs"), Cell::new(stats.subgraphs)])
            .add_row(vec![Cell::new("Channels"), Cell::new(stats.channels)])
            .add_row(vec![Cell::new("Events"), Cell::new(stats.events)])
            .add_row(vec![
                Cell::new("Total Runtime"),
                Cell::new(format!("{:#?}", stats.runtime)),
            ]);

        println!("{}", table);
    }

    let mut worker_stats = CrossbeamExtractor::new(worker_stats_receiver).extract_all();
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        //.set_table_width(80)
        .set_header(vec![
            "Worker",
            "Dataflows",
            "Operators",
            "Subgraphs",
            "Channels",
            "Events",
            "Runtime",
        ]);

    debug_assert_eq!(worker_stats.len(), 1);
    for (worker, stats) in worker_stats.remove(0) {
        table.add_row(vec![
            Cell::new(format!("Worker {}", worker.into_inner())),
            Cell::new(stats.dataflows),
            Cell::new(stats.operators),
            Cell::new(stats.subgraphs),
            Cell::new(stats.channels),
            Cell::new(stats.events),
            Cell::new(format!("{:#?}", stats.runtime)),
        ]);
    }

    println!("{}", table);

    // Extract the data from timely
    // let mut graph_nodes = SequenceTrie::new();
    // let mut operator_addresses = HashMap::new();
    // let mut operator_names = HashMap::new();
    let mut subgraph_ids = Vec::new();

    let mut node_events = CrossbeamExtractor::new(node_receiver).extract_all();
    node_events.sort_unstable_by_key(|(addr, _)| addr.clone());
    tracing::info!("finished extracting {} node events", node_events.len());

    // for (addr, event) in node_events.clone() {
    //     let id = event.id;
    //
    //     operator_names.insert(id, event.name.clone());
    //     graph_nodes.insert(&addr.addr, Graph::Node(event));
    //     operator_addresses.insert(id, addr);
    // }

    let mut subgraph_events = CrossbeamExtractor::new(subgraph_receiver).extract_all();
    subgraph_events.sort_unstable_by_key(|(addr, _)| addr.clone());
    tracing::info!(
        "finished extracting {} subgraph events",
        subgraph_events.len(),
    );

    for &((worker, ref _addr), ref event) in subgraph_events.iter() {
        let id = event.id;

        // operator_names.insert(id, event.name.clone());
        // graph_nodes.insert(&addr.addr, Graph::Subgraph(event));
        // operator_addresses.insert(id, addr);
        subgraph_ids.push((worker, id));
    }

    let (mut operator_stats, mut raw_timings) = (HashMap::new(), Vec::new());
    let mut stats_events = CrossbeamExtractor::new(stats_receiver).extract_all();
    stats_events.sort_unstable_by_key(|&(operator, _)| operator);
    tracing::info!("finished extracting {} stats events", stats_events.len());

    for (operator, stats) in stats_events.clone() {
        if !subgraph_ids.contains(&operator) {
            raw_timings.push(stats.total);
        }

        operator_stats.insert(operator, stats);
    }

    let mut edge_events = CrossbeamExtractor::new(edge_receiver).extract_all();
    edge_events.sort_unstable_by_key(|(worker, _, channel, _)| (*worker, channel.channel_id()));
    tracing::info!("finished extracting {} edge events", edge_events.len());

    let (max_time, min_time) = (
        raw_timings.iter().max().copied().unwrap_or_default(),
        raw_timings.iter().min().copied().unwrap_or_default(),
    );

    let mut timeline_events = HashMap::new();
    for (_time, data) in CrossbeamExtractor::new(timeline_receiver).extract() {
        for (event, start_time, diff) in data {
            timeline_events.entry(event).or_insert((start_time, 0)).1 += diff;
        }
    }

    let mut timeline_events: Vec<_> = timeline_events
        .into_iter()
        .filter_map(|(event, (start_time, diff))| {
            if diff >= 1 {
                Some(WorkerTimelineEvent {
                    start_time: start_time.as_nanos() as u64,
                    ..event
                })
            } else {
                None
            }
        })
        .collect();

    timeline_events.sort_unstable_by_key(|event| (event.worker, event.start_time));
    tracing::info!(
        "finished extracting {} timeline events",
        timeline_events.len(),
    );

    let html_nodes: Vec<_> = node_events
        .into_iter()
        .filter_map(|((worker, addr), OperatesEvent { id, name, .. })| {
            let &OperatorStats {
                max,
                min,
                average,
                total,
                invocations,
                ref activation_durations,
                ref arrangement_size,
                ..
            } = operator_stats.get(&(worker, id))?;

            let fill_color = select_color(&args.palette, total, (max_time, min_time));
            let text_color = fill_color.text_color();

            Some(ui::Node {
                id,
                worker,
                addr,
                name,
                max_activation_time: format!("{:#?}", max),
                min_activation_time: format!("{:#?}", min),
                average_activation_time: format!("{:#?}", average),
                total_activation_time: format!("{:#?}", total),
                invocations,
                fill_color: format!("{}", fill_color),
                text_color: format!("{}", text_color),
                activation_durations: activation_durations
                    .iter()
                    .map(|(duration, time)| ActivationDuration {
                        activation_time: duration.as_nanos() as u64,
                        activated_at: time.as_nanos() as u64,
                    })
                    .collect(),
                max_arrangement_size: arrangement_size.as_ref().map(|arr| arr.max_size),
                min_arrangement_size: arrangement_size.as_ref().map(|arr| arr.min_size),
            })
        })
        .collect();

    let html_subgraphs: Vec<_> = subgraph_events
        .into_iter()
        .filter_map(|((worker, addr), OperatesEvent { id, name, .. })| {
            let OperatorStats {
                max,
                min,
                average,
                total,
                invocations,
                ..
            } = *operator_stats.get(&(worker, id))?;

            let fill_color = select_color(&args.palette, total, (max, min));
            let text_color = fill_color.text_color();

            Some(ui::Subgraph {
                id,
                worker,
                addr,
                name,
                max_activation_time: format!("{:#?}", max),
                min_activation_time: format!("{:#?}", min),
                average_activation_time: format!("{:#?}", average),
                total_activation_time: format!("{:#?}", total),
                invocations,
                fill_color: format!("{}", fill_color),
                text_color: format!("{}", text_color),
            })
        })
        .collect();

    let html_edges: Vec<_> = edge_events
        // .clone()
        .into_iter()
        .map(
            |(
                worker,
                OperatesEvent { addr: src, .. },
                channel,
                OperatesEvent { addr: dest, .. },
            )| {
                ui::Edge {
                    src,
                    dest,
                    worker,
                    channel_id: channel.channel_id(),
                    edge_kind: match channel {
                        Channel::Normal { .. } => EdgeKind::Normal,
                        Channel::ScopeCrossing { .. } => EdgeKind::Crossing,
                    },
                }
            },
        )
        .collect();

    let mut palette_colors = Vec::with_capacity(10);
    let mut pos = 0.0;
    for _ in 0..10 {
        palette_colors.push(format!("{}", Color::new(args.palette.eval_continuous(pos))));
        pos += 0.1;
    }

    ui::render(
        &args,
        html_nodes,
        html_subgraphs,
        html_edges,
        palette_colors,
        timeline_events,
    )?;

    println!(
        "Wrote the output graph to file://{}",
        fs::canonicalize(&args.output_dir)
            .context("failed to get path of output dir")?
            .join("graph.html")
            .display(),
    );

    Ok(())
}
