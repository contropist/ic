use std::time::Instant;

use anyhow::Error;
use async_trait::async_trait;
use candid::Principal;
use certificate_orchestrator_interface::IcCertificate;
use ic_agent::{hash_tree::HashTree, Certificate};
use opentelemetry::{
    baggage::BaggageExt,
    metrics::{Counter, Histogram, Meter},
    Context, KeyValue,
};
use tracing::info;
use trust_dns_resolver::{error::ResolveError, lookup::Lookup, proto::rr::RecordType};

use crate::{
    acme,
    certificate::{self, ExportError, GetCert, GetCertError, Package, Pair, UploadError},
    check::{Check, CheckError},
    dns::{self, Record, Resolve},
    registration::{
        Create, CreateError, Get, GetError, Id, Registration, Remove, RemoveError, Update,
        UpdateError, UpdateType,
    },
    verification::{Verify, VerifyError},
    work::{
        extract_domain, Dispense, DispenseError, Peek, PeekError, Process, ProcessError, Queue,
        QueueError, Task,
    },
};

#[derive(Clone)]
pub struct MetricParams {
    pub action: String,
    pub counter: Counter<u64>,
    pub recorder: Histogram<f64>,
}

impl MetricParams {
    pub fn new(meter: &Meter, namespace: &str, action: &str) -> Self {
        Self {
            action: action.to_string(),
            counter: meter
                .u64_counter(format!("{namespace}.{action}"))
                .with_description(format!("Counts occurrences of {action} calls"))
                .init(),
            recorder: meter
                .f64_histogram(format!("{namespace}.{action}.duration_sec"))
                .with_description(format!("Records the duration of {action} calls in sec"))
                .init(),
        }
    }
}

#[derive(Clone)]
pub struct WithMetrics<T>(pub T, pub MetricParams);

#[async_trait]
impl<T: Create> Create for WithMetrics<T> {
    async fn create(&self, name: &str, canister: &Principal) -> Result<Id, CreateError> {
        let start_time = Instant::now();

        let out = self.0.create(name, canister).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                CreateError::Duplicate(_) => "duplicate",
                CreateError::RateLimited(_) => "rate-limited",
                CreateError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Update> Update for WithMetrics<T> {
    async fn update(&self, id: &Id, typ: &UpdateType) -> Result<(), UpdateError> {
        let start_time = Instant::now();

        let out = self.0.update(id, typ).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                UpdateError::NotFound => "not-found",
                UpdateError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[
            KeyValue::new("status", status),
            KeyValue::new(
                "type",
                match typ {
                    UpdateType::Canister(_) => "update_canister".into(), // ignore canister id as it's unbounded
                    UpdateType::State(state) => state.to_string(),
                },
            ),
        ];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, typ = ?typ, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Remove> Remove for WithMetrics<T> {
    async fn remove(&self, id: &Id) -> Result<(), RemoveError> {
        let start_time = Instant::now();

        let out = self.0.remove(id).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                RemoveError::NotFound => "not-found",
                RemoveError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Get> Get for WithMetrics<T> {
    async fn get(&self, id: &Id) -> Result<Registration, GetError> {
        let start_time = Instant::now();

        let out = self.0.get(id).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                GetError::NotFound => "not-found",
                GetError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: GetCert> GetCert for WithMetrics<T> {
    async fn get_cert(&self, id: &Id) -> Result<Pair, GetCertError> {
        let start_time = Instant::now();

        let out = self.0.get_cert(id).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                GetCertError::NotFound => "not-found",
                GetCertError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Queue> Queue for WithMetrics<T> {
    async fn queue(&self, id: &Id, t: u64) -> Result<(), QueueError> {
        let start_time = Instant::now();

        let out = self.0.queue(id, t).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                QueueError::NotFound => "not-found",
                QueueError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, t, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Peek> Peek for WithMetrics<T> {
    async fn peek(&self) -> Result<Id, PeekError> {
        let start_time = Instant::now();

        let out = self.0.peek().await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                PeekError::NoTasksAvailable => "no-tasks-available",
                PeekError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Dispense> Dispense for WithMetrics<T> {
    async fn dispense(&self) -> Result<(Id, Task), DispenseError> {
        let start_time = Instant::now();

        let out = self.0.dispense().await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                DispenseError::NoTasksAvailable => "no-tasks-available",
                DispenseError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Process> Process for WithMetrics<T> {
    async fn process(&self, id: &Id, task: &Task) -> Result<(), ProcessError> {
        let start_time = Instant::now();

        let out = self.0.process(id, task).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                ProcessError::AwaitingAcmeOrderCreation => "awaiting-acme-order-creation",
                ProcessError::AwaitingDnsPropagation => "awaiting-dns-propagation",
                ProcessError::AwaitingAcmeOrderReady => "awaiting-acme-order-ready",
                ProcessError::FailedUserConfigurationCheck => "failed-user-configuration-check",
                ProcessError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let cx = Context::current();
        let bgg = cx.baggage();
        let is_renewal = bgg.get("is_renewal").unwrap().to_string();
        let is_important = bgg.get("is_important").unwrap().to_string();

        let apex_domain = match is_important.as_str() {
            "1" => extract_domain(&task.name),
            _ => "N/A",
        };

        let labels = &[
            KeyValue::new("status", status),
            KeyValue::new("task", task.action.to_string()),
            KeyValue::new("is_renewal", is_renewal.clone()),
            KeyValue::new("is_important", is_important.clone()),
            KeyValue::new("apex_domain", apex_domain.to_string()),
        ];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), id, name = task.name, task = task.action.to_string(), is_renewal, is_important, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Resolve> Resolve for WithMetrics<T> {
    async fn lookup(&self, name: &str, record_type: RecordType) -> Result<Lookup, ResolveError> {
        let start_time = Instant::now();

        let out = self.0.lookup(name, record_type).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[
            KeyValue::new("status", status),
            KeyValue::new("record_type", record_type.to_string()),
        ];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, record_type = record_type.to_string(), status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: dns::Create> dns::Create for WithMetrics<T> {
    async fn create(&self, zone: &str, name: &str, record: Record) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.create(zone, name, record).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), zone, name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: dns::Delete> dns::Delete for WithMetrics<T> {
    async fn delete(&self, zone: &str, name: &str) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.delete(zone, name).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), zone, name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: acme::Order> acme::Order for WithMetrics<T> {
    async fn order(&self, name: &str) -> Result<String, Error> {
        let start_time = Instant::now();

        let out = self.0.order(name).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: acme::Ready> acme::Ready for WithMetrics<T> {
    async fn ready(&self, name: &str) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.ready(name).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: acme::Finalize> acme::Finalize for WithMetrics<T> {
    async fn finalize(&self, name: &str) -> Result<(String, String), acme::FinalizeError> {
        let start_time = Instant::now();

        let out = self.0.finalize(name).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: certificate::Upload> certificate::Upload for WithMetrics<T> {
    async fn upload(&self, id: &Id, pair: certificate::Pair) -> Result<(), UploadError> {
        let start_time = Instant::now();

        let out = self.0.upload(id, pair).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                UploadError::NotFound => "not-found",
                UploadError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), %id, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Verify> Verify for WithMetrics<T> {
    async fn verify(
        &self,
        key: Option<String>,
        limit: u64,
        pkgs: &[Package],
        cert: &Certificate,
        tree: &HashTree<Vec<u8>>,
    ) -> Result<(), VerifyError> {
        let start_time = Instant::now();

        let out = self.0.verify(key.clone(), limit, pkgs, cert, tree).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), ?key, limit, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: certificate::Export> certificate::Export for WithMetrics<T> {
    async fn export(
        &self,
        key: Option<String>,
        limit: u64,
    ) -> Result<(Vec<certificate::Package>, IcCertificate), ExportError> {
        let start_time = Instant::now();

        let out = self.0.export(key.clone(), limit).await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), ?key, limit, status, duration, error = ?out.as_ref().err());

        out
    }
}

#[async_trait]
impl<T: Check> Check for WithMetrics<T> {
    async fn check(&self, name: &str) -> Result<Principal, CheckError> {
        let start_time = Instant::now();

        let out = self.0.check(name).await;

        let status = match &out {
            Ok(_) => "ok",
            Err(err) => match err {
                CheckError::ExistingDnsTxtChallenge { .. } => "existing-dns-txt-challenge",
                CheckError::MissingDnsCname { .. } => "missing-dns-cname",
                CheckError::MissingDnsTxtCanisterId { .. } => "missing-dns-txt-canister-id",
                CheckError::MultipleDnsTxtCanisterId { .. } => "multiple-dns-txt-canister-id",
                CheckError::InvalidDnsTxtCanisterId { .. } => "invalid-dns-txt-canister-id",
                CheckError::KnownDomainsUnavailable { .. } => "known-domains-unavailable",
                CheckError::MissingKnownDomains { .. } => "missing-known-domains",
                CheckError::UnexpectedError(_) => "fail",
            },
        };

        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        counter.add(1, labels);
        recorder.record(duration, labels);

        info!(action = action.as_str(), name, status, duration, error = ?out.as_ref().err());

        out
    }
}
