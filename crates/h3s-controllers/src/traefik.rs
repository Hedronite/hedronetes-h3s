//! Traefik: in-process HTTP Ingress serving. It is an h3s controller, not an
//! upstream binary or image: every reconciliation lists Ingresses and
//! Services, then the listener proxies a matching HTTP request to its Service.
use crate::Error;
use k8s_openapi::api::{
    core::v1::Service,
    networking::v1::{Ingress, IngressBackend},
};
use kube::{
    api::{Api, ListParams},
    Client,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const TRAEFIK_CONTROLLER_ID: &str = "system:h3s:traefik";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortRef {
    Number(u16),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub host: String,
    pub path: String,
    pub namespace: String,
    pub service: String,
    pub port: PortRef,
}

#[derive(Default)]
struct Table {
    routes: Vec<Route>,
    addresses: HashMap<(String, String), String>,
    ports: HashMap<(String, String), Vec<(String, u16)>>,
}

/// Route table shared by a polling reconciler and every HTTP connection.
#[derive(Clone, Default)]
pub struct Gateway(Arc<Mutex<Table>>);

impl Gateway {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Table::default())))
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        match self.0.lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn rebuild(&self, ingresses: &[Ingress], services: &[Service]) {
        let mut table = self.table();
        table.addresses.clear();
        table.ports.clear();
        for service in services {
            let namespace = service.metadata.namespace.clone().unwrap_or_default();
            let name = service.metadata.name.clone().unwrap_or_default();
            let key = (namespace, name);
            if let Some(spec) = service.spec.as_ref() {
                if let Some(ip) = spec.cluster_ip.as_ref().filter(|ip| ip.as_str() != "None") {
                    table.addresses.insert(key.clone(), ip.clone());
                }
                table.ports.insert(
                    key,
                    spec.ports
                        .iter()
                        .flatten()
                        .filter_map(|port| {
                            u16::try_from(port.port)
                                .ok()
                                .map(|number| (port.name.clone().unwrap_or_default(), number))
                        })
                        .collect(),
                );
            }
        }
        table.routes = ingresses
            .iter()
            .flat_map(ingress_routes)
            .collect::<Vec<_>>();
    }

    pub fn select(&self, host: &str, path: &str) -> Option<Route> {
        self.table()
            .routes
            .iter()
            .filter(|route| route.host.is_empty() || route.host == host)
            .filter(|route| path.starts_with(route.path.as_str()))
            .max_by_key(|route| (usize::from(route.host == host), route.path.len()))
            .cloned()
    }

    fn endpoint(&self, route: &Route) -> Result<(String, u16), String> {
        let table = self.table();
        let key = (route.namespace.clone(), route.service.clone());
        let port = match &route.port {
            PortRef::Number(number) => *number,
            PortRef::Name(wanted) => table
                .ports
                .get(&key)
                .and_then(|ports| {
                    ports
                        .iter()
                        .find(|(name, _)| name == wanted)
                        .map(|(_, port)| *port)
                })
                .ok_or_else(|| {
                    status_line(503, "Service Unavailable", "backend port not published")
                })?,
        };
        let address = table
            .addresses
            .get(&key)
            .cloned()
            .ok_or_else(|| status_line(503, "Service Unavailable", "backend has no address"))?;
        if port == 0 {
            return Err(status_line(
                503,
                "Service Unavailable",
                "backend has no port",
            ));
        }
        Ok((address, port))
    }
}

fn backend(backend: &IngressBackend, _namespace: &str) -> Option<(String, PortRef)> {
    let service = backend.service.as_ref()?;
    let port = match service.port.as_ref() {
        Some(port) if port.number.is_some() => {
            PortRef::Number(u16::try_from(port.number.unwrap_or_default()).ok()?)
        }
        Some(port) if port.name.is_some() => PortRef::Name(port.name.clone().unwrap_or_default()),
        _ => return None,
    };
    Some((service.name.clone(), port))
}

/// Routes all valid rules from one Ingress; an optional default backend is a
/// host-less `/` route.
pub fn ingress_routes(ingress: &Ingress) -> Vec<Route> {
    let Some(spec) = ingress.spec.as_ref() else {
        return Vec::new();
    };
    let namespace = ingress.metadata.namespace.clone().unwrap_or_default();
    let mut routes = Vec::new();
    for rule in spec.rules.iter().flatten() {
        let Some(http) = rule.http.as_ref() else {
            continue;
        };
        let host = rule.host.clone().unwrap_or_default().to_ascii_lowercase();
        for path in &http.paths {
            let Some((service, port)) = backend(&path.backend, &namespace) else {
                continue;
            };
            routes.push(Route {
                host: host.clone(),
                path: path
                    .path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .unwrap_or("/")
                    .to_owned(),
                namespace: namespace.clone(),
                service,
                port,
            });
        }
    }
    if let Some(default) = spec.default_backend.as_ref() {
        if let Some((service, port)) = backend(default, &namespace) {
            routes.push(Route {
                host: String::new(),
                path: "/".to_owned(),
                namespace,
                service,
                port,
            });
        }
    }
    routes
}

fn status_line(status: u16, reason: &str, message: &str) -> String {
    let body = format!("{status} {reason}: {message}\n");
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn request_head(stream: &mut TcpStream) -> Result<(String, String, String), String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => return Err(status_line(400, "Bad Request", "empty request")),
            Ok(_) => bytes.push(byte[0]),
            Err(_) => return Err(status_line(400, "Bad Request", "unreadable request")),
        }
        if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err(status_line(
                431,
                "Request Header Fields Too Large",
                "headers too large",
            ));
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(_)) = (request.next(), request.next(), request.next())
    else {
        return Err(status_line(400, "Bad Request", "malformed request line"));
    };
    let host = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let host = host.split(':').next().unwrap_or(host.as_str()).to_owned();
    let path = target.split(['?', '#']).next().unwrap_or(target).to_owned();
    Ok((method.to_owned(), path, host))
}

/// Route one request and proxy it to its backend Service's cluster IP.
pub async fn serve(mut stream: TcpStream, gateway: Gateway) {
    let (method, path, host) = match request_head(&mut stream).await {
        Ok(head) => head,
        Err(response) => {
            let _ = stream.write_all(response.as_bytes()).await;
            return;
        }
    };
    let response = match gateway.select(&host, &path) {
        None => status_line(404, "Not Found", "no such host on this gateway"),
        Some(route) => match gateway.endpoint(&route) {
            Err(response) => response,
            Ok((ip, port)) => {
                let address = format!("{ip}:{port}");
                let exchange = async {
                    let mut backend = TcpStream::connect(&address).await?;
                    backend
                        .write_all(
                            format!(
                                "{method} {path} HTTP/1.0\r\nhost: {address}\r\nconnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await?;
                    backend.flush().await?;
                    backend.shutdown().await?;
                    tokio::io::copy(&mut backend, &mut stream).await?;
                    Ok::<(), std::io::Error>(())
                };
                match tokio::time::timeout(Duration::from_secs(30), exchange).await {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => {
                        status_line(502, "Bad Gateway", &format!("backend {address}: {error}"))
                    }
                    Err(_) => {
                        status_line(502, "Bad Gateway", &format!("backend {address} timed out"))
                    }
                }
            }
        },
    };
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn reconcile(
    gateway: &Gateway,
    ingresses: &Api<Ingress>,
    services: &Api<Service>,
) -> Result<(), Error> {
    let ingresses = ingresses.list(&ListParams::default()).await?;
    let services = services.list(&ListParams::default()).await?;
    gateway.rebuild(&ingresses.items, &services.items);
    Ok(())
}

/// Reconciles Ingresses and Services every second and accepts matching HTTP
/// requests. Cancellation drops the listener with the server process.
pub async fn run_traefik(client: Client, listener: TcpListener) -> Result<(), Error> {
    let gateway = Gateway::new();
    let ingresses = Api::<Ingress>::all(client.clone());
    let services = Api::<Service>::all(client);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                match reconcile(&gateway, &ingresses, &services).await {
                    Ok(()) => {}
                    Err(error) => eprintln!("traefik controller: {error}"),
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let gateway = gateway.clone();
                        tokio::spawn(async move { serve(stream, gateway).await });
                    }
                    Err(error) => eprintln!("traefik accept failed: {error}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn routes_prefer_exact_host_then_longest_path_then_default_backend() {
        let ingress: Ingress = from_value(json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {
                "defaultBackend": {"service": {"name": "fallback", "port": {"number": 8080}}},
                "rules": [
                    {"host": "app.example", "http": {"paths": [
                        {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "root", "port": {"number": 80}}}},
                        {"path": "/api", "pathType": "Prefix", "backend": {"service": {"name": "api", "port": {"number": 9000}}}}
                    ]}}
                ]
            }
        }))
        .unwrap();
        let routes = ingress_routes(&ingress);
        assert_eq!(routes.len(), 3);
        let gateway = Gateway::new();
        let service: Service = from_value(json!({
            "metadata": {"name": "api", "namespace": "default"},
            "spec": {"clusterIP": "10.43.0.20", "ports": [{"port": 9000}]}
        }))
        .unwrap();
        gateway.rebuild(&[ingress], &[service]);
        let route = gateway.select("app.example", "/api/v1").unwrap();
        assert_eq!(route.service, "api");
        assert_eq!(route.path, "/api");
        let route = gateway.select("elsewhere", "/").unwrap();
        assert_eq!(route.service, "fallback");
    }

    #[tokio::test]
    async fn matched_requests_reach_backend_and_unmatched_return_404() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = origin.accept().await {
                let mut bytes = Vec::new();
                let _ = stream.read_to_end(&mut bytes).await;
                let _ = stream
                    .write_all(b"HTTP/1.0 200 OK\r\ncontent-length: 4\r\n\r\nfrom")
                    .await;
            }
        });
        let ingress: Ingress = from_value(json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"rules": [{"host": "app.example", "http": {"paths": [
                {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "echo", "port": {"number": port}}}}
            ]}}]}
        })).unwrap();
        let service: Service = from_value(json!({
            "metadata": {"name": "echo", "namespace": "default"},
            "spec": {"clusterIP": "127.0.0.1", "ports": [{"port": port}]}
        }))
        .unwrap();
        let gateway = Gateway::new();
        gateway.rebuild(&[ingress], &[service]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve(stream, gateway).await;
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: app.example\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.0 200"), "{response}");
        assert!(response.ends_with("from"), "{response}");
        served.await.unwrap();
    }
}
