use axum::{Extension, Router};
use h3s_auth::User;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
    service::TowerToHyperService,
};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Peer(pub Option<User>);

#[derive(Clone)]
pub(crate) struct ConnectionContext {
    pub shutdown: CancellationToken,
    _permit: Arc<OwnedSemaphorePermit>,
}

/// Identity is derived from the completed rustls handshake, never from headers.
/// Bound accepted connections and TLS handshake time; shutdown drops all peers.
pub async fn serve(
    listener: TcpListener,
    mut tls: rustls::ServerConfig,
    router: Router,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    // Bound this server's TLS records. A handshake carries the whole
    // certificate chain in one flight, and over an overlay a record larger
    // than the Pod's path MTU is dropped without an ICMP that the tunnel
    // carries back, so the client never completes the handshake and this side
    // sees only a timeout. One kilobyte fits every plausible path here.
    tls.max_fragment_size = Some(1024);
    let lifecycle = CancellationToken::new();
    let _cancel_on_drop = lifecycle.clone().drop_guard();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let permits = Arc::new(Semaphore::new(256));
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let (stream,peer_address)=accepted?;
                // A rejected or abandoned connection is logged with its peer:
                // a client that is dropped here sees an empty TLS record and a
                // reset, which is indistinguishable from a broken path.
                // Wait briefly for a slot instead of dropping the socket. A
                // dropped socket is a reset in the client's TLS handshake,
                // which is indistinguishable from a broken datapath.
                let permit=match tokio::time::timeout(
                    Duration::from_secs(5),
                    permits.clone().acquire_owned(),
                ).await {
                    Ok(Ok(permit))=>permit,
                    Ok(Err(_))=>continue,
                    Err(_)=>{
                        eprintln!("h3s apiserver: connection capacity reached, refusing {peer_address}");
                        drop(stream);
                        continue;
                    }
                };
                let acceptor=acceptor.clone();let router=router.clone();
                let context=ConnectionContext {shutdown:lifecycle.clone(),_permit:Arc::new(permit)};
                tasks.spawn(async move {
                    // A Pod reaching the API crosses a bridge and a NAT before the
                    // first byte arrives, so a handshake that is merely slow must
                    // not be aborted while the client still waits: the client only
                    // sees the socket close, which it reports as a failed TLS
                    // record. The deadline is long enough for a path that stalls
                    // and recovers, and every outcome is logged with how far the
                    // connection got.
                    let started=std::time::Instant::now();
                    let stream=match tokio::time::timeout(Duration::from_secs(30),acceptor.accept(stream)).await {
                        Ok(Ok(stream))=>stream,
                        Ok(Err(error))=>{
                            eprintln!("h3s apiserver: TLS handshake with {peer_address} failed after {}ms: {error}",started.elapsed().as_millis());
                            return;
                        }
                        Err(_)=>{
                            eprintln!("h3s apiserver: TLS handshake with {peer_address} timed out after {}ms",started.elapsed().as_millis());
                            return;
                        }
                    };
                    let user=match stream.get_ref().1.peer_certificates().and_then(|c|c.first()) {
                        Some(cert)=>match User::from_verified_certificate(cert.as_ref()){Ok(user)=>Some(user),Err(_)=>return},None=>None,
                    };
                    let service=TowerToHyperService::new(router.layer(Extension(Peer(user))).layer(Extension(context)));
                    let _=auto::Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(TokioIo::new(stream),service).await;
                });
            }
        }
    }
    lifecycle.cancel();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
