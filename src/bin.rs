extern crate atty;
extern crate chrono;
#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate librain;
#[macro_use]
extern crate log;
extern crate nix;
extern crate num_cpus;
extern crate serde_json;
extern crate tokio_core;

pub mod start;

use std::collections::HashMap;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::io::Write;

use clap::{App, Arg, ArgMatches, SubCommand};
use nix::unistd::getpid;

use librain::{server, worker, VERSION};
use librain::errors::Result;

const DEFAULT_SERVER_PORT: u16 = 7210;
const DEFAULT_WORKER_PORT: u16 = 0;

const DEFAULT_HTTP_SERVER_PORT: u16 = 8080;

fn parse_listen_arg(key: &str, args: &ArgMatches, default_port: u16) -> SocketAddr {
    if !args.is_present(key) {
        return SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), default_port);
    }

    value_t!(args, key, SocketAddr).unwrap_or_else(|_| match value_t!(args, key, IpAddr) {
        Ok(ip) => SocketAddr::new(ip, default_port),
        _ => SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            value_t_or_exit!(args, key, u16),
        ),
    })
}

fn run_server(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let listen_address = parse_listen_arg("LISTEN_ADDRESS", cmd_args, DEFAULT_SERVER_PORT);
    let http_listen_address =
        parse_listen_arg("HTTP_LISTEN_ADDRESS", cmd_args, DEFAULT_HTTP_SERVER_PORT);
    let ready_file = cmd_args.value_of("READY_FILE");
    info!(
        "Starting Rain {} server at port {}",
        VERSION, listen_address
    );

    let log_dir = cmd_args
        .value_of("LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_logging_directory("server"));

    ensure_directory(&log_dir, "logging directory").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    let mut tokio_core = tokio_core::reactor::Core::new().unwrap();

    let debug_mode = ::std::env::var("RAIN_DEBUG_MODE")
        .map(|s| s == "1")
        .unwrap_or(false);

    if debug_mode {
        ::librain::DEBUG_CHECK_CONSISTENCY.store(true, ::std::sync::atomic::Ordering::Relaxed);
        info!("DEBUG mode enabled");
    }

    let test_mode = ::std::env::var("RAIN_TEST_MODE")
        .map(|s| s == "1")
        .unwrap_or(false);

    if test_mode {
        info!("TESTING mode enabled");
    }

    let state = server::state::StateRef::new(
        tokio_core.handle(),
        listen_address,
        http_listen_address,
        log_dir,
        test_mode,
    );
    state.start();

    // Create ready file - a file that is created when server is ready
    if let Some(name) = ready_file {
        ::librain::common::fs::create_ready_file(Path::new(name));
    }

    loop {
        tokio_core.turn(None);
        if !state.turn() {
            break;
        }
    }
}

fn default_working_directory() -> PathBuf {
    let pid = getpid();
    let hostname = ::librain::common::sys::get_hostname();
    PathBuf::from("/tmp/rain-work").join(format!("worker-{}-{}", hostname, pid))
}

fn default_logging_directory(basename: &str) -> PathBuf {
    let pid = getpid();
    let hostname = ::librain::common::sys::get_hostname();
    PathBuf::from("/tmp/rain-logs").join(format!("{}-{}-{}", basename, hostname, pid))
}

fn ensure_directory(dir: &Path, name: &str) -> Result<()> {
    if !dir.exists() {
        debug!("{} not found, creating ... {:?}", name, dir);
        if let Err(e) = std::fs::create_dir_all(dir) {
            bail!(format!(
                "{} {:?} cannot by created: {}",
                name,
                dir,
                e.description()
            ));
        }
    } else if !dir.is_dir() {
        bail!("{} {:?} exists but it is not a directory", name, dir);
    }
    Ok(())
}

fn run_worker(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let ready_file = cmd_args.value_of("READY_FILE");
    let listen_address = parse_listen_arg("LISTEN_ADDRESS", cmd_args, DEFAULT_WORKER_PORT);
    let mut server_address = cmd_args.value_of("SERVER_ADDRESS").unwrap().to_string();
    if !server_address.contains(':') {
        server_address = format!("{}:{}", server_address, DEFAULT_SERVER_PORT);
    }

    let server_addr = match server_address.to_socket_addrs() {
        Err(_) => {
            error!("Cannot resolve server address");
            exit(1);
        }
        Ok(mut addrs) => match addrs.next() {
            None => {
                error!("Cannot resolve server address");
                exit(1);
            }
            Some(ref addr) => *addr,
        },
    };

    fn detect_cpus() -> i32 {
        debug!("Detecting number of cpus");
        let cpus = num_cpus::get();
        if cpus < 1 {
            error!("Autodetection of CPUs failed. Use --cpus with a positive argument.");
            exit(1);
        }
        cpus as i32
    }

    let cpus = if cmd_args.value_of("CPUS") != Some("detect") {
        let value = value_t_or_exit!(cmd_args, "CPUS", i32);
        if value < 0 {
            let cpus = detect_cpus();
            if cpus <= -value {
                error!(
                    "{} cpus detected and {} is subtracted via --cpus. No cpus left.",
                    cpus, -value
                );
                exit(1);
            }
            detect_cpus() + value
        } else {
            value
        }
    } else {
        detect_cpus()
    };
    assert!(cpus >= 0);

    let work_dir = cmd_args
        .value_of("WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_working_directory);

    ensure_directory(&work_dir, "working directory").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    let log_dir = cmd_args
        .value_of("LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_logging_directory("worker"));

    ensure_directory(&log_dir, "logging directory").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    info!("Starting Rain {} as worker", VERSION);
    info!("Resources: {} cpus", cpus);
    info!("Working directory: {:?}", work_dir);
    info!(
        "Server address {} was resolved as {}",
        server_address, server_addr
    );

    let mut tokio_core = tokio_core::reactor::Core::new().unwrap();

    let mut subworkers = HashMap::new();
    subworkers.insert(
        "py".to_string(),
        vec![
            "python3".to_string(),
            "-m".to_string(),
            "rain.subworker".to_string(),
        ],
    );

    let state = worker::state::StateRef::new(
        tokio_core.handle(),
        work_dir,
        log_dir,
        cpus as u32,
        // Python subworker
        subworkers,
    );

    state.start(server_addr, listen_address, ready_file);

    loop {
        tokio_core.turn(None);
        state.turn();
    }
}

fn run_starter(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let listen_address = parse_listen_arg("LISTEN_ADDRESS", cmd_args, DEFAULT_SERVER_PORT);
    let http_listen_address =
        parse_listen_arg("HTTP_LISTEN_ADDRESS", cmd_args, DEFAULT_HTTP_SERVER_PORT);
    let log_dir = cmd_args
        .value_of("LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_logging_directory("worker"));

    info!("Log directory: {}", log_dir.to_str().unwrap());

    ensure_directory(&log_dir, "logging directory").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    let mut local_workers = Vec::new();

    if cmd_args.is_present("SIMPLE") && cmd_args.is_present("LOCAL_WORKERS") {
        error!("--simple and --local-workers are mutually exclusive");
        exit(1);
    }

    if cmd_args.is_present("SIMPLE") {
        local_workers.push(None);
    }

    if let Some(workers) = cmd_args.value_of("LOCAL_WORKERS") {
        local_workers = match ::serde_json::from_str(workers) {
            Ok(cpus) => {
                let cpus: Vec<u32> = cpus;
                cpus.iter().map(|x| Some(*x)).collect()
            }
            Err(_) => {
                error!("Invalid format for --local-workers");
                exit(1);
            }
        }
    }

    let run_prefix = cmd_args
        .value_of("RUN_PREFIX")
        .map(|v| v.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_else(Vec::new);

    if !run_prefix.is_empty() {
        info!("Command prefix: {:?}", run_prefix);
    }

    let mut config = start::starter::StarterConfig::new(
        local_workers,
        listen_address,
        http_listen_address,
        &log_dir,
        cmd_args.is_present("RCOS"),
        run_prefix,
    );

    config.worker_host_file = cmd_args.value_of("WORKER_HOST_FILE").map(PathBuf::from);

    // Autoconf
    match cmd_args.value_of("AUTOCONF") {
        None => Ok(()),
        Some("pbs") => config.autoconf_pbs(),
        Some(name) => {
            error!("Unknown autoconf environment '{}'", name);
            exit(1)
        }
    }.map_err(|e| {
        error!("Autoconf failed: {}", e.description());
        exit(1);
    })
        .unwrap();

    // Ignite starter
    let mut starter = start::starter::Starter::new(config);

    match starter.start() {
        Ok(()) => info!("Rain started. \u{1F327}"),
        Err(e) => {
            error!("{}", e.description());
            if starter.has_processes() {
                info!("Error occurs; clean up started processes ...");
                starter.kill_all();
            }
        }
    }
}

fn init_log() {
    // T    emporary simple logger for better module log control, default level is INFO
    // TODO: replace with Fern or log4rs later
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    if ::atty::is(::atty::Stream::Stdout) {
        ::env_logger::Builder::new()
            .format(|buf, record| {
                use env_logger::Color;
                use log::Level;

                let ts = buf.timestamp();
                let level = record.level();
                let mut level_style = buf.style();

                match level {
                    Level::Trace => level_style.set_color(Color::White),
                    Level::Debug => level_style.set_color(Color::Blue),
                    Level::Info => level_style.set_color(Color::Green),
                    Level::Warn => level_style.set_color(Color::Yellow),
                    Level::Error => level_style.set_color(Color::Red).set_bold(true),
                };
                let mut cloud_style = buf.style();
                cloud_style.set_color(Color::Blue);
                writeln!(
                    buf,
                    "{} {:>5} {} {}",
                    cloud_style.value("\u{1F327}"),
                    level_style.value(level),
                    ts,
                    record.args()
                )
            })
            .parse(&::std::env::var("RUST_LOG").unwrap_or("info".into()))
            .init();
    } else {
        ::env_logger::init();
    }
}

fn main() {
    init_log();

    // We do not use clap macro to build parser,
    // since it cannot handle "-" in name of long arguments
    let args = App::new("Rain")
        .version(VERSION)
        .about("Task-based workflow manager and executor")
        .subcommand( // ---- SERVER ----
            SubCommand::with_name("server")
                .about("Rain server")
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .long("--listen")
                    .help("Listening port/address/address:port (default 0.0.0.0:7210)")
                    .takes_value(true))
                .arg(Arg::with_name("HTTP_LISTEN_ADDRESS")
                    .long("--http-listen")
                    .value_name("ADDRESS")
                    .help("Listening HTTP port/address/address:port (default = 0.0.0.0:8080)")
                    .takes_value(true))
                .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory (default /tmp/rain-logs/server-$HOSTANE-$PID)")
                    .takes_value(true))
                .arg(Arg::with_name("READY_FILE")
                    .long("--ready-file")
                    .help("Create a file when server is initialized and ready to accept connections")
                    .takes_value(true)))
        .subcommand( // ---- WORKER ----
            SubCommand::with_name("worker")
                .about("Rain worker")
                .arg(Arg::with_name("SERVER_ADDRESS")
                    .help("Listening address: port/address/address:port (default 0.0.0.0:7210)")
                    .required(true))
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .long("--listen")
                    .value_name("ADDRESS")
                    .help("Listening port/address/address:port (default = 0.0.0.0:auto)")
                    .takes_value(true))
                .arg(Arg::with_name("CPUS")
                    .long("--cpus")
                    .help("Number of cpus or 'detect' (default = detect)")
                    .value_name("N")
                    .default_value("detect"))
                .arg(Arg::with_name("WORK_DIR")
                    .long("--workdir")
                    .help("Workding directory (default /tmp/rain-work/worker-$HOSTANE-$PID)")
                    .value_name("DIR")
                    .takes_value(true))
                .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory (default /tmp/rain-logs/worker-$HOSTANE-$PID)")
                    .takes_value(true))
                .arg(Arg::with_name("READY_FILE")
                    .long("--ready-file")
                    .value_name("DIR")
                    .help("Create a file when worker is initialized and connected to the server")
                    .takes_value(true)))
        .subcommand( // ---- START ----
            SubCommand::with_name("start")
                .about("Start server & workers at once")
                .arg(Arg::with_name("SIMPLE")
                    .long("--simple")
                    .help("Start server and one local worker"))
                .arg(Arg::with_name("LOCAL_WORKERS")
                    .long("--local-workers")
                    .help("Specify local workers (e.g. --local-workers=[4,4])")
                     .value_name("RESOURCES")
                    .takes_value(true))
                .arg(Arg::with_name("WORKER_HOST_FILE")
                     .long("--worker-host-file")
                     .help("File with hosts for workers, one each line")
                     .value_name("FILE")
                     .takes_value(true))
                .arg(Arg::with_name("AUTOCONF")
                    .long("--autoconf")
                    .help("Automatic configuration - possible values: pbs")
                    .possible_value("pbs")
                     .takes_value(true))
                .arg(Arg::with_name("RCOS") // RCOS = Reserve CPUs on Server
                     .short("-S")
                     .help("Reserve a CPU on server machine"))
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .value_name("ADDRESS")
                    .long("--listen")
                    .help("Server listening port/address/address:port (default = 0.0.0.0:auto)")
                    .takes_value(true))
                .arg(Arg::with_name("HTTP_LISTEN_ADDRESS")
                    .long("--http-listen")
                    .value_name("ADDRESS")
                    .help("Server listening HTTP port/address/address:port (default = 0.0.0.0:8080)")
                    .takes_value(true))
                .arg(Arg::with_name("RUN_PREFIX")
                    .long("--runprefix")
                    .value_name("COMMAND")
                    .help("Command used for runnig rain (e.g. --runprefix='valgrind --tool=callgrind'")
                    .takes_value(true))
                .arg(Arg::with_name("WORK_DIR")
                    .long("--workdir")
                    .help("Working directory for workers (default /tmp/rain-work/worker-$HOSTANE-$PID)")
                    .takes_value(true))
                .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory for workers & server (default /tmp/rain-logs/run-$HOSTANE-$PID)")
                    .takes_value(true)))
        .get_matches();

    match args.subcommand() {
        ("server", Some(cmd_args)) => run_server(&args, cmd_args),
        ("worker", Some(cmd_args)) => run_worker(&args, cmd_args),
        ("start", Some(cmd_args)) => run_starter(&args, cmd_args),
        _ => {
            error!("No subcommand provided.");
            ::std::process::exit(1);
        }
    }
}
