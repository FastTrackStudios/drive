//! One connection to the node, as the agent sees it: how its websocket is
//! set up, why it ended (logged once per connection, drive#172), and how
//! long to wait before the next one.

use std::time::Duration;

use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// The websocket settings every connection to a node uses: messages and
/// frames up to [`drive_proto::MAX_MESSAGE`] (the node takes the same).
pub(crate) fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(drive_proto::MAX_MESSAGE))
        .max_frame_size(Some(drive_proto::MAX_MESSAGE))
}

/// Why a connection to the node ended (`conn.end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It never opened.
    ConnectFailed(String),
    /// The handshake could not be sent.
    HelloFailed(String),
    /// The node sent a close frame.
    NodeClosed { code: u16, reason: String },
    /// Reading from the socket failed (a message too big, a reset, TLS).
    ReadFailed(String),
    /// The socket ended without a close frame.
    Hangup,
}

impl Ended {
    fn kind(&self) -> &'static str {
        match self {
            Ended::ConnectFailed(_) => "connect_failed",
            Ended::HelloFailed(_) => "hello_failed",
            Ended::NodeClosed { .. } => "node_closed",
            Ended::ReadFailed(_) => "read_failed",
            Ended::Hangup => "hangup",
        }
    }

    fn detail(&self) -> String {
        match self {
            Ended::ConnectFailed(e) | Ended::HelloFailed(e) | Ended::ReadFailed(e) => e.clone(),
            Ended::NodeClosed { code, reason } => format!("{code} {reason}"),
            Ended::Hangup => String::new(),
        }
    }
}

/// What one connection did, for the event logged when it ends.
#[derive(Debug, Default)]
pub(crate) struct Summary {
    /// The node accepted the handshake ([`drive_proto::ToClient::Welcome`]).
    pub welcomed: bool,
    pub messages_in: u64,
    /// Messages that did not decode (skipped).
    pub undecoded: u64,
    pub largest_in: usize,
    /// The last [`drive_proto::ToClient::Error`] the node sent.
    pub refused: Option<String>,
}

impl Summary {
    pub(crate) fn received(&mut self, len: usize) {
        self.messages_in += 1;
        self.largest_in = self.largest_in.max(len);
    }
}

/// The one event a connection leaves: why it ended and what it did. A
/// warning unless the node let it run for a while and closed it cleanly,
/// so it shows at `drive_native=warn` (the agent's default on a Mac).
pub(crate) fn log_ended(
    url: &str,
    ended: &Ended,
    summary: &Summary,
    lasted: Duration,
    retry: Duration,
) {
    let refused = summary.refused.as_deref().unwrap_or("");
    let lasted_ms = lasted.as_millis() as u64;
    let retry_ms = retry.as_millis() as u64;
    let detail = ended.detail();
    macro_rules! event {
        ($level:ident) => {
            tracing::$level!(
                drive.url = %url,
                conn.end = ended.kind(),
                conn.error = %detail,
                conn.lasted_ms = lasted_ms,
                conn.welcomed = summary.welcomed,
                conn.messages_in = summary.messages_in,
                conn.undecoded = summary.undecoded,
                conn.largest_in = summary.largest_in,
                conn.refused = refused,
                conn.retry_ms = retry_ms,
                "a connection to the node ended"
            )
        };
    }
    let quiet = lasted >= STABLE && matches!(ended, Ended::NodeClosed { .. });
    if quiet {
        event!(info);
    } else {
        event!(warn);
    }
}

/// A connection that stayed up this long was a good one: the next
/// reconnect starts the backoff over.
pub(crate) const STABLE: Duration = Duration::from_secs(30);
const FIRST: Duration = Duration::from_millis(500);
const CAP: Duration = Duration::from_secs(30);

/// How long to wait before reconnecting. Grows (doubling, capped) while
/// connections fail or end soon after opening, so a node that drops a
/// device right away is not redialled every second (drive#172).
#[derive(Debug)]
pub(crate) struct Backoff {
    wait: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { wait: FIRST }
    }
}

impl Backoff {
    /// The wait after a connection that lasted `lasted` (zero if it never
    /// opened). `jitter` in `[0, 1)` spreads it over its upper half, so
    /// devices dropped together do not come back together.
    pub(crate) fn next(&mut self, lasted: Duration, jitter: f64) -> Duration {
        if lasted >= STABLE {
            self.wait = FIRST;
        }
        let wait = self.wait;
        self.wait = (wait * 2).min(CAP);
        wait.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
    }
}

/// A number in `[0, 1)`, different every call (std's hasher keys are
/// random per instance).
pub(crate) fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_that_end_at_once_wait_longer_each_time_up_to_the_cap() {
        let mut b = Backoff::default();
        let waits: Vec<Duration> = (0..10)
            .map(|_| b.next(Duration::from_millis(300), 1.0))
            .collect();
        assert_eq!(waits[0], Duration::from_millis(500));
        assert_eq!(waits[1], Duration::from_secs(1));
        assert!(waits.windows(2).all(|w| w[1] >= w[0]), "{waits:?}");
        assert_eq!(*waits.last().unwrap(), CAP);
    }

    #[test]
    fn a_connection_that_stayed_up_starts_over() {
        let mut b = Backoff::default();
        for _ in 0..6 {
            b.next(Duration::ZERO, 1.0);
        }
        assert_eq!(b.next(STABLE, 1.0), FIRST);
    }

    #[test]
    fn jitter_keeps_a_wait_in_its_upper_half() {
        // The fifth wait is 8 s before jitter.
        let fifth = |j: f64| {
            let mut b = Backoff::default();
            (0..4).for_each(|_| {
                b.next(Duration::ZERO, j);
            });
            b.next(Duration::ZERO, j)
        };
        assert_eq!(fifth(0.0), Duration::from_secs(4));
        assert!(fifth(0.99) > Duration::from_millis(7900) && fifth(0.99) < Duration::from_secs(8));
        let j = jitter();
        assert!((0.0..1.0).contains(&j));
    }
}
