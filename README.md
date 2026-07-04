# treadmill-bluetooth-macos

A macOS Bluetooth Low Energy connector for a **Yesoul** treadmill, written in **Rust**.

Discovers the treadmill over BLE (CoreBluetooth), connects, and streams live
telemetry — speed, incline, distance — via the standard **Fitness Machine
Service** (FTMS) GATT profile.

> ⚠️ Early stage. Read-only telemetry first; start/stop and speed/incline
> control is planned. Yesoul may also expose a vendor-specific service that
> still needs reverse engineering.

## Requirements

- macOS with Bluetooth
- Rust 1.95+ (edition 2024)

## Usage

```bash
cargo run             # scan: list nearby BLE devices (diagnostic)
cargo run -- connect  # connect to the first FTMS treadmill and stream data
```

Verbose logs:

```bash
RUST_LOG=debug cargo run -- connect
```

On first run macOS asks for Bluetooth permission — grant it, otherwise the
scan returns nothing.

## Development

```bash
cargo test     # unit tests
cargo clippy   # lints
cargo fmt      # format
```

See [`CLAUDE.md`](./CLAUDE.md) for architecture and protocol notes, and
[`docs/`](./docs) for research and tasks.

## License

MIT — see [`LICENSE`](./LICENSE).
