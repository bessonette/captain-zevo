//! Whether the app behind a route is listening where Traefik can reach it.
//!
//! The usual failure is a dev server bound to 127.0.0.1 inside its container:
//! it works from a shell in the container but not from Traefik, which arrives
//! over the container's network interface. The kernel's socket table
//! (`/proc/net/tcp`, present in every Linux container) shows exactly that,
//! with no tools needed inside the container.

use std::collections::BTreeMap;

use serde::Serialize;

/// Command whose output [`parse`] reads. `tcp6` is missing when IPv6 is off,
/// which only loses its lines.
pub const COMMAND: &[&str] = &["sh", "-c", "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Listening {
    /// Listening on an address Traefik can reach.
    Yes,
    /// Listening on the loopback address only.
    LoopbackOnly,
    /// Nothing is listening on the port.
    No,
    /// Not checked: the container is not running, could not be inspected, or
    /// the route points somewhere other than the container.
    Unknown,
}

/// Listening TCP ports from `/proc/net/tcp` and `/proc/net/tcp6` content,
/// mapped to whether any listener is reachable from outside the container.
pub fn parse(proc_net: &str) -> BTreeMap<u16, bool> {
    const LISTEN: &str = "0A";
    let mut ports = BTreeMap::new();
    for line in proc_net.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_), Some(local), Some(_), Some(state)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if state != LISTEN {
            continue;
        }
        let Some((address, port)) = local.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        let reachable = !is_loopback(address);
        *ports.entry(port).or_insert(false) |= reachable;
    }
    ports
}

/// Addresses are hex in host byte order per 32-bit word, so on the usual
/// little-endian machines 127.0.0.1 reads `0100007F` and ::1 ends in
/// `01000000`.
fn is_loopback(address: &str) -> bool {
    let address = address.to_ascii_uppercase();
    match address.len() {
        // IPv4: 127.0.0.0/8
        8 => address.ends_with("7F"),
        32 => {
            address == "00000000000000000000000001000000" // ::1
                || (address.starts_with("0000000000000000FFFF0000") && address.ends_with("7F")) // ::ffff:127.x
        }
        _ => false,
    }
}

/// The state of one port given the parsed table.
pub fn port_state(ports: &BTreeMap<u16, bool>, port: u16) -> Listening {
    match ports.get(&port) {
        Some(true) => Listening::Yes,
        Some(false) => Listening::LoopbackOnly,
        None => Listening::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured shape of the two files: a header line each, then sockets.
    const SAMPLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0BEA 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0
   1: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1 0 100 0 0 10 0
   2: 0B00007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 3 1 0 100 0 0 10 0
   3: 030011AC:0BEA 020011AC:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 4 1 0 100 0 0 10 0
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5 1 0 100 0 0 10 0
   1: 00000000000000000000000001000000:0FA2 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6 1 0 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:0FA3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 7 1 0 100 0 0 10 0
";

    #[test]
    fn tells_reachable_listeners_from_loopback_ones() {
        let ports = parse(SAMPLE);
        assert_eq!(port_state(&ports, 3050), Listening::Yes); // 0.0.0.0:3050
        assert_eq!(port_state(&ports, 8000), Listening::LoopbackOnly); // 127.0.0.1:8000
        assert_eq!(port_state(&ports, 53), Listening::LoopbackOnly); // 127.0.0.11:53
        assert_eq!(port_state(&ports, 5173), Listening::Yes); // [::]:5173
        assert_eq!(port_state(&ports, 4002), Listening::LoopbackOnly); // [::1]:4002
        assert_eq!(port_state(&ports, 4003), Listening::LoopbackOnly); // ::ffff:127.0.0.1
        assert_eq!(port_state(&ports, 9999), Listening::No);
        // The established connection on 3050 (state 01) is not a listener.
        assert_eq!(ports.len(), 6);
    }

    #[test]
    fn a_port_bound_on_both_counts_as_reachable() {
        let both = "\
   0: 0100007F:1F40 00000000:0000 0A
   1: 00000000:1F40 00000000:0000 0A
";
        assert_eq!(port_state(&parse(both), 8000), Listening::Yes);
        assert!(parse("garbage\n\n").is_empty());
    }
}
