//! An optional SCM entry, using the same Core and graceful shutdown as serve.
//! Registration/account selection is an explicit operator step, never automatic.
use crate::{GatewayError, Result, cli::Lifecycle};
use std::{
    ffi::OsString,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

define_windows_service!(ffi_main, service_main);
fn failure() -> GatewayError {
    GatewayError::message(
        "SERVICE_ERROR",
        "Windows service control failed; start service run through SCM",
    )
}
pub fn dispatch() -> Result<()> {
    service_dispatcher::start("KVMHelm", ffi_main).map_err(|_| failure())
}

struct Status {
    handle: ServiceStatusHandle,
    state: ServiceState,
    checkpoint: u32,
}
impl Status {
    fn publish(&mut self, state: ServiceState, failed: bool) -> Result<()> {
        let pending = matches!(
            state,
            ServiceState::StartPending | ServiceState::StopPending
        );
        self.checkpoint = if pending {
            self.checkpoint.saturating_add(1)
        } else {
            0
        };
        self.state = state;
        self.handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: if state == ServiceState::Running {
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
                } else {
                    ServiceControlAccept::empty()
                },
                exit_code: if failed {
                    ServiceExitCode::ServiceSpecific(1)
                } else {
                    ServiceExitCode::Win32(0)
                },
                checkpoint: self.checkpoint,
                wait_hint: if pending {
                    Duration::from_secs(10)
                } else {
                    Duration::ZERO
                },
                process_id: None,
            })
            .map_err(|_| failure())
    }
}
fn service_main(arguments: Vec<OsString>) {
    if let Err(error) = run(arguments) {
        eprintln!("{error}");
    }
}
fn run(arguments: Vec<OsString>) -> Result<()> {
    // OWN_PROCESS dispatch uses the actual SCM name, allowing independent
    // explicitly installed test instances without a hard-coded account/name.
    let name = arguments.first().ok_or_else(failure)?;
    let stop = CancellationToken::new();
    let request_stop = stop.clone();
    let handle = service_control_handler::register(name, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            request_stop.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(|_| failure())?;
    let mut state = Status {
        handle,
        state: ServiceState::StartPending,
        checkpoint: 0,
    };
    state.publish(ServiceState::StartPending, false)?;
    let state = Arc::new(Mutex::new(state));
    let ready = state.clone();
    let stopping = state.clone();
    let lifecycle = Lifecycle {
        stop,
        ready: Box::new(move || ready.lock().unwrap().publish(ServiceState::Running, false)),
        stopping: Box::new(move || {
            stopping
                .lock()
                .unwrap()
                .publish(ServiceState::StopPending, false)
        }),
    };
    let result = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(async {
            // Pending states report progress while bounded store/device/plugin
            // initialization or owned-work cleanup is still in progress.
            let done = CancellationToken::new();
            let finish = done.clone();
            let progress = state.clone();
            let reporter = tokio::spawn(async move {
                loop {
                    tokio::select! {biased;_=finish.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(2))=>()};
                    let mut state = progress.lock().unwrap();
                    let current = state.state;
                    if matches!(current, ServiceState::StartPending | ServiceState::StopPending) { let _ = state.publish(current, false); }
                }
            });
            let result = crate::cli::run_service(lifecycle).await;
            done.cancel();
            let _ = reporter.await;
            result
        }),
        Err(_) => Err(failure()),
    };
    let stopped = state
        .lock()
        .unwrap()
        .publish(ServiceState::Stopped, result.is_err());
    result.and(stopped)
}
