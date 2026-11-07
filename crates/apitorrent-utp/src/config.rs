use std::time::Duration;

/// Settings for a socket and its connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The largest datagram we send, uTP header included. The default, 1400, fits an
    /// Ethernet, `PPPoE` or typical VPN path for IPv4 and IPv6 without fragmentation.
    /// See the crate docs on path MTU.
    pub packet_size: usize,
    /// Bytes the application may queue for sending, including bytes in flight.
    pub send_buffer: usize,
    /// Bytes we buffer for the application, in order or not. This is the window we
    /// advertise.
    pub recv_buffer: usize,
    /// LEDBAT's queuing-delay target.
    pub target_delay: Duration,
    /// How long `connect` keeps retrying the SYN.
    pub connect_timeout: Duration,
    /// Consecutive retransmission timeouts, with no packet from the peer in between, after
    /// which the connection is dropped. With the RTO doubling each time, the default (7)
    /// gives up after about a minute of silence.
    pub max_timeouts: u32,
    pub initial_rto: Duration,
    pub min_rto: Duration,
    pub max_rto: Duration,
    /// Send an ACK after this long without sending anything, so NATs keep the mapping.
    pub keepalive: Duration,
    /// Connections that arrived but were not accepted yet; more are refused with a reset.
    pub accept_backlog: usize,
    /// The most live connections on one socket.
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            packet_size: 1400,
            send_buffer: 1024 * 1024,
            recv_buffer: 1024 * 1024,
            target_delay: Duration::from_millis(100),
            connect_timeout: Duration::from_secs(10),
            max_timeouts: 7,
            initial_rto: Duration::from_secs(1),
            min_rto: Duration::from_millis(500),
            max_rto: Duration::from_secs(16),
            keepalive: Duration::from_secs(29),
            accept_backlog: 128,
            max_connections: 4096,
        }
    }
}

impl Config {
    /// The smallest allowed `packet_size`: the IPv6 minimum MTU less the IPv6 and UDP
    /// headers.
    pub const MIN_PACKET_SIZE: usize = 1232;
    /// The largest allowed `packet_size`: a 1500-byte MTU less the IPv4 and UDP headers.
    pub const MAX_PACKET_SIZE: usize = 1472;

    /// The configuration with every field brought into its valid range.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let packet_size = self
            .packet_size
            .clamp(Self::MIN_PACKET_SIZE, Self::MAX_PACKET_SIZE);
        Self {
            packet_size,
            send_buffer: self.send_buffer.max(packet_size * 4),
            recv_buffer: self.recv_buffer.max(packet_size * 4),
            target_delay: self.target_delay.max(Duration::from_millis(1)),
            max_timeouts: self.max_timeouts.max(1),
            min_rto: self.min_rto.max(Duration::from_millis(10)),
            max_rto: self.max_rto.max(self.min_rto),
            accept_backlog: self.accept_backlog.max(1),
            max_connections: self.max_connections.max(1),
            ..*self
        }
    }
}
