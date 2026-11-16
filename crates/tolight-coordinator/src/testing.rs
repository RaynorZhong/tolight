//! Project-only M1 harness. This is not a local conformance server or SDK.

use crate::{
    room::RoomService,
    service::{self, ServiceError},
};
use sqlx::PgPool;
use tokio::sync::oneshot;
use tolight_coordinator_store::Store;
use tolight_core::config::Config;

pub struct RunningCoordinator {
    pub room: RoomService,
    pub address: std::net::SocketAddr,
    pub origin: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), ServiceError>>>,
}
impl RunningCoordinator {
    pub async fn shutdown(mut self) -> Result<(), ServiceError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.take().expect("running test service").await?
    }
}
impl Drop for RunningCoordinator {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn start(pool: PgPool, config: Config) -> Result<RunningCoordinator, ServiceError> {
    start_with_clock(pool, config, crate::clock::system_clock()).await
}

pub async fn start_with_clock(
    pool: PgPool,
    mut config: Config,
    clock: crate::clock::Clock,
) -> Result<RunningCoordinator, ServiceError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    config.listen = address;
    config.observability.listen = "127.0.0.1:0".parse().unwrap();
    config.observability.log_dir = config
        .public
        .dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new(".local"))
        .join(".service-logs");
    config.participant_hosts = vec![address.to_string()];
    config.validate()?;
    let room = RoomService::with_clock(Store::from_pool(pool), config, clock);
    let (stop, stopped) = oneshot::channel();
    let running = room.clone();
    let task = tokio::spawn(async move {
        service::serve_on(listener, running, async {
            let _ = stopped.await;
        })
        .await
    });
    Ok(RunningCoordinator {
        room,
        address,
        origin: format!("http://{address}"),
        stop: Some(stop),
        task: Some(task),
    })
}
