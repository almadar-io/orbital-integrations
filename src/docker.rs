//! The `docker` service: the Rust twin of `DockerIntegration` (`@almadar/integrations` `integrations/docker`), an
//! in-memory container simulation (no daemon is contacted; a configured `DOCKER_HOST` changes nothing).
//! Actions: `build`, `run`, `stop`, `remove`, `logs`, `status`, `list`.

use std::sync::{Arc, Mutex};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{
    configured_from_declared, js_truthy, opt_f64, opt_str, parse_iso_ms, random_hex_id, str_param,
    unknown_action, Rand, SystemRand,
};
use crate::provider::{CallContext, Integrations, Provider};
use crate::wallclock::{iso8601, system_clock, Clock};

pub const SERVICE: &str = "docker";

/// Names every JS object answers `in` for, because `labels` is a plain `{}`.
const OBJECT_PROTOTYPE_NAMES: [&str; 12] = [
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

struct Container {
    id: String,
    name: String,
    image: String,
    status: &'static str,
    ports: Vec<Value>,
    created_at: i64,
    started_at: Option<i64>,
    stopped_at: Option<i64>,
    logs: Vec<String>,
}

pub struct DockerProvider {
    containers: Mutex<Vec<Container>>,
    clock: Clock,
    rand: Arc<dyn Rand>,
    timestamp: Regex,
}

/// Whether the credentials configure `docker` (it declares no required credential).
pub fn is_configured(creds: &Credentials) -> bool {
    configured_from_declared(creds, SERVICE)
}

pub fn register(integrations: &mut Integrations, creds: &Credentials) {
    if is_configured(creds) {
        integrations.register(SERVICE, Arc::new(DockerProvider::new()));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

impl Default for DockerProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn null_item() -> IntegrationError {
    IntegrationError::plain("Cannot read properties of null (reading 'host')")
}

fn find(containers: &[Container], id: &str) -> Option<usize> {
    containers
        .iter()
        .position(|c| c.id == id)
        .or_else(|| containers.iter().position(|c| c.id.starts_with(id)))
}

fn not_found(id: &str) -> IntegrationError {
    IntegrationError::plain(format!("Container not found: {id}"))
}

/// `array.slice(-tail)`.
fn slice_tail(lines: Vec<String>, tail: f64) -> Vec<String> {
    let len = lines.len() as f64;
    let n = if tail.is_nan() { 0.0 } else { tail.trunc() };
    let start = -n;
    let from = if start < 0.0 {
        (len + start).max(0.0)
    } else {
        start.min(len)
    };
    lines.into_iter().skip(from as usize).collect()
}

impl DockerProvider {
    pub fn new() -> Self {
        Self::with_sources(system_clock(), Arc::new(SystemRand::new()))
    }

    pub fn with_sources(clock: Clock, rand: Arc<dyn Rand>) -> Self {
        Self {
            containers: Mutex::new(Vec::new()),
            clock,
            rand,
            timestamp: Regex::new(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z").expect("timestamp pattern"),
        }
    }

    fn generate_id(&self) -> String {
        random_hex_id(&*self.rand, 16)
    }

    fn log_lines(&self, container: &Container, count: f64) -> Vec<String> {
        let mut lines = container.logs.clone();
        let base = container.started_at.unwrap_or(container.created_at);
        while (lines.len() as f64) < count {
            let at = base + lines.len() as i64 * 1000;
            lines.push(format!(
                "{} [info] Container {} \u{2014} heartbeat #{}",
                iso8601(at),
                container.name,
                lines.len() + 1
            ));
        }
        lines
    }
}

fn port_view(item: &Value, protocol_default: bool) -> Result<Value, IntegrationError> {
    if item.is_null() {
        return Err(null_item());
    }
    let mut out = Map::new();
    if let Some(host) = item.get("host") {
        out.insert("host".into(), host.clone());
    }
    if protocol_default {
        if let Some(container) = item.get("container") {
            out.insert("container".into(), container.clone());
        }
        let protocol = item
            .get("protocol")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(|| json!("tcp"));
        out.insert("protocol".into(), protocol);
    }
    Ok(Value::Object(out))
}

impl Provider for DockerProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let mut containers = self.containers.lock().expect("docker state");
        let now = (self.clock)();
        match action {
            "build" => {
                let tag = str_param(params, "tag")?;
                let image_id = format!("sha256:{}", self.generate_id());
                let size = 150_000_000 + (self.rand.unit() * 350_000_000.0).floor() as i64;
                let build_time = 2000 + (self.rand.unit() * 8000.0).floor() as i64;
                Ok(
                    json!({ "imageId": image_id, "tag": tag, "size": size, "buildTime": build_time }),
                )
            }
            "run" => {
                let image = str_param(params, "image")?;
                let empty = Vec::new();
                let raw_ports = params
                    .get("ports")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty);
                let raw_volumes = params
                    .get("volumes")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty);
                for port in raw_ports {
                    port_view(port, false)?;
                }
                for volume in raw_volumes {
                    port_view(volume, false)?;
                }
                let name = match opt_str(params, "name") {
                    Some(name) => name.to_string(),
                    None => format!("container-{}", &self.generate_id()[..12]),
                };
                let id = self.generate_id();
                let ports = raw_ports
                    .iter()
                    .map(|p| port_view(p, true))
                    .collect::<Result<Vec<_>, _>>()?;
                let stamp = iso8601(now);
                let container = Container {
                    id: id.clone(),
                    name: name.clone(),
                    image: image.to_string(),
                    status: "running",
                    ports: ports.clone(),
                    created_at: now,
                    started_at: Some(now),
                    stopped_at: None,
                    logs: vec![
                        format!("{stamp} [info] Starting container {name} from image {image}"),
                        format!("{stamp} [info] Container {name} is now running"),
                    ],
                };
                match containers.iter().position(|c| c.id == id) {
                    Some(at) => containers[at] = container,
                    None => containers.push(container),
                }
                Ok(json!({ "containerId": id, "name": name, "status": "running", "ports": ports }))
            }
            "stop" => {
                let id = str_param(params, "containerId")?;
                let at = find(&containers, id).ok_or_else(|| not_found(id))?;
                let container = &mut containers[at];
                if container.status != "running" && container.status != "paused" {
                    return Err(IntegrationError::plain(format!(
                        "Container {id} is not running (current status: {})",
                        container.status
                    )));
                }
                container.status = "exited";
                container.stopped_at = Some(now);
                let line = format!(
                    "{} [info] Container {} stopped",
                    iso8601(now),
                    container.name
                );
                container.logs.push(line);
                Ok(json!({ "containerId": container.id, "status": "exited", "stoppedAt": now }))
            }
            "remove" => {
                let id = str_param(params, "containerId")?;
                let force = js_truthy(params.get("force"));
                let at = find(&containers, id).ok_or_else(|| not_found(id))?;
                if containers[at].status == "running" && !force {
                    return Err(IntegrationError::plain(format!(
                        "Container {id} is running. Stop it first or use force=true."
                    )));
                }
                let removed = containers.remove(at);
                Ok(json!({ "containerId": removed.id, "removed": true }))
            }
            "logs" => {
                let id = str_param(params, "containerId")?;
                let tail = opt_f64(params, "tail").unwrap_or(100.0);
                let at = find(&containers, id).ok_or_else(|| not_found(id))?;
                let container = &containers[at];
                let mut lines = self.log_lines(container, tail);
                if let Some(since) = opt_str(params, "since").filter(|s| !s.is_empty()) {
                    let since_ms = parse_iso_ms(since);
                    lines.retain(|line| match self.timestamp.find(line) {
                        None => true,
                        Some(stamp) => match (parse_iso_ms(stamp.as_str()), since_ms) {
                            (Some(at), Some(since)) => at >= since,
                            _ => false,
                        },
                    });
                }
                let lines = slice_tail(lines, tail);
                Ok(json!({ "containerId": container.id, "logs": lines, "lineCount": lines.len() }))
            }
            "status" => {
                let id = str_param(params, "containerId")?;
                let at = find(&containers, id).ok_or_else(|| not_found(id))?;
                let c = &containers[at];
                Ok(json!({
                    "containerId": c.id,
                    "name": c.name,
                    "image": c.image,
                    "status": c.status,
                    "ports": c.ports,
                    "createdAt": c.created_at,
                    "startedAt": c.started_at,
                    "stoppedAt": c.stopped_at,
                }))
            }
            "list" => {
                let show_all = js_truthy(params.get("all"));
                let label = opt_str(params, "filterByLabel").filter(|s| !s.is_empty());
                let mut entries: Vec<&Container> = containers.iter().collect();
                if !show_all {
                    entries.retain(|c| c.status == "running");
                }
                if let Some(label) = label {
                    let (key, value) = match label.split_once('=') {
                        Some((key, rest)) => (key, Some(rest.split('=').next().unwrap_or(rest))),
                        None => (label, None),
                    };
                    if value.is_some() || !OBJECT_PROTOTYPE_NAMES.contains(&key) {
                        entries.clear();
                    }
                }
                entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                let listed: Vec<Value> = entries
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "name": c.name,
                            "image": c.image,
                            "status": c.status,
                            "ports": c.ports,
                            "createdAt": c.created_at,
                        })
                    })
                    .collect();
                let total = listed.len();
                Ok(json!({ "containers": listed, "total": total }))
            }
            other => Err(unknown_action("docker", other)),
        }
    }
}
