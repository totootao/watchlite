mod alerts;
mod collectors;
mod config;
mod http;
mod persist;
mod prom;
mod sampler;
mod shutdown;
mod state;
mod update;

fn main() {
    let config = config::Config::from_args();
    let state = state::Shared::new();

    // catch SIGTERM/SIGINT so the sampler can flush history before exit
    shutdown::install();

    let sampler_state = state.clone();
    let sampler_config = config.clone();
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || sampler::run(sampler_state, sampler_config))
        .expect("failed to spawn sampler thread");

    if config.once {
        print_first_snapshot(&state);
        return;
    }

    // Optional scheduled orphan-image GC: a separate thread removes images
    // that no container (running or stopped) was ever created from, on a
    // fixed interval. Safe by design — plain DELETE, no force.
    if let Some(gc) = config.image_gc_interval {
        let gc_socket = config.container_socket.clone();
        std::thread::Builder::new()
            .name("docker-gc".into())
            .spawn(move || collectors::docker::gc_loop(gc, gc_socket))
            .expect("failed to spawn docker-gc thread");
    }

    http::serve(&config, state);
}

/// --once: wait for the sampler's first real snapshot, print it, exit.
/// Logs go to stderr, so stdout is clean JSON for pipelines.
fn print_first_snapshot(state: &state::SharedState) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let json = state.json.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !json.contains("\"warming_up\"") {
            println!("{json}");
            return;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("error: timed out waiting for the first sample");
            std::process::exit(1);
        }
    }
}
