# Yesoul W2 Pro Treadmill — BLE Protocol Research Synthesis

## 1. Executive Summary — FTMS vs Proprietary Likelihood

**Verdict: treat the W2 Pro as standard FTMS (`0x1826`) until a real capture proves otherwise — high confidence, but not directly verified on this exact model.**

Rationale, in order of evidential weight:

- **No W2 Pro packet capture exists anywhere public** (GitHub, forums, Reddit). Every Yesoul reverse-engineering artifact found targets the S3/C1H **spin bikes**, not treadmills. All treadmill-specific claims below are therefore inference — marked **UNVERIFIED**.
- **Yesoul advertises W2 Pro compatibility with Zwift, Peloton, and Kinomap.** Those third-party platforms consume standard FTMS; native multi-platform support is strong circumstantial evidence the machine exposes standard **FTMS Treadmill Data (`0x2ACD`)**. (Inference, not a capture.)
- **Yesoul's own product line splits two ways:**
  - *Older/vendor path (bikes):* proprietary service `0xFFF0` / write `0xFFF1` / notify `0xFFF4` (qdomyos-zwift `yesoulbike`).
  - *Newer/standard path:* plain FTMS. `Raelx/Yesoul_BLE` and `TrackMyIndoorWorkout` both confirm the **S3 bike** speaks pure FTMS (`0x1826` / Indoor Bike Data `0x2AD2`), no vendor service at all.
- **qdomyos-zwift contains an explicit fallback**: a device named `"YESOUL"` that lacks the `0xFFF0` vendor service is force-treated as a **generic FTMS treadmill**. This is the closest thing to direct evidence that Yesoul's treadmill line is FTMS-based.

**Caveat (do not skip vendor sniffing):** Yves Debeer's Focus Fitness Senator case showed an OEM treadmill that exposed a *vendor* service and required a magic handshake byte-string before it emitted any telemetry — enabling FTMS notify alone did nothing. Yesoul *could* behave similarly. Plan for FTMS, but keep an Android HCI-snoop capture in your back pocket (Section 7).

**Practical stance for the Rust/btleplug connector:** implement a *flag-driven* FTMS `0x2ACD` parser first (never hardcode field offsets — see the S3 "two-packet" quirk in §6), and detect the vendor `0xFFF0` service as a secondary path.

---

## 2. Known Yesoul GATT UUIDs

| UUID | Role | Applies to | Source | Confidence |
|---|---|---|---|---|
| `0x1826` | Fitness Machine Service (FTMS) | S3 bike (confirmed); W2 Pro treadmill (assumed) | Raelx/Yesoul_BLE; TrackMyIndoorWorkout; qdomyos fallback | Confirmed for bike; **UNVERIFIED** for W2 Pro |
| `0x2AD2` | Indoor Bike Data (notify) | S3 bike | Raelx/Yesoul_BLE; TrackMyIndoorWorkout | Confirmed (bike only) |
| `0x2ACD` | Treadmill Data (notify) | W2 Pro treadmill | Inferred from FTMS assumption | **UNVERIFIED** |
| `0x2AD9` | Fitness Machine Control Point (write/indicate) | W2 Pro (control) | Inferred from FTMS spec | **UNVERIFIED** |
| `0x2ADA` | Fitness Machine Status (notify) | W2 Pro (status) | Inferred from FTMS spec | **UNVERIFIED** |
| `0xFFF0` | Vendor-specific service (proprietary) | Yesoul **bikes** (C1H, S3, M1, G1 family, A-series) | qdomyos-zwift `yesoulbike.cpp` | Confirmed (bike only); not confirmed for treadmills |
| `0xFFF1` | Vendor write characteristic | Yesoul bikes | qdomyos-zwift `yesoulbike.cpp` | Confirmed (bike only) |
| `0xFFF4` | Vendor notify characteristic (12-byte frame) | Yesoul bikes | qdomyos-zwift `yesoulbike.cpp` | Confirmed (bike only) |
| `0x2902` | CCCD descriptor (enable notify/indicate) | Standard | qdomyos-zwift | Standard GATT |

**Advertising / identification hints:**
- BLE name prefixes routed to Yesoul bikes: `YS_C1_`, `YS_G1_`, `YS_M1P_`, `YS_G1MPLUS`, `YS_G1MMAX`, `YS_G1M_`, `YS_A…`, plus literal `YESOUL`. (qdomyos-zwift `bluetooth.cpp`) — **W2 Pro's advertised name prefix is UNKNOWN; verify on hardware.**
- Manufacturer data (bike, Android background detection): ID `637` (0x027D), payload `{ 0x01, 0x05, 0x00, 0xFF, 0xFF }`. (qdomyos-zwift `yesoulbike.h`) — bike-only, **UNVERIFIED** for treadmill.
- Manufacturer name strings seen on S3: `"Yesoul"`, `"FUJIAN YESOUL"`; some units ship a **Huawei** BLE console instead of Fujian, and Android 13 could suppress the name entirely. (TrackMyIndoorWorkout) — relevant because name-based detection is fragile.

---

## 3. FTMS Treadmill Data (`0x2ACD`) — Exact Field Layout (notify)

Source: Bluetooth SIG FTMS v1.0 spec (Researcher 4, parsed from primary PDF). Applies to any spec-compliant treadmill; W2 Pro conformance **UNVERIFIED**.

**Wire structure:** `Flags (uint16, little-endian) + present fields packed in bit order (LSO→MSO)`. All multi-byte fields little-endian.

**Flags bitmask** (bit set ⇒ field present, *except bit 0 which is inverted*):

| Bit | Meaning |
|---|---|
| 0 | **More Data** — *inverted*: bit **clear (0)** ⇒ Instantaneous Speed IS present. **UNVERIFIED semantics — unit-test against a real capture.** |
| 1 | Average Speed present |
| 2 | Total Distance present |
| 3 | Inclination + Ramp Angle Setting present |
| 4 | Positive + Negative Elevation Gain present |
| 5 | Instantaneous Pace present |
| 6 | Average Pace present |
| 7 | Expended Energy (Total + Per Hour + Per Minute) present |
| 8 | Heart Rate present |
| 9 | Metabolic Equivalent present |
| 10 | Elapsed Time present |
| 11 | Remaining Time present |
| 12 | Force on Belt + Power Output present |
| 13–15 | RFU |

**Field order** (each present field appended in this sequence):

| # | Field | Type | Unit | Resolution |
|---|---|---|---|---|
| 1 | Instantaneous Speed | uint16 | km/h | 0.01 |
| 2 | Average Speed | uint16 | km/h | 0.01 |
| 3 | Total Distance | uint24 | m | 1 |
| 4 | Inclination | sint16 | % | 0.1 |
| 5 | Ramp Angle Setting | sint16 | ° | 0.1 |
| 6 | Positive Elevation Gain | uint16 | m | 0.1 |
| 7 | Negative Elevation Gain | uint16 | m | 0.1 |
| 8 | Instantaneous Pace | uint8 | km/min | 0.1 |
| 9 | Average Pace | uint8 | km/min | 0.1 |
| 10 | Total Energy | uint16 | kcal | 1 |
| 11 | Energy Per Hour | uint16 | kcal | 1 |
| 12 | Energy Per Minute | uint8 | kcal | 1 |
| 13 | Heart Rate | uint8 | bpm | 1 |
| 14 | Metabolic Equivalent | uint8 | MET | 0.1 |
| 15 | Elapsed Time | uint16 | s | 1 |
| 16 | Remaining Time | uint16 | s | 1 |
| 17 | Force on Belt | sint16 | N | 1 |
| 18 | Power Output | sint16 | W | 1 |

**Implementation note:** parse `flags` first, then walk fields conditionally — never assume fixed byte offsets. The Yesoul S3 bike is documented to split one logical sample across **two notifications with different flag masks** (§6); expect the same on the treadmill.

---

## 4. FTMS Control Point (`0x2AD9`) — Opcodes & Byte Formats (write + indicate)

Source: FTMS v1.0 spec. **W2 Pro control support entirely UNVERIFIED** — this is the standard the device *should* implement if control is exposed.

**Value layout:** `Op Code (uint8) + Parameter (0–18 octets, little-endian)`.

| Op Code | Name | Parameter |
|---|---|---|
| `0x00` | Request Control | none |
| `0x01` | Reset | none |
| `0x02` | Set Target Speed | uint16, km/h, ×0.01 |
| `0x03` | Set Target Inclination | sint16, %, ×0.1 |
| `0x04` | Set Target Resistance | uint8, ×0.1 |
| `0x05` | Set Target Power | sint16, W, ×1 |
| `0x06` | Set Target Heart Rate | uint8, bpm |
| `0x07` | Start or Resume | none |
| `0x08` | Stop or Pause | uint8: `0x01`=Stop, `0x02`=Pause |
| `0x09` | Set Targeted Expended Energy | uint16, kcal |
| `0x0A` | Set Targeted Steps | uint16 |
| `0x0B` | Set Targeted Strides | uint16 |
| `0x0C` | Set Targeted Distance | uint24, m |
| `0x0D` | Set Targeted Training Time | uint16, s |
| `0x0E–0x10` | Set Targeted Time in N HR Zones | array of uint16 per zone, s |
| `0x11` | Set Indoor Bike Simulation Params | Wind sint16 (m/s ×0.001), Grade sint16 (% ×0.01), Crr uint8 (×0.0001), Cw uint8 (kg/m ×0.01) |
| `0x12` | Set Wheel Circumference | uint16, mm, ×0.1 |
| `0x13` | Spin Down Control | uint8: `0x01`=Start, `0x02`=Ignore |
| `0x14` | Set Targeted Cadence | uint16, 1/min, ×0.5 |
| `0x80` | Response Code (server→client) | see below |

> Note: the `0x0E–0x11` HR-zone rows and the `0x11` Bike Simulation row overlap in the raw research (opcode range ambiguity). Bike-simulation is cycling-only and irrelevant to a treadmill; **verify exact opcode boundaries against the spec before relying on `0x0E`–`0x11`.**

**Response (server indicates back on `0x2AD9`), ≥3 bytes:**
```
Byte 0: 0x80              (Response Op Code, fixed)
Byte 1: <request opcode>  (echo of the processed opcode)
Byte 2: <result code>
Byte 3..N: optional response parameter (e.g. Spin Down success)
```

**Result codes:** `0x01` Success · `0x02` Op Code Not Supported · `0x03` Invalid Parameter · `0x04` Operation Failed · `0x05` Control Not Permitted · (`0x00`, `0x06–0xFF` RFU).

**Concrete example bytes (little-endian):**
- Request Control: `[0x00]`
- Set Target Speed 6.00 km/h → 600 = 0x0258 → `[0x02, 0x58, 0x02]`
- Set Target Inclination 2.0% → 20 = 0x0014 → `[0x03, 0x14, 0x00]`
- Start/Resume: `[0x07]`
- Stop: `[0x08, 0x01]` · Pause: `[0x08, 0x02]`
- Success response echo for the Start: `[0x80, 0x07, 0x01]`

**Fitness Machine Status (`0x2ADA`) — notify only** (`Op Code + Parameter`), useful for observing external state changes:

| Op Code | Meaning | Parameter |
|---|---|---|
| `0x01` | Reset | — |
| `0x02` | Stopped/Paused by User | uint8 (0x01 Stop / 0x02 Pause) |
| `0x03` | Stopped by Safety Key | — |
| `0x04` | Started/Resumed by User | — |
| `0x05` | Target Speed Changed | uint16 km/h ×0.01 |
| `0x06` | Target Incline Changed | sint16 % ×0.1 |
| `0x07` | Target Resistance Changed | uint8 ×0.1 |
| `0x08` | Target Power Changed | sint16 W |
| `0x09` | Target HR Changed | uint8 bpm |
| `0x0A–0x15` | various "Targeted X Changed" | type matches the corresponding CP setter |
| `0xFF` | **Control Permission Lost** | — |

---

## 5. Yesoul Proprietary Framing (bikes only — NOT confirmed for treadmill)

Found only for Yesoul **bikes** via the vendor `0xFFF0`/`0xFFF1`/`0xFFF4` service (qdomyos-zwift `yesoulbike.cpp`). Included for completeness / in case the W2 Pro exposes a similar vendor path. **Do NOT assume any of this applies to the W2 Pro treadmill.**

**Init handshake** written to `0xFFF1` on connect (the only command Yesoul bikes require):
```
[0xF5, 0x20, 0x20, 0x40, 0xF6]
```
No distinct start/stop/speed vendor commands exist for the bike (a resistance-write function is present but dead/commented-out). No treadmill vendor command set is known anywhere.

**Notify frame on `0xFFF4` — fixed 12 bytes, big-endian multi-byte fields** (bike telemetry):

| Offset | Field | Encoding |
|---|---|---|
| 0–1 | unused (likely elapsed time / status) | not parsed |
| 2–3 | Distance | uint16 BE, ÷100 → km |
| 4 | Resistance level | uint8 |
| 5 | unused | not parsed |
| 6 | Cadence | uint8, RPM |
| 7–8 | Power | uint16 BE, watts |
| 9–11 | unused (likely mode flags / checksum) | not parsed |

- Speed is *derived client-side*, not on the wire: `Speed = 0.37497622 × Cadence` (km/h, empirical) or power-model fallback.
- **No checksum/CRC** is validated in any Yesoul implementation reviewed.
- Note the **endianness contrast**: vendor bike frame is **big-endian**; standard FTMS is **little-endian**. Don't cross-wire the two parsers.

---

## 6. Practical Gotchas

**Yesoul-firmware-specific (observed on bikes, expect on treadmill):**
- **Multi-packet flag splitting.** The Yesoul S3 does NOT pack all fields into one notification. It alternates two packets with different flag masks — e.g. packet A `flags=0x0800` (speed + elapsed only), packet B `flags=0x01F5` (cadence + distance + power + calories). A generic client must merge fragments by flag-signature, not expect one superset packet. **Assume the W2 Pro treadmill does the same on `0x2ACD`** — handle varying flag combos across consecutive notifications and null-coalesce into one synthesized record.
- **Do not copy hardcoded offsets from bike bridges.** `Raelx/Yesoul_BLE` uses fixed offsets (bytes 4–5 cadence, 9 resistance, 11–12 power ×1.28 correction) valid *only* for that unit's specific flag combo — the author explicitly warns it's not a general parser. Always parse the FTMS flags field.
- **Name-based detection is fragile.** S3 units ship with either "Fujian" or "Huawei" BLE consoles; Android 13 sometimes suppressed the advertised name entirely. Prefer service-UUID matching over name matching where possible.

**FTMS Control Point protocol (spec-mandated):**
- **Request Control first.** Send `0x00` and wait for result `0x01` before any other control opcode, else you get `0x05 Control Not Permitted`.
- **Control Point uses indications, not notifications.** Enable the CCCD for indication before writing, or the server returns "CCCD Improperly Configured." On btleplug/macOS CoreBluetooth, `subscribe()` should handle indicate-only chars transparently, **but verify with a capture** — some vendor stacks quirk on this.
- **Serialize writes.** Wait for the `0x80` indication before sending the next opcode, or you'll hit ATT "Procedure Already In Progress."
- **Control is pre-emptible.** The OEM app grabbing control or a user touching the console silently revokes yours — watch `0x2ADA` for `0xFF Control Permission Lost` and re-request.
- **Elapsed/Remaining time only advance after Start/Resume; Reset (`0x01`) zeroes them.** Stop/Pause does not reset elapsed time.

**Vendor-handshake risk (the big unknown):**
- Some OEM treadmills (Yves Debeer's Focus Fitness Senator) expose a **vendor service** and emit *zero* telemetry until a magic byte-string handshake (`f0 c3 03 00 00 00 b6`) is written — plain FTMS notify subscription did nothing. If the W2 Pro is silent after subscribing, suspect a required vendor handshake and go to sniffing (§7).

---

## 7. Open Questions to Resolve on Real Hardware

1. **Does the W2 Pro expose `0x1826` FTMS at all?** Scan its GATT tree — confirm `0x1826` + `0x2ACD` presence (and whether `0x2AD9`/`0x2ADA` exist).
2. **Is there a vendor service (`0xFFF0` or a 128-bit UUID)?** Enumerate *all* services, not just FTMS.
3. **What is the W2 Pro's advertised BLE name prefix and manufacturer data?** (Bike prefixes are `YS_*`/`YESOUL`; treadmill unknown.)
4. **Does it require a handshake before telemetry?** Subscribe to `0x2ACD` and check whether notifications arrive with no prior write.
5. **What exact flag masks does `0x2ACD` emit, and does it multi-packet split?** Capture several notifications; validate the bit-0 "More Data" inverted semantics empirically.
6. **Is bidirectional control supported?** Try Request Control (`0x00`) → Start (`0x07`) → Set Target Speed, and observe result codes / actual belt response.
7. **Does control require the OEM app to be disconnected?** Test pre-emption behavior.

**Reverse-engineering method (proven):** Android Developer Options → enable Bluetooth HCI snoop log → run the official Yesoul app through connect / start / stop / speed-change / incline-change → `adb pull /sdcard/btsnoop_hci.log` → open in Wireshark filtered to the device's BLE address/name → diff payloads across actions to map opcodes/fields empirically.

---

## 8. Sources

**Yesoul bike implementations (proprietary + FTMS confirmed for bikes):**
- cagnulein/qdomyos-zwift — `yesoulbike.{h,cpp}`, `bluetooth.cpp`, Equipment Compatibility wiki, issues #1443/#2185/#1410. https://github.com/cagnulein/qdomyos-zwift
- Raelx/Yesoul_BLE — ESP32/NimBLE S3 bridge, FTMS `0x1826`/`0x2AD2`. https://github.com/Raelx/Yesoul_BLE (local: `…/scratchpad/Yesoul_BLE/src/main.cpp`)
- TrackMyIndoorWorkout/TrackMyIndoorWorkout — `device_fourcc.dart`, `device_factory.dart`, `fitness_equipment.dart`, `yesoul_s3_test.dart`. https://github.com/TrackMyIndoorWorkout/TrackMyIndoorWorkout · Changelog: https://trackmyindoorworkout.github.io/changelog/

**W2 Pro product info (compatibility inference only):**
- YESOUL W2 Pro Specifications — https://support.yesoulfitness.com/hc/en-001/articles/45968589061915-Product-Specifications-YESOUL-W2-Pro
- YESOUL W2 Pro product page — https://yesoulfitness.com/products/yesoul-w2-pro

**FTMS spec + treadmill reverse-engineering:**
- Bluetooth SIG FTMS v1.0 (primary, all opcode/field tables) — https://www.onelap.cn/pdf/FTMS_v1.0.pdf (local parse: `…/scratchpad/ftms.txt`)
- GATT XML, Treadmill Data — https://raw.githubusercontent.com/oesmith/gatt-xml/master/org.bluetooth.characteristic.treadmill_data.xml
- Yves Debeer, "Hacking the bluetooth of my treadmill" — https://yvesdebeer.github.io/Treadmill-Bluetooth/
- Yves Debeer, "Using an ESP32 to retrieve treadmill data" — https://yvesdebeer.github.io/Using-an-ESP32-to-retrieve-treadmill-data-via-bluetooth/
- Nordic DevZone, FTMS Control Point (indication requirement, response format) — https://devzone.nordicsemi.com/f/nordic-q-a/56186/
- fitnesskit.github.io, Fitness Machine Control reference — https://fitnesskit.github.io/BluetoothMessageProtocol/Fitness%20Machine%20Control.html
- Marcel R.V. gist, Bluetooth Fitness Data notes — https://gist.github.com/marcelrv/6e8f75b2aa6b3967b8159bc6a8617a47
