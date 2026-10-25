fn main() {
    #[cfg(windows)]
    let service = std::env::args().skip(1).take(2).eq(["service", "run"]);
    #[cfg(not(windows))]
    let service = false;
    let result = if service {
        #[cfg(windows)]
        {
            kvmhelm::service::dispatch()
        }
        #[cfg(not(windows))]
        {
            unreachable!()
        }
    } else {
        match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(kvmhelm::cli::run()),
            Err(_) => Err(kvmhelm::GatewayError::new("INTERNAL_ERROR")),
        }
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
