use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

const MAX_CONCURRENT_STREAMS: u32 = 256;
const STREAM_RECEIVE_WINDOW: u32 = 8 * 1024 * 1024;
const CONNECTION_RECEIVE_WINDOW: u32 = 32 * 1024 * 1024;

knot_types::scalar_newtype! {
    pub struct MaxConcurrentStreams(u32) => sealed;
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectionBudget {
    max_concurrent_streams: MaxConcurrentStreams,
    stream_receive_window: u32,
    connection_receive_window: u32,
}

impl ConnectionBudget {
    const DEFAULT: Self = Self {
        max_concurrent_streams: MaxConcurrentStreams::new(MAX_CONCURRENT_STREAMS),
        stream_receive_window: STREAM_RECEIVE_WINDOW,
        connection_receive_window: CONNECTION_RECEIVE_WINDOW,
    };

    pub fn max_concurrent_streams(self) -> MaxConcurrentStreams {
        self.max_concurrent_streams
    }

    pub fn stream_receive_window(self) -> u32 {
        self.stream_receive_window
    }

    pub fn connection_receive_window(self) -> u32 {
        self.connection_receive_window
    }
}

// Separate types that `ListenLimits::new` used to take as
// a bunch of `NonZeroU64` in a row.
// Swapping them would incorrectly compile and
// gave the conn the wrong deadline.
#[derive(Debug, Clone, Copy)]
pub struct HeaderTimeout(Duration);

impl HeaderTimeout {
    pub fn from_millis(millis: NonZeroU64) -> Self {
        Self(Duration::from_millis(millis.get()))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct IdleTimeout(Duration);

impl IdleTimeout {
    pub fn from_millis(millis: NonZeroU64) -> Self {
        Self(Duration::from_millis(millis.get()))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ListenLimits {
    header_timeout: HeaderTimeout,
    idle_timeout: IdleTimeout,
    max_connections: usize,
}

impl ListenLimits {
    pub fn new(
        header_timeout: HeaderTimeout,
        idle_timeout: IdleTimeout,
        max_connections: NonZeroU32,
    ) -> Self {
        Self {
            header_timeout,
            idle_timeout,
            max_connections: max_connections.get() as usize,
        }
    }

    pub fn header_timeout(&self) -> HeaderTimeout {
        self.header_timeout
    }

    pub fn idle_timeout(&self) -> IdleTimeout {
        self.idle_timeout
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    pub fn connection_budget(&self) -> ConnectionBudget {
        ConnectionBudget::DEFAULT
    }
}
