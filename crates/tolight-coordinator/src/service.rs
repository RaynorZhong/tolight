//! Supervised runtime: one DB lease, isolated operational listener and publication.

use crate::{admin, http, metrics, publisher, room::RoomService};
use std::{
    error::Error,
    future::{Future, IntoFuture},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    time::{Instant, MissedTickBehavior},
};
use tolight_coordinator_store::Store;
use tolight_core::config::Config;

pub type ServiceError = Box<dyn Error + Send + Sync>;

pub async fn serve<F>(store: Store, mut config: Config, shutdown: F) -> Result<(), ServiceError>
where
    F: Future<Output = ()> + Send + 'static,
{
    config.validate()?;
    if !config.listen.ip().is_loopback() {
        return Err("Coordinator 后端监听必须为 loopback；公网由 Caddy 提供 HTTPS".into());
    }
    let listener = TcpListener::bind(config.listen).await?;
    serve_on(listener, RoomService::new(store, config), shutdown).await
}

pub async fn serve_on<F>(
    listener: TcpListener,
    room: RoomService,
    shutdown: F,
) -> Result<(), ServiceError>
where
    F: Future<Output = ()> + Send + 'static,
{
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("Coordinator 后端监听必须为 loopback；公网由 Caddy 提供 HTTPS".into());
    }
    room.store().check_schema().await?;
    if !room.config().observability.listen.ip().is_loopback() {
        return Err("健康检查只允许 loopback".into());
    }
    room.process_metrics().logger.initialize()?;

    let mut lease = room.store().acquire_serve_lock().await?;
    publisher::validate_manifest(room.store(), room.config()).await?;
    // Capture before the publication snapshot. Reading it afterwards could miss
    // an Operator commit made between that snapshot and the first polling tick.
    let observability = TcpListener::bind(room.config().observability.listen).await?;
    room.process_metrics()
        .set_address(observability.local_addr()?);
    room.process_metrics()
        .logger
        .event("serve_started", None, None)?;
    room.process_metrics()
        .logger
        .participant_hosts(&room.config().participant_hosts)?;
    eprintln!(
        "生效的 participant_hosts：{}",
        room.config().participant_hosts.join(", ")
    );
    let mut previous = room.store().room_state().await?;
    let initial = publisher::publish(room.store(), room.config(), false).await;
    let mut pending = initial.is_err();
    if !pending {
        room.record_publication();
    }
    if pending {
        room.process_metrics()
            .logger
            .event("initial_publish_retry", None, None)?;
    }
    let mut publish_at = Instant::now();
    let mut publishing: Option<tokio::task::JoinHandle<Result<(), publisher::PublishError>>> = None;
    let mut poll = tokio::time::interval(Duration::from_secs(room.config().room_poll_interval));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (stop_sender, stop_receiver) = oneshot::channel();
    let (observe_stop, observe_stopped) = oneshot::channel();
    let ops_address = observability.local_addr()?;
    let ops_router = metrics::router(room.clone()).merge(admin::router(room.clone(), ops_address));
    let observe_server = axum::serve(
        observability,
        ops_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = observe_stopped.await;
    })
    .into_future();
    tokio::pin!(observe_server);
    let mut observe_finished = false;

    let server = axum::serve(
        listener,
        http::router(room.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = stop_receiver.await;
    })
    .into_future();
    tokio::pin!(server);
    tokio::pin!(shutdown);
    let mut network_finished = false;
    let result = loop {
        tokio::select! {
            result=&mut server=>{network_finished=true;break result.map_err(Into::into);},
            result=&mut observe_server=>{observe_finished=true;break result.map_err(Into::into);},
            _=&mut shutdown=>break Ok(()),
            result=async {publishing.as_mut().expect("guarded publishing task").await},if publishing.is_some()=>{
                publishing=None;
                if matches!(result,Ok(Ok(()))) {room.record_publication();}
                else {pending=true;publish_at=Instant::now();let _=room.process_metrics().logger.event("publish_retry",None,None);}
            },
            _=poll.tick()=>{
                if let Err(error)=lease.check().await {break Err(Box::new(error) as ServiceError);}
                match room.store().room_state().await {
                    Err(error)=>break Err(Box::new(error) as ServiceError),
                    Ok(state)=>{
                        metrics::observe_lag(&room,state.last_seq);
                        if state.updated_at!=previous.updated_at || state.last_seq!=previous.last_seq || state.paused!=previous.paused {
                            room.notify_change();
                            if !pending {publish_at=Instant::now()+Duration::from_secs(room.config().publish_coalesce);}
                            pending=true;
                            previous=state;
                        }
                    }
                }
                if pending && publishing.is_none() && Instant::now()>=publish_at {
                    pending=false;
                    let store=room.store().clone();let config=room.config().clone();
                    publishing=Some(tokio::spawn(async move {publisher::publish(&store,&config,false).await}));
                }
            }
        }
    };
    let _ = stop_sender.send(());
    let _ = observe_stop.send(());
    room.shutdown();
    if !network_finished {
        // Wake waiting reads before graceful listener shutdown. No pending DB lock is
        // retained, and a failed DB lease cannot leave an orphan serving process.
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
    }
    // Never detach a publisher on shutdown. Its blocking writer owns the OS
    // descriptor until rename/fsync and cleanup have actually finished.
    if let Some(publishing) = publishing {
        let _ = publishing.await;
    }
    if !observe_finished {
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut observe_server).await;
    }
    let _ = room
        .process_metrics()
        .logger
        .event("serve_stopped", None, None);
    drop(lease);
    result
}
