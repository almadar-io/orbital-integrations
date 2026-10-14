//! The `queue` service: the Rust twin of `QueueIntegration` (`@almadar/integrations` `integrations/queue`), an
//! in-memory job queue. Actions: `enqueue`, `dequeue`, `status`, `complete`, `fail`, `cancel`, `size`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{
    configured_from_declared, number_to_radix_string, opt_f64, str_param, unknown_action, Rand,
    SystemRand,
};
use crate::json::number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "queue";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Processing,
    Completed,
    Failed,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

struct Job {
    id: String,
    queue: String,
    payload: Value,
    priority: f64,
    status: Status,
    enqueued_at: i64,
    delay: f64,
}

impl Job {
    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "payload": self.payload,
            "enqueuedAt": self.enqueued_at,
            "priority": number(self.priority),
        })
    }
}

#[derive(Default)]
struct State {
    queues: HashMap<String, Vec<String>>,
    jobs: HashMap<String, Job>,
}

pub struct QueueProvider {
    state: Mutex<State>,
    clock: Clock,
    rand: Arc<dyn Rand>,
}

/// Whether the credentials configure `queue` (`QUEUE_URL` is a required declared credential).
pub fn is_configured(creds: &Credentials) -> bool {
    configured_from_declared(creds, SERVICE)
}

pub fn register(integrations: &mut Integrations, creds: &Credentials) {
    if is_configured(creds) {
        integrations.register(SERVICE, Arc::new(QueueProvider::new()));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

impl Default for QueueProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueProvider {
    pub fn new() -> Self {
        Self::with_sources(system_clock(), Arc::new(SystemRand::new()))
    }

    pub fn with_sources(clock: Clock, rand: Arc<dyn Rand>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
            rand,
        }
    }

    fn job_id(&self, now: i64) -> String {
        let suffix: String = number_to_radix_string(self.rand.unit(), 36)
            .chars()
            .skip(2)
            .take(6)
            .collect();
        format!("job_{now}_{suffix}")
    }
}

impl Provider for QueueProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let mut state = self.state.lock().expect("queue state");
        let state = &mut *state;
        let now = (self.clock)();
        match action {
            "enqueue" => {
                let queue = str_param(params, "queue")?;
                let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                let delay = opt_f64(params, "delay").unwrap_or(0.0);
                let priority = opt_f64(params, "priority").unwrap_or(0.0);
                let id = self.job_id(now);
                state.jobs.insert(
                    id.clone(),
                    Job {
                        id: id.clone(),
                        queue: queue.to_string(),
                        payload,
                        priority,
                        status: Status::Pending,
                        enqueued_at: now,
                        delay,
                    },
                );
                let line = state.queues.entry(queue.to_string()).or_default();
                let at = line
                    .iter()
                    .position(|existing| {
                        state
                            .jobs
                            .get(existing)
                            .is_some_and(|job| job.priority < priority)
                    })
                    .unwrap_or(line.len());
                line.insert(at, id.clone());
                Ok(json!({ "jobId": id, "position": at }))
            }
            "dequeue" => {
                let queue = str_param(params, "queue")?;
                let line = state.queues.entry(queue.to_string()).or_default();
                for index in 0..line.len() {
                    let Some(job) = state.jobs.get_mut(&line[index]) else {
                        continue;
                    };
                    if job.status != Status::Pending {
                        continue;
                    }
                    if (now as f64) < job.enqueued_at as f64 + job.delay {
                        continue;
                    }
                    job.status = Status::Processing;
                    let view = job.view();
                    line.remove(index);
                    return Ok(json!({ "job": view }));
                }
                Ok(json!({ "job": null }))
            }
            "status" => {
                let id = str_param(params, "jobId")?;
                Ok(match state.jobs.get(id) {
                    None => json!({ "status": "pending", "job": null }),
                    Some(job) => json!({ "status": job.status.name(), "job": job.view() }),
                })
            }
            "complete" => {
                let id = str_param(params, "jobId")?;
                match state.jobs.get_mut(id) {
                    Some(job) if job.status == Status::Processing => {
                        job.status = Status::Completed;
                        Ok(json!({ "completed": true }))
                    }
                    _ => Ok(json!({ "completed": false })),
                }
            }
            "fail" => {
                let id = str_param(params, "jobId")?;
                match state.jobs.get_mut(id) {
                    Some(job) if job.status == Status::Processing => {
                        job.status = Status::Failed;
                        Ok(json!({ "failed": true }))
                    }
                    _ => Ok(json!({ "failed": false })),
                }
            }
            "cancel" => {
                let id = str_param(params, "jobId")?;
                let Some(job) = state.jobs.get(id) else {
                    return Ok(json!({ "cancelled": false }));
                };
                if job.status != Status::Pending {
                    return Ok(json!({ "cancelled": false }));
                }
                if let Some(line) = state.queues.get_mut(&job.queue) {
                    if let Some(at) = line.iter().position(|queued| queued == id) {
                        line.remove(at);
                    }
                }
                state.jobs.remove(id);
                Ok(json!({ "cancelled": true }))
            }
            "size" => {
                let queue = str_param(params, "queue")?;
                let (mut pending, mut processing) = (0, 0);
                for job in state.jobs.values().filter(|job| job.queue == queue) {
                    match job.status {
                        Status::Pending => pending += 1,
                        Status::Processing => processing += 1,
                        _ => {}
                    }
                }
                Ok(
                    json!({ "size": pending + processing, "pending": pending, "processing": processing }),
                )
            }
            other => Err(unknown_action("queue", other)),
        }
    }
}
