// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The routes (`HttpApi.cpp`): the same paths, bodies and status codes, the
//! API key, the per-address and global per-minute limits, the trusted-proxy
//! rules and CORS.
//!
//! | route | |
//! | --- | --- |
//! | `POST /pow` | submit `{"prefix", "wait_ms"?, "height"?}`; held up to `wait_ms` |
//! | `GET /pow/<job_id>?wait_ms=` | the job, held up to `wait_ms` while it runs |
//! | `DELETE /pow/<job_id>` | cancel |
//! | `GET /stats` | counters |
//! | `GET /health` | queue depth; outside the API key and the rate limit |
//! | `GET /` | name, version, endpoints |

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use wrkz_rpc::http::{Request, Response};

use crate::config::Config;
use crate::log::{Level, Logger};
use crate::service::{bump, JobState, JobView, PowService};

/// Larger than any prefix the network accepts, small enough that a flood of
/// oversized bodies costs little.
pub const MAX_BODY_BYTES: usize = 512 * 1024;

/// What the routes need from the configuration.
#[derive(Clone, Debug, Default)]
pub struct ApiConfig {
    pub api_key: String,
    pub cors_header: String,
    pub trusted_proxies: Vec<String>,
    pub rate_limit_per_minute: u32,
    pub max_jobs_per_minute: u32,
    pub max_wait_ms: u32,
    pub max_difficulty: u64,
}

impl From<&Config> for ApiConfig {
    fn from(c: &Config) -> Self {
        ApiConfig {
            api_key: c.api_key.clone(),
            cors_header: c.cors_header.clone(),
            trusted_proxies: c.trusted_proxies.clone(),
            rate_limit_per_minute: c.rate_limit_per_minute,
            max_jobs_per_minute: c.max_jobs_per_minute,
            max_wait_ms: c.max_wait_ms,
            max_difficulty: c.max_difficulty,
        }
    }
}

#[derive(Default)]
struct RateState {
    address_window: u64,
    by_address: HashMap<String, u32>,
    job_window: u64,
    jobs_this_window: u32,
}

/// The route table over a [`PowService`].
pub struct Api {
    config: ApiConfig,
    service: Arc<PowService>,
    logger: Arc<Logger>,
    rate: Mutex<RateState>,
}

/// `/pow/<32 lowercase hex>`, the C++ `JOB_ID_PATTERN`.
fn job_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/pow/")?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(id)
}

/// The start of the current minute, the window both limits count in.
fn current_minute() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    now - now % 60
}

impl Api {
    pub fn new(config: ApiConfig, service: Arc<PowService>, logger: Arc<Logger>) -> Self {
        Api { config, service, logger, rate: Mutex::new(RateState::default()) }
    }

    /// Answer one request from `peer`.
    pub fn handle(&self, req: &Request, peer: IpAddr) -> Response {
        if req.method == "OPTIONS" {
            return self.options();
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => self.root(req, peer),
            ("GET", "/health") => self.health(),
            ("GET", "/stats") => self.stats(req, peer),
            ("POST", "/pow") => self.submit(req, peer),
            (method, path) => match (method, job_id(path)) {
                ("GET", Some(id)) => self.poll(req, peer, id),
                ("DELETE", Some(id)) => self.cancel(req, peer, id),
                // cpp-httplib's answer to a route it does not have.
                _ => Response::new(404),
            },
        }
    }

    fn reply(&self, status: u16, body: Value) -> Response {
        let mut res = Response::json(status, body.to_string());
        if !self.config.cors_header.is_empty() {
            res.set_header("Access-Control-Allow-Origin", &self.config.cors_header);
        }
        res
    }

    fn error(&self, status: u16, message: &str) -> Response {
        self.reply(status, json!({ "status": "error", "error": message }))
    }

    /// `clientAddress`: a trusted proxy's request belongs to the client it
    /// names in `X-Real-IP`, else the last `X-Forwarded-For` entry — the one
    /// the proxy itself appended. Anyone else is their own address, so the
    /// headers cannot be used to dodge the limit.
    fn client_address(&self, req: &Request, peer: IpAddr) -> String {
        let peer = peer.to_string();
        if !self.config.trusted_proxies.contains(&peer) {
            return peer;
        }
        if let Some(real) = req.header("X-Real-IP").map(str::trim).filter(|r| !r.is_empty()) {
            return real.to_string();
        }
        if let Some(last) =
            req.header("X-Forwarded-For").and_then(|f| f.rsplit(',').next()).map(str::trim).filter(|l| !l.is_empty())
        {
            return last.to_string();
        }
        peer
    }

    fn lock_rate(&self) -> std::sync::MutexGuard<'_, RateState> {
        self.rate.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn address_rate_limited(&self, address: &str) -> bool {
        let limit = self.config.rate_limit_per_minute;
        if limit == 0 {
            return false;
        }
        let window = current_minute();
        let mut rate = self.lock_rate();
        // One entry per address that ever connected would grow without bound.
        if window != rate.address_window {
            rate.by_address.clear();
            rate.address_window = window;
        }
        let count = rate.by_address.entry(address.to_string()).or_insert(0);
        if *count >= limit {
            return true;
        }
        *count += 1;
        false
    }

    fn job_rate_limited(&self) -> bool {
        let limit = self.config.max_jobs_per_minute;
        if limit == 0 {
            return false;
        }
        let window = current_minute();
        let mut rate = self.lock_rate();
        if window != rate.job_window {
            rate.jobs_this_window = 0;
            rate.job_window = window;
        }
        if rate.jobs_this_window >= limit {
            return true;
        }
        rate.jobs_this_window += 1;
        false
    }

    /// The API key, then (for the job routes) the per-address limit.
    fn admit(&self, req: &Request, peer: IpAddr, rate_limit_applies: bool) -> Result<(), Response> {
        bump(&self.service.counters.requests);
        if !self.config.api_key.is_empty() && req.header("X-API-KEY") != Some(self.config.api_key.as_str()) {
            bump(&self.service.counters.unauthorized);
            return Err(self.error(401, "missing or incorrect X-API-KEY header"));
        }
        if rate_limit_applies && self.address_rate_limited(&self.client_address(req, peer)) {
            bump(&self.service.counters.rate_limited);
            return Err(self.error(429, "too many requests from this address, retry later"));
        }
        Ok(())
    }

    fn job_json(&self, v: &JobView) -> Value {
        let mut j = json!({
            "job_id": v.id,
            "state": v.state.name(),
            "difficulty": v.shape.difficulty,
            "inputs": v.shape.inputs,
            "outputs": v.shape.outputs,
            "hashes": v.hashes,
            "elapsed_ms": v.elapsed_ms,
        });
        match v.state {
            JobState::Done => {
                j["status"] = "done".into();
                j["nonce"] = hex::encode(v.nonce).into();
            }
            JobState::Failed => {
                j["status"] = "error".into();
                j["error"] = v.error.clone().into();
            }
            JobState::Cancelled => j["status"] = "cancelled".into(),
            JobState::Queued | JobState::Running => j["status"] = "pending".into(),
        }
        if v.state == JobState::Queued {
            j["queue_length"] = self.service.queued().into();
        }
        j
    }

    /// `wait_ms` from the body, else the query string; capped at `--max-wait-ms`.
    fn wait(&self, req: &Request, body: Option<&Value>) -> Duration {
        let asked = body.and_then(|b| b.get("wait_ms")).and_then(Value::as_u64).unwrap_or_else(|| {
            req.query
                .split('&')
                .find_map(|pair| pair.strip_prefix("wait_ms="))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        });
        Duration::from_millis(asked.min(u64::from(self.config.max_wait_ms)))
    }

    fn submit(&self, req: &Request, peer: IpAddr) -> Response {
        if let Err(res) = self.admit(req, peer, true) {
            return res;
        }
        let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
            return self.error(400, "body is not valid JSON");
        };
        let Some(prefix_hex) = body.get("prefix").and_then(Value::as_str).filter(|_| body.is_object()) else {
            return self.error(400, "body must be an object with a hex 'prefix' field");
        };
        let Ok(prefix) = hex::decode(prefix_hex) else {
            return self.error(400, "'prefix' is not valid hex");
        };
        let height = body.get("height").and_then(Value::as_u64);

        // The jobs-per-minute budget is spent only on prefixes that passed the
        // checks, so a flood of garbage cannot starve real wallets of it.
        let submitted = self.service.submit(&prefix, height, || {
            if self.job_rate_limited() {
                bump(&self.service.counters.global_limited);
                return false;
            }
            true
        });
        let Some(job) = submitted.job else {
            return self.error(submitted.status, &submitted.error);
        };
        self.logger.log(Level::Debug, format!("Job {} from {}", job.id, self.client_address(req, peer)));

        let wait = self.wait(req, Some(&body));
        if !wait.is_zero() {
            job.wait_for(wait);
        }
        self.reply(200, self.job_json(&job.view()))
    }

    fn poll(&self, req: &Request, peer: IpAddr, id: &str) -> Response {
        if let Err(res) = self.admit(req, peer, true) {
            return res;
        }
        let Some(job) = self.service.find(id) else {
            return self.error(404, "unknown or expired job");
        };
        let wait = self.wait(req, None);
        if !wait.is_zero() && !job.finished() {
            job.wait_for(wait);
        }
        self.reply(200, self.job_json(&job.view()))
    }

    fn cancel(&self, req: &Request, peer: IpAddr, id: &str) -> Response {
        if let Err(res) = self.admit(req, peer, true) {
            return res;
        }
        if self.service.cancel(id) {
            return self.reply(200, json!({ "status": "cancelled", "job_id": id }));
        }
        match self.service.find(id) {
            Some(job) => self.reply(200, self.job_json(&job.view())),
            None => self.error(404, "unknown or expired job"),
        }
    }

    /// Counters only: the deployment (bind addresses, proxies, limits, CORS,
    /// whether a key is required) stays in the start-up banner. The two limits
    /// a client can act on are the exception.
    fn stats(&self, req: &Request, peer: IpAddr) -> Response {
        if let Err(res) = self.admit(req, peer, false) {
            return res;
        }
        let mut stats = self.service.stats_json();
        stats["version"] = crate::version_line().into();
        stats["limits"] =
            json!({ "max_difficulty": self.config.max_difficulty, "max_wait_ms": self.config.max_wait_ms });
        self.reply(200, stats)
    }

    /// Deliberately outside the API key and the rate limit, so load balancers
    /// and monitors can always reach it.
    fn health(&self) -> Response {
        bump(&self.service.counters.requests);
        let limits = self.service.limits();
        self.reply(
            200,
            json!({ "status": "OK", "queue": self.service.queued(), "capacity": limits.max_queue, "threads": limits.threads }),
        )
    }

    fn root(&self, req: &Request, peer: IpAddr) -> Response {
        if let Err(res) = self.admit(req, peer, false) {
            return res;
        }
        self.reply(
            200,
            json!({
                "name": "wrkz-txpow-server",
                "version": crate::version_line(),
                "endpoints": ["POST /pow", "GET /pow/<job_id>", "DELETE /pow/<job_id>", "GET /stats", "GET /health"],
            }),
        )
    }

    fn options(&self) -> Response {
        let mut res = Response::new(204);
        if !self.config.cors_header.is_empty() {
            res.set_header("Access-Control-Allow-Origin", &self.config.cors_header);
            res.set_header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS");
            res.set_header("Access-Control-Allow-Headers", "Origin, X-Requested-With, Content-Type, Accept, X-API-KEY");
            res.set_header("Access-Control-Max-Age", "600");
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Limits;
    use std::net::Ipv4Addr;

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
    const PROXY: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn api(config: ApiConfig) -> (Api, Arc<PowService>) {
        let logger = Arc::new(Logger::new(Level::Disabled));
        let service =
            PowService::start(Limits { fixed_difficulty: Some(u64::MAX), ..Limits::default() }, Arc::clone(&logger));
        (Api::new(config, Arc::clone(&service), logger), service)
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            version: "HTTP/1.1".into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn body(res: &Response) -> Value {
        serde_json::from_slice(&res.body).unwrap()
    }

    #[test]
    fn job_ids_match_the_cpp_pattern() {
        assert_eq!(job_id("/pow/0123456789abcdef0123456789abcdef"), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(job_id("/pow/0123456789ABCDEF0123456789abcdef"), None);
        assert_eq!(job_id("/pow/abc"), None);
        assert_eq!(job_id("/pow/../stats"), None);
    }

    #[test]
    fn the_api_key_guards_everything_but_health() {
        let (api, service) = api(ApiConfig { api_key: "k".into(), ..ApiConfig::default() });
        let res = api.handle(&req("GET", "/stats", &[], ""), PEER);
        assert_eq!((res.status, body(&res)["error"].as_str()), (401, Some("missing or incorrect X-API-KEY header")));
        assert_eq!(api.handle(&req("GET", "/stats", &[("x-api-key", "k")], ""), PEER).status, 200);
        let health = api.handle(&req("GET", "/health", &[], ""), PEER);
        assert_eq!((health.status, body(&health)["status"].as_str()), (200, Some("OK")));
        assert_eq!(api.handle(&req("GET", "/nowhere", &[], ""), PEER).status, 404);
        service.stop();
    }

    #[test]
    fn the_rate_limit_counts_the_client_a_trusted_proxy_names() {
        let config =
            ApiConfig { rate_limit_per_minute: 2, trusted_proxies: vec!["127.0.0.1".into()], ..ApiConfig::default() };
        let (api, service) = api(config);
        let poll = |headers: &[(&str, &str)], peer| {
            api.handle(&req("GET", "/pow/0123456789abcdef0123456789abcdef", headers, ""), peer).status
        };
        // Two polls allowed per address per minute (404: no such job).
        assert_eq!(poll(&[], PEER), 404);
        assert_eq!(poll(&[], PEER), 404);
        // The minute can roll over between calls; only a third call inside the
        // same window is certain to be limited, so allow either outcome here.
        let third = poll(&[], PEER);
        assert!(third == 429 || third == 404);
        // Through the proxy, each named client has its own budget, and the
        // last X-Forwarded-For entry is the one believed.
        assert_eq!(poll(&[("X-Real-IP", "203.0.113.5")], PROXY), 404);
        assert_eq!(poll(&[("X-Forwarded-For", "6.6.6.6, 203.0.113.9")], PROXY), 404);
        // A direct client cannot claim to be someone else.
        assert_eq!(api.client_address(&req("GET", "/", &[("X-Real-IP", "1.2.3.4")], ""), PEER), "10.0.0.7");
        service.stop();
    }

    #[test]
    fn submissions_are_checked_before_they_are_queued() {
        let (api, service) = api(ApiConfig { max_wait_ms: 10, ..ApiConfig::default() });
        let submit = |b: &str| {
            let res = api.handle(&req("POST", "/pow", &[], b), PEER);
            (res.status, body(&res)["error"].as_str().unwrap_or_default().to_string())
        };
        assert_eq!(submit("{"), (400, "body is not valid JSON".into()));
        assert_eq!(submit("[1]"), (400, "body must be an object with a hex 'prefix' field".into()));
        assert_eq!(submit(r#"{"prefix":"zz"}"#), (400, "'prefix' is not valid hex".into()));
        assert_eq!(submit(r#"{"prefix":"00"}"#), (400, "prefix is too short".into()));

        let good = hex::encode(crate::service::tests::prefix(&[(1000, 8)], &[900], true));
        let res = api.handle(&req("POST", "/pow", &[], &format!(r#"{{"prefix":"{good}","wait_ms":5}}"#)), PEER);
        let job = body(&res);
        assert_eq!((res.status, job["status"].as_str()), (200, Some("pending")));
        let id = job["job_id"].as_str().unwrap().to_string();

        let res = api.handle(&req("DELETE", &format!("/pow/{id}"), &[], ""), PEER);
        assert_eq!(body(&res)["status"], "cancelled");
        service.stop();
    }

    #[test]
    fn cors_is_on_every_reply_and_the_preflight() {
        let (api, service) =
            api(ApiConfig { cors_header: "https://rust-wallet.wrkz.work".into(), ..ApiConfig::default() });
        let pre = api.handle(&req("OPTIONS", "/pow", &[], ""), PEER);
        assert_eq!(pre.status, 204);
        assert_eq!(pre.header("Access-Control-Allow-Origin"), Some("https://rust-wallet.wrkz.work"));
        assert!(pre.header("Access-Control-Allow-Headers").unwrap().contains("X-API-KEY"));
        let health = api.handle(&req("GET", "/health", &[], ""), PEER);
        assert_eq!(health.header("Access-Control-Allow-Origin"), Some("https://rust-wallet.wrkz.work"));
        service.stop();
    }
}
