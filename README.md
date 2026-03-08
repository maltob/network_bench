# Network Bench

A network benchmarking suite designed for real-time visualization and multi-protocol testing.

## Features

- **Multi-Protocol Throughput Testing**: Test performance using QUIC, HTTP, HTTPS, and raw TCP.
- **Latency Testing**: Accurate RTT and jitter detection for network diagnostics.
- **Multicast Testing**: High-throughput multicast performance and latency tracking.
- **Cross-Platform**: Built in Rust for Windows, Linux, and macOS.

## Tools

### 1. `transfer-tool`
High-speed throughput tester - good for resting TCP/HTTPS/QUIC throughput.
- **Scenarios**: `default` (1GB), `jumbo` (10GB), `small` (1MB), and `mixture` (mostly 1MB, some 1GB, rarely a 10GB).
- **Burst Mode**: Use `--connect-each` to test connection setup overhead by forcing new connections for each download.
- **Protocols**: `--proto <quic|http|https|tcp>`.

### 2. `sip-tool`
VoIP simulation and latency tool.
- **Signaling**: Full INVITE -> 200 OK -> ACK flow.
- **Media**: 5-second RTP-like audio stream (20ms intervals).
- **Metrics**: Tracks setup latency, media jitter, and packet loss.

### 3. latency-tool
Measures RTT latency between two systems as well as the jitter.
- **Server**: Run `latency-tool server` to listen for echo requests.
- **Client**: Run `latency-tool client --target <ip>` to measure performance.

### 4. multicast-tool
Measures throughput over multicast. Please note that with no multicast clients, it will likely be line rate. 

### 5. metrics-server 
Centralized metrics collection hub and dashboard. It displays recent results for each tool in a dedicated tab.
- **Dashboard**: Access via `http://localhost:3000/dashboard` in your browser.
- **Database**: Stores all metrics in a local SQLite database.

## Configuration

A unified `config.toml` can be used to set defaults for all tools:

```toml
server_url = "http://127.0.0.1:3000"
duration_secs = 300
interval_ms = 1000
```

## Building

Requires Rust 1.75+.

```bash
cargo build --release
```

Binaries will be located in `target/release/`.

## Quick Start

### 1. Start the Metrics Infrastructure
```bash
# Start the server (default port 3000)
./metrics-server
```
Access the dashboard at `http://localhost:3000/dashboard`.

### 2. Run Latency Tests
```bash
# On Server Node:
./latency-tool server --bind 0.0.0.0:8080

# On Client Node:
./latency-tool client --target <server-ip>:8080
```

### 3. Run SIP (VoIP) Simulation
```bash
# On Server Node:
./sip-tool server --bind 0.0.0.0:5060

# On Client Node:
./sip-tool client --target <server-ip>:5060
```

### 4. Run Throughput Tests (Transfer Tool)
```bash
# Start an HTTPS Server:
./transfer-tool server --proto https --listen 0.0.0.0:4433

# Run a Mixture Scenario Client:
./transfer-tool client --target <server-ip>:4433 --proto https --scenario mixture
```

### 5. Run Multicast Tests
```bash
# On Receiver Node:
./multicast-tool receiver --group 239.0.0.1:5000

# On Sender Node:
./multicast-tool sender --group 239.0.0.1:5000
```
