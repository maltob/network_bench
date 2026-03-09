# Network Bench

A comprehensive network benchmarking suite designed for real-time visualization, multi-protocol testing, and deep-dive network validation.

## Features

- **Multi-Protocol Throughput Testing**: Test performance using QUIC, HTTP, HTTPS, and raw TCP (`transfer-tool`, `tcp-bench`, `udp-bench`).
- **Latency & Path Analysis**: Accurate RTT, jitter detection, and MTR-style hop-by-hop tracking (`latency-tool`, `path-tool`).
- **Advanced Validation Validation**: Stress-test connection limits, validate QoS/DSCP markings, measure DNS reliability, and detect Path MTU blackholes.
- **Suite-Wide Simplified CLI**: Automatically assumes client modes when target IPs are provided, speeding up testing workflows.
- **Real-time Dashboard**: All tools automatically report telemetry to the `metrics-server` for live charting.
- **Cross-Platform**: Built in Rust for Windows, Linux, and macOS.

## Core Tools

### 1. `transfer-tool`
High-speed throughput tester - great for testing TCP, HTTPS, and QUIC throughput.
- **Burst Mode**: Use `--connect-each` to test connection setup overhead by forcing new connections for each download.
- **Usage**:
  - Server: `./transfer-tool server --proto https --listen 0.0.0.0:4433`
  - Client: `./transfer-tool 192.168.1.10:4433 --proto https --scenario mixture`

### 2. `tcp-bench`
Measures TCP throughput and calculates Bandwidth Delay Product (BDP) in real-time.
- **Usage**:
  - Server: `./tcp-bench server`
  - Client: `./tcp-bench 192.168.1.10`

### 3. `udp-bench`
Measures raw UDP throughput, loss, and jitter.
- **Usage**:
  - Server: `./udp-bench server`
  - Client: `./udp-bench 192.168.1.10 --rate 100mbps`

### 4. `latency-tool`
Measures basic UDP RTT latency between two systems as well as the jitter.
- **Usage**:
  - Server: `./latency-tool server`
  - Client: `./latency-tool 192.168.1.10`

### 5. `path-tool`
MTR-style tool that measures loss and latency for every hop in the network path (ICMP).
- **Usage**: `./path-tool 8.8.8.8` (requires Admin/Root privileges)

### 6. `multicast-tool`
Measures throughput over multicast groups.
- **Usage**:
  - Receiver (Default): `./multicast-tool 224.0.0.251`
  - Sender: `./multicast-tool sender 224.0.0.251`

### 7. `sip-tool`
VoIP simulation tool. Evaluates SIP signaling (INVITE -> 200 OK) and 5-second RTP-like media streams.
- **Usage**:
  - Server: `./sip-tool server`
  - Client: `./sip-tool 192.168.1.10`

---

## Validation Edge-Tools

### 8. `qos-tool`
Validates QoS enforcement by injecting UDP packets with specific DSCP (IP_TOS) markings.
- **Usage**:
  - Server: `./qos-tool server`
  - Client: `./qos-tool 192.168.1.10 --dscp 46`

### 9. `cps-bench`
Validates Connections Per Second (CPS) by heavily stress-testing TCP connection setups. Crucial for NAT gateways and load balancers.
- **Usage**:
  - Server: `./cps-bench server`
  - Client: `./cps-bench 192.168.1.10 --concurrency 500`

### 10. `dns-bench`
Validates DNS reliability and speed against specific resolvers without caching bias. Uses standard udp/tcp resolution.
- **Usage**: `./dns-bench 8.8.8.8 --qps 50 -D google.com,cloudflare.com`

### 11. `pmtu-tool` & `mtu-discovery`
Discovers Path MTU and identifies silent blackholes by sweeping UDP packets with the Don't Fragment (DF) bit enabled.
- **Usage**:
  - Server: `./mtu-discovery server`
  - Client: `./mtu-discovery client --target 192.168.1.10 --perf-check`
- **Performance Check**: The `--perf-check` flag runs a fragmentation validation test. **Note:** Throughput (Mbps) may appear *higher* for fragmented packets if the bottleneck is CPU syscall rate rather than physical bandwidth. However, fragmentation incurs a measurable latency penalty and risks complete packet loss (blackholing) on misconfigured networks.

### 12. `rrul-bench`
Bufferbloat & AQM Tester. Simulates "latency under load" by simultaneously running a high-bandwidth TCP upload stream and measuring precise UDP latency dynamically to expose unmanaged router queues.
- **Usage**:
  - Server: `./rrul-bench server`
  - Client: `./rrul-bench 192.168.1.10 --concurrency 4`

---

## Centralized Reporting

### 13. `metrics-server`
Centralized metrics collection hub and dashboard. It dynamically generates charts for any of the above tools sending it data.
- **Usage**: `./metrics-server`
- **Dashboard**: Access via `http://localhost:3000/dashboard` in your browser.

## Configuration

A unified `config.toml` can be used to set defaults for all tools without passing CLI flags:

```toml
server_url = "http://127.0.0.1:3000"
duration_secs = 300
interval_ms = 1000
```

## Building

Requires Rust 1.70+.

```bash
cargo build --release --workspace
```

Binaries will be located in `target/release/`.
