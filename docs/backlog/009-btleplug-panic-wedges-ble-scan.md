# 009 — btleplug panic on discover_services wedges BLE scan forever

**Status:** done → задача [051](../tasks/051-ble-scan-auto-recover.md) (2026-07-11):
panic fail-fast hook (exit 101 → launchd restart), typed `ScanStartFailed` +
`ScanRecovery` (recycle adapter после 3 подряд, exit 87 после 2 recycle без
здорового скана), powered-off радио не кормит wedge-streak (`38de7da`)  
**Severity:** high for MTTR (session dead until manual kickstart)  
**Seen live:** 2026-07-11 smoke ([048](../tasks/048-live-smoke-035-047.md))  
**Class:** liveness / third-party panic / process isolation

## Symptom

Log sequence:

1. `connecting to treadmill id=…`
2. Thread panic (process continues):  
   `Got descriptors for a characteristic we don't know about`  
   at `btleplug-0.12.0/src/corebluetooth/internal.rs:282`
3. `discover services timed out`
4. Forever: `treadmill not found this cycle, retrying err=start filtered BLE scan`  
   (retry every ~5s — scan never starts)

`tm status` still shows daemon alive + AC scanning; widget empty; only recovery is process restart.

## Why KeepAlive did not help

Panic is on an **unnamed btleplug/CoreBluetooth callback thread**, not the main tokio task.  
Process exit code stays 0 / process lives → launchd `KeepAlive` does not restart.

## Workaround (ops)

```bash
launchctl kickstart -k "gui/$(id -u)/com.korniychuk.treadmill-bluetooth-macos.daemon"
```

Wake treadmill console if advertising stopped after failed connect.

## Possible fixes (pick one later)

1. **Fail-fast:** `std::panic::set_hook` or scoped catch that `process::exit(WATCHDOG_EXIT_CODE)` on any panic → launchd restarts (fastest MTTR, blunt).
2. **Adapter recycle:** on `start_scan` failure streak (N× `start filtered`), drop adapter, re-open central, continue loop without full process exit.
3. **Upstream:** upgrade/patch btleplug for descriptor race; or avoid full service discover path that triggers it.
4. **Detect + status:** surface `ble_scan_broken` in `daemon_status` / `tm doctor` when `start_scan` fails repeatedly (observability even before auto-heal).

## Non-goals

- Fixing CoreBluetooth itself.
- Requiring Yesoul app closed is already operator practice; not the root cause here (panic is in our central after connect).

## Acceptance (when implemented)

- [ ] After the same discover panic (or simulated scan-start failure streak), daemon recovers without manual kickstart within ~1 min
- [ ] `tm doctor` or logs make the failure class obvious
- [ ] Normal connect path still works; no false restarts on healthy scans
