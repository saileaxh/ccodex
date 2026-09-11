//! Telemetry facade: turn tracker + analytics events channel + Statsig metrics,
//! bundled so the forward/gateway layers deal with one handle. Emissions produced
//! by the tracker are fanned out to the account's analytics queue (official JSON)
//! and the metrics client (official instrument set) here.

use crate::accounts::Account;
use crate::analytics::{
    AnalyticsHub, CodexCompactionEventRequest, CodexTurnEventRequest, ThreadInitializedEvent,
    TrackEventRequest,
};
use crate::metrics::MetricsHub;
use crate::turns::{Emission, TurnEnv, TurnTracker};

pub struct Telemetry {
    pub tracker: TurnTracker,
    pub analytics: AnalyticsHub,
    pub metrics: MetricsHub,
}

impl Telemetry {
    pub fn new(proxies: std::sync::Arc<crate::proxies::ProxyStore>) -> Self {
        Self {
            tracker: TurnTracker::new(TurnEnv::new()),
            analytics: AnalyticsHub::new(proxies),
            metrics: MetricsHub::new(),
        }
    }

    /// Fan out tracker emissions for an account: analytics event to the account queue,
    /// accompanying metrics with the official instrument/tag set. Both channels egress
    /// through the account's own proxy binding (same exit IP as its data-plane traffic).
    pub fn emit(&self, account: &Account, emissions: Vec<Emission>) {
        if emissions.is_empty() {
            return;
        }
        let sender = self.analytics.sender_for(account);
        for emission in emissions {
            match emission {
                Emission::ThreadInitialized(params) => {
                    self.metrics.thread_started(&account.metrics, &params.model);
                    sender.track(TrackEventRequest::ThreadInitialized(
                        ThreadInitializedEvent {
                            event_type: "codex_thread_initialized",
                            event_params: *params,
                        },
                    ));
                }
                Emission::Turn(turn) => {
                    let model = turn.params.model.clone().unwrap_or_default();
                    self.metrics.turn_finished(
                        &account.metrics,
                        &model,
                        turn.ttft_ms,
                        turn.ttfm_ms,
                        turn.params.duration_ms.unwrap_or(0),
                        turn.params.total_tool_call_count.unwrap_or(0),
                    );
                    sender.track(TrackEventRequest::TurnEvent(Box::new(
                        CodexTurnEventRequest {
                            event_type: "codex_turn_event",
                            event_params: turn.params,
                        },
                    )));
                }
                Emission::Compaction(compaction) => {
                    self.metrics.task_compact(
                        &account.metrics,
                        &compaction.model,
                        compaction.compact_type,
                        compaction.manual,
                    );
                    sender.track(TrackEventRequest::Compaction(Box::new(
                        CodexCompactionEventRequest {
                            event_type: "codex_compaction_event",
                            event_params: compaction.params,
                        },
                    )));
                }
            }
        }
    }

    /// Metrics client of an account identified by name (stream path): `None` when the
    /// account vanished mid-reload — same drop semantics as `emit_for_account_name`.
    pub fn metrics_for_name(
        &self,
        pool: &crate::accounts::Pool,
        account_name: &str,
    ) -> Option<std::sync::Arc<codex_otel::MetricsClient>> {
        pool.accounts()
            .iter()
            .find(|a| a.name == account_name)
            .map(|a| std::sync::Arc::clone(&a.metrics))
    }

    /// Emit for an account identified by name (stream-tail path): looks the account up
    /// in the pool; if it vanished mid-reload, events are dropped (official equivalent:
    /// process exited before the queue drained).
    pub fn emit_for_account_name(
        &self,
        pool: &crate::accounts::Pool,
        account_name: &str,
        emissions: Vec<Emission>,
    ) {
        if emissions.is_empty() {
            return;
        }
        if let Some(account) = pool.accounts().iter().find(|a| a.name == account_name) {
            self.emit(account, emissions);
        }
    }

    /// Background maintenance of the turn tracker: every second, finalize failed turns
    /// whose retry grace expired (their event must not wait for the next request on that
    /// session) and drop TTL-expired state (silent = process-kill analog). `pool` is a
    /// snapshot provider so the task doesn't hold the AppState.
    pub fn spawn_sweep<F>(self: &std::sync::Arc<Self>, pool: F)
    where
        F: Fn() -> crate::accounts::Pool + Send + 'static,
    {
        let this = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                for (account, emission) in this.tracker.sweep() {
                    let pool = pool();
                    this.emit_for_account_name(&pool, &account, vec![emission]);
                }
            }
        });
    }
}
