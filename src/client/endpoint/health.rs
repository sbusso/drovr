use std::time::{Duration, Instant};

pub(super) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HealthAction {
    None,
    Ping,
    Expired,
}

pub(super) struct EndpointHealth {
    connected_at: Instant,
    last_received: Instant,
    ping_sent_at: Option<Instant>,
    ready: bool,
}

impl EndpointHealth {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            connected_at: now,
            last_received: now,
            ping_sent_at: None,
            ready: false,
        }
    }

    pub(super) fn received(&mut self, now: Instant) {
        self.last_received = now;
        self.ping_sent_at = None;
    }

    pub(super) fn ready(&mut self) {
        self.ready = true;
    }

    pub(super) fn action(&self, now: Instant) -> HealthAction {
        let initial_snapshot_expired =
            !self.ready && now.saturating_duration_since(self.connected_at) >= HEARTBEAT_TIMEOUT;
        let probe_expired = self
            .ping_sent_at
            .is_some_and(|sent_at| now.saturating_duration_since(sent_at) >= HEARTBEAT_TIMEOUT);
        if initial_snapshot_expired || probe_expired {
            HealthAction::Expired
        } else if self.ping_sent_at.is_none()
            && now.saturating_duration_since(self.last_received) >= HEARTBEAT_INTERVAL
        {
            HealthAction::Ping
        } else {
            HealthAction::None
        }
    }

    pub(super) fn ping_sent(&mut self, now: Instant) {
        self.ping_sent_at = Some(now);
    }

    /// andreconde fork: time since the outstanding ping, if any. Pings only go
    /// out after HEARTBEAT_INTERVAL of silence, so the next message is almost
    /// always the pong.
    pub(super) fn rtt_sample(&self, now: Instant) -> Option<Duration> {
        self.ping_sent_at
            .map(|sent_at| now.saturating_duration_since(sent_at))
    }
}

/// andreconde fork: smoothed round-trip time per endpoint, for the sidebar.
fn rtt_store() -> &'static std::sync::Mutex<std::collections::HashMap<super::ClientEndpointId, f64>>
{
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<super::ClientEndpointId, f64>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(Default::default)
}

pub(super) fn record_rtt(endpoint_id: &super::ClientEndpointId, sample: Duration) {
    let sample = sample.as_secs_f64() * 1000.0;
    let mut store = rtt_store().lock().unwrap_or_else(|e| e.into_inner());
    let smoothed = store
        .get(endpoint_id)
        .map_or(sample, |previous| previous * 0.7 + sample * 0.3);
    store.insert(endpoint_id.clone(), smoothed);
}

pub(crate) fn endpoint_rtt_ms(endpoint_id: &super::ClientEndpointId) -> Option<u32> {
    rtt_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(endpoint_id)
        .map(|ms| ms.round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_connection_is_probed_then_expires_without_a_reply() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        assert_eq!(health.action(now), HealthAction::None);
        assert_eq!(health.action(now + HEARTBEAT_INTERVAL), HealthAction::Ping);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        assert_eq!(
            health.action(now + HEARTBEAT_INTERVAL + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }

    #[test]
    fn any_incoming_message_satisfies_an_outstanding_probe() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        health.ping_sent(now);
        health.received(now + HEARTBEAT_TIMEOUT - Duration::from_millis(1));
        assert_eq!(health.action(now + HEARTBEAT_TIMEOUT), HealthAction::None);
    }

    #[test]
    fn heartbeats_do_not_hide_a_missing_initial_snapshot() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        health.received(now + HEARTBEAT_INTERVAL + Duration::from_secs(1));
        assert_eq!(
            health.action(now + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }
}
