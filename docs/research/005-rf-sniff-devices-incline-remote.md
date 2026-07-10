# 005 — RF-устройства для сниффа пульта наклона (MacBook)

**Date:** 2026-07-10  
**Context:** incline у Yesoul — **RF-remote only** (FTMS `SetTargetInclination` → `0x04`; см. [001](001-yesoul-ble-protocol.md), backlog [004](../backlog/004-led-control-via-hci-capture.md)). Нужен эфирный capture (± replay), не BLE HCI.

## Workflow

1. Спектр → частота (часто **433.92**, реже 315 / 2.4 GHz / IR).
2. IQ-запись → decode (OOK/FSK, bits, preamble).
3. Replay → incline ↑/↓.
4. Позже: постоянный TX (ESP/CC1101 / CLI) в daemon.

**Mac-софт:** SDR++ · GQRX · CubicSDR · URH · `rtl_433` · inspectrum. (SDR# ≈ Windows.)

## Устройства (Mac USB/serial)

### RX-only SDR (спектр + IQ)

| Device | Band (typ.) | TX | Note |
|---|---|---|---|
| **RTL-SDR Blog V3/V4** | ~0.5–1766 MHz | — | лучший старт |
| Nooelec NESDR (Mini/SMArt/V5) | как RTL | — | TCXO/корпус |
| Generic RTL2832U | ~24–1766 | — | дёшево, хуже drift |
| Airspy Mini/R2 | VHF/UHF, шире BW | — | чувствительнее RTL |
| Airspy HF+ Discovery | HF+ | — | для 433 overkill |
| SDRplay RSP1A/dx/duo | ~до 2 GHz | — | широкий RX |
| FunCube Pro+ | VHF/UHF | — | ниша |
| RX-888 и т.п. | в основном HF | — | не для пульта |

### TX+RX SDR (sniff + replay)

| Device | Band | Note |
|---|---|---|
| **HackRF One** | 1 MHz–6 GHz | half-duplex; стандарт RE |
| **HackRF + Portapack** (H2/H4M) | то же | автономно + USB к Mac |
| LimeSDR Mini/USB | ~0.1–3.8 GHz | full-duplex, сложнее |
| ADALM-Pluto | ~70 MHz–6 GHz | full-duplex, lab |
| bladeRF 2.0 micro | ~47 MHz–6 GHz | pro, overkill |
| USRP B200/B210 | wide | lab, $$$$ |

### Sub-GHz specialists

| Device | Band | TX | Note |
|---|---|---|---|
| **Flipper Zero** | ~300–928 MHz (+IR) | ✓ | быстрый fixed OOK; не 2.4 GHz |
| YARD Stick One + rfcat | ~300–348 / 391–464 / 782–928 | ✓ | Python на Mac |
| PandwaRF | sub-1 GHz | ✓ | primary Android/Linux, не Mac |
| CC1111 USB (rfcat) | sub-GHz | ✓ | дешёвый YS1-класс |
| RF Explorer | model-dep. | — | handheld spectrum |
| TinySA / Ultra | model-dep. | gen. | спектр, слабый decode |

### DIY → Mac serial

| Build | Band | TX | Note |
|---|---|---|---|
| **ESP32 + CC1101** | 315/433/868 | ✓ | sniff+replay, потом в систему |
| Arduino/ESP + 433 OOK (RXB6/SYN*) | 433 OOK | ✓ | только простой OOK |
| M5Stack RF 433 / io433+CC1101 | 433 | ✓ | готовые sniffer-проекты |

### PCB (не эфир)

Saleae / USB LA, осциллограф — DATA pin encoder на плате пульта (EV1527, HT12E, HCS301…).

### Не то

Ubertooth (BLE 2.4) · Proxmark3 (RFID) · nRF BLE sniffer · Wi‑Fi monitor — incline не закроют.

## Рекомендации

| Цель | Покупка |
|---|---|
| Только частота + формат | **RTL-SDR Blog V4** + SDR++ + URH |
| Всё в одном + replay | **HackRF (± Portapack)** |
| 315/433 fixed, UX | **Flipper Zero** (если не 2.4 / сложный PHY → HackRF) |
| Бюджет → постоянный TX | RTL (анализ) + **ESP32+CC1101** (replay) |

## Индонезия (Tokopedia / Shopee ID)

### ✅ Local

| Device | ~IDR | Role |
|---|---|---|
| Flipper Zero (original; много фейков) | 3–7M | Sub-GHz replay |
| HackRF One (часто клоны) | 3–5M+ | sniff+replay |
| HackRF + Portapack H2/H4M | 3.2–5.5M | то же + автоном |
| RTL-SDR Blog V3/V4 | 1.0–1.6M+ | спектр/IQ |
| Generic RTL-SDR | сотни k | бюджет RX |
| ESP32, CC1101, 433 OOK | дёшево | DIY TX/RX |
| TinySA / Ultra | обычно есть | спектр |
| USB logic analyzer | дёшево | PCB |

### 🟡 Import / редко

Nooelec · Airspy · SDRplay · YARD Stick One · PandwaRF · LimeSDR · Pluto · bladeRF/USRP · Saleae Pro.

## Buy-tomorrow shortlist

1. **RTL-SDR Blog V4** — найти частоту  
2. **HackRF + Portapack** — sniff + incline replay  
3. **Flipper Zero** — если 315/433 простой  
4. **ESP32+CC1101** — дешёвый постоянный TX  

**Legal:** свой пульт / своя дорожка; TX на чужие устройства / jamming — нет. ID: SDPPI и т.п. — home lab, min power.
