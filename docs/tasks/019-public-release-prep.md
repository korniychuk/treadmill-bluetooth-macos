# 019 — Подготовка репозитория к публикации (open-source)

- **Status:** ✅ DONE — репозиторий публичный, релиз `v0.1.0` опубликован (2026-07-06)

## Execution log (2026-07-06)

- ✅ **Секреты:** аудит файлов + всей истории git — чисто, чистка истории не нужна.
- ✅ **Life OS:** прямой интеграции нет; политика зафиксирована в ADR 0002.
- ✅ **WS0 гигиена:** `.claude/*.lock` в gitignore, lock анстейджен, `rust-version=1.95`.
- ✅ **WS1 приватность:** `ReQuant` убран; dotfiles → нейтральное *AnKor Dotfiles*;
  `YS_W2PRO_02395` и `IDENTITY 'AnKor'` оставлены (по решению).
- ✅ **WS2 доки:** README переписан (+ Limitations), CONTRIBUTING, CHANGELOG.
- ✅ **WS3 tmux:** скрипт + README в `scripts/tmux/`; `docs/tasks/009` обновлён.
  ⏳ follow-up вне этого репо: перенаправить симлинк в *AnKor Dotfiles* на этот репо.
- ✅ **WS4 CI/CD:** `.github/workflows/ci.yml` + `release.yml` (unsigned tar.gz);
  плюс `scripts/install-prebuilt.sh` (установка без Rust). ⚠️ не smoke-тестирован.
- ✅ **WS5 политика:** ADR 0002.
- ✅ **Verify:** `cargo fmt --check` чист, `clippy -D warnings` чист, 76 тестов green.
- ✅ **WS6:** репо публичный (`gh repo edit --visibility public`); тег `v0.1.0`
  → `release.yml` собрал unsigned `arm64` tar.gz и создал GitHub Release.
  Install-однострочник (`releases/latest/download/...`) резолвится публично (HTTP 200).
- ✅ **Скриншоты:** 9 шт. (тосты, tmux-виджет walking/paused, CLI) в `docs/screenshots/`,
  встроены в README (секции Demo + tmux). Замусоренные кадры кропнуты.
- ✅ **Follow-up dotfiles:** копия виджета в *AnKor Dotfiles* заменена symlink'ом на
  `scripts/tmux/treadmill-widget.sh` (single source of truth).
- ℹ️ Коммит `b167572` (план) в истории всё ещё содержит литералы `ankor-dotfiles`/
  `ReQuant` — не секрет; оставлено как есть (по решению; force-push в main запрещён).
- ℹ️ Дистрибуция: unsigned tar.gz + `install-prebuilt.sh` ($0). Notarization ($99/год)
  и Homebrew-tap — отклонены/отложены.
- **Owner:** Anton
- **Депендси:** —
- **Затрагивает:** README, `docs/`, `.gitignore`, `scripts/`, `.github/workflows/`,
  `Cargo.toml`, внешний dotfiles-конфиг автора *AnKor Dotfiles* (симлинк tmux-виджета)

## Цель

Сделать приватный репозиторий `treadmill-bluetooth-macos` публичным так, чтобы:

1. **Сторонние люди могли запустить и пользоваться** — clone → сборка → демон →
   статистика, с честно описанными шагами (Bluetooth-permission, self-signing).
2. **Могли поставить tmux-виджет**, если он им нужен — без доступа к чужому
   приватному dotfiles-репо.
3. **Не мешало личному workflow владельца** — правки и использование софта у
   владельца остаются такими же удобными, как сейчас.
4. **Никаких секретов/приватных данных** — ни в файлах, ни в истории коммитов.
5. **Из личной экосистемы (Life OS / dotfiles) ничего лишнего не просачивается**
   в публичный репо — ни сейчас, ни при будущей интеграции.
6. **CI/CD под macOS** — чтобы люди могли скачать готовый артефакт и не пересобирать
   весь Rust с нуля (в рамках того, что реально даёт бесплатный Apple-tier).

Плюс честно задокументированы **текущие ограничения**.

---

## Анализ текущего состояния (по итогам разведки)

Проведён аудит 4 параллельными субагентами: (1) скан секретов файлов+истории,
(2) поиск интеграции с Life OS, (3) анализ tmux-виджета, (4) инвентаризация
готовности к публикации.

### A. Секреты и приватность — БЛОКЕРОВ НЕТ ✅

- **Рабочее дерево:** секретов (API-ключей, токенов, паролей, private keys,
  `.env`, строк подключения к БД) — **не найдено**. Хардкода приватных путей
  (`/Users/anton`, `/Volumes/Code`) в `src/*.rs` — **нет** (только `$HOME`-anchored
  `~/Library/...`, что нормально для macOS-приложения). E-mail в файлах — нет.
  MAC-адресов, SSID — нет.
- **История git (53 коммита):** сообщения коммитов, диффы всей истории
  (`git log --all -G'<pattern>'` по всем чувствительным паттернам),
  удалённые файлы (`--diff-filter=D` — пусто), бинарные blob'ы — **чисто**.
  Никаких `.db`/`.sqlite`/ключей никогда не коммитилось. **Чистка истории
  (BFG / git filter-repo) НЕ требуется.**
- E-mail автора `dev@korniychuk.pro` присутствует только в git author/committer
  метаданных коммитов — это стандартно и нормально для публичного гита.

**Персональные (не секретные) данные — под осознанное решение:**

| # | Что | Где | Рекомендация |
|---|-----|-----|--------------|
| P1 | Serial-подобное BLE-имя `YS_W2PRO_02395` + CoreBluetooth peripheral UUID | `docs/research/gatt-snapshot.json`, `docs/research/001`, `docs/tasks/005` | Заменить на плейсхолдер (`YS_W2PRO_xxxxx`) — не переносимо на чужой хост, но зачем светить конкретный unit. **Recommended: заменить.** |
| P2 | Имя self-signed identity `"AnKor Treadmill BLE Dev"` (дефолт в скриптах) | `scripts/install-daemon.sh`, `scripts/run.sh`, docs | Это ник/бренд, не секрет. Оставить как дефолт, но параметризовать через `IDENTITY` (уже параметризован ✅) и в README показать generic-пример. |
| P3 | Упоминание смежного приватного проекта (не связан с этим репо) | `docs/tasks/003:112` | Убрать — оставить только нейтральное «личная экосистема автора (Life OS)». |
| P4 | Внутренняя структура личного dotfiles-конфига (пути `goals.json`, `install.sh`) | `docs/tasks/009`, `docs/tasks/011` | Переписать под публичную инструкцию «симлинк из вашего конфига»; называть внешний конфиг нейтрально *AnKor Dotfiles*, не раскрывая, что это приватный репо/где хранится. |

### B. Случайно застейдженный / трекнутый мусор ⚠️

- `.claude/scheduled_tasks.lock` — **сейчас staged** (`A` в git status), содержит
  runtime-данные сессии агента (`sessionId`, `pid`, `procStart`). **Не публиковать.**
- `.claude/flow.json` — уже закоммичен (tracked); ledger нумерации work-item'ов
  AnKor AI Spec workflow. Off-topic для публичного репо про дорожку. **Решение D6.**
- `.gitignore` минимальный: только `/target`, `/workouts`. Нужно расширить.

### C. Life OS — прямой интеграции НЕТ ✅

- В коде **нет ни одного HTTP-клиента** (`reqwest`/`hyper`/`ureq` отсутствуют в
  `Cargo.toml`). Всё общение с внешним миром — только BLE (CoreBluetooth) и
  локальный SQLite. Экспорта телеметрии наружу, вебхуков, синхронизаций — **нет**.
- `docs/backlog/` и `docs/ideas/` — пустые (`.gitkeep`).
- Единственные следы личной экосистемы — текстовые упоминания (см. P3, P4).
- **Вывод:** «просачиваться» из Life OS пока нечему. Нужна **политика на будущее**
  (см. Workstream 6), чтобы будущая интеграция не притащила приватное.

### D. tmux-виджет — сейчас вне репо

- Контракт `tm widget` (реализован в `src/main.rs`, документирован в `docs/tasks/009`):
  одна строка, **8 tab-separated полей**:
  `STATE  WORKOUT_COUNT  CUR_WALKING_S  CUR_STEPS  CUR_DISTANCE_M  DAY_WALKING_S  DAY_STEPS  DAY_DISTANCE_M`.
  `STATE ∈ walking|paused|away|unknown`. Пустой вывод + exit 0 = «скрыть сегмент»
  (когда дорожка off / heartbeat старше 95с). Read-only, BLE не открывает.
- Презентация живёт во внешнем dotfiles-конфиге автора (*AnKor Dotfiles*) —
  Dracula **custom-plugin** скрипт (`custom:treadmill.sh`), рендерит цветную
  powerline-pill. Ставится симлинком в `~/.tmux/plugins/tmux/scripts/treadmill.sh`
  собственным `install.sh` этого конфига.
- **Это НЕ TPM-плагин** (у TPM-плагина нужен `<name>.tmux` entry-point + свой репо).
  Это скрипт для Dracula, работает только если Dracula уже статус-бар.

### E. Готовность к публикации — чего не хватает

- **README** — есть, приличный, но без разделов: Limitations / Known Issues,
  установка демона (`install-daemon.sh`), демон/LaunchAgent, self-signing нюанс,
  tmux-виджет, скриншот/пример вывода, CI-бейдж.
- **Нет:** `CONTRIBUTING.md`, `CHANGELOG.md`, `.github/workflows/` (CI с нуля).
- `Cargo.toml`: **`rust-version` (MSRV) не закреплён** (в доках «1.95+»).
- **Смешанный ru/en** в `docs/tasks/*` — рабочий журнал. Решение D5.

---

## Ограничения (для README → Limitations / Known Issues)

Собрано для честного раздела в README:

1. **Только macOS.** `core-foundation`/`io-kit-sys`, embedded Info.plist через
   `build.rs`, `mac-notification-sys`, `codesign`/TCC, LaunchAgent — всё
   macOS-специфично. Linux/Windows не поддерживаются.
2. **Верифицировано на одном устройстве** — Yesoul W2 Pro (`FW SDC_W2_BT_V3.03-50-54`).
   Код написан как generic **FTMS** (`0x1826`), теоретически работает с любой
   FTMS-дорожкой, но нигде кроме W2 Pro не тестировалось.
3. **Наклон (incline) не поддерживается** — W2 Pro не отдаёт inclination через
   FTMS (нет `0x2AD5`, `SetTargetInclination → Operation Failed`). Только RF-пульт.
4. **Двусторонний контроль частичный:** start/stop + SetTargetSpeed —
   реализованы и hardware-verified; incline — нет; LED backlight — backlog
   (`docs/backlog/004`, не начато).
5. **Нет записи в системное Bluetooth-меню macOS by design** (ADR 0001) —
   app-managed periphery без OS-level pairing. Статус — только `tm status` / виджет.
6. **Требуется code signing для стабильного UX** — без своего сертификата
   Bluetooth-permission переспрашивается на каждой пересборке (ad-hoc cdhash меняется).
7. **Скачанный из CI бинарь не notarized** — Gatekeeper покажет «unidentified
   developer»; нужно снять quarantine (`xattr -d com.apple.quarantine`) или собрать
   локально. Полная нотаризация требует платного Apple Developer ID ($99/год).
8. **`goals.json` живёт вне репо** (`~/.config/treadmill-bluetooth-macos/goals.json`),
   создаётся вручную по `config/goals.example.json`.
9. **Протокол Yesoul reverse-engineered, unofficial**, no affiliation with Yesoul.

---

## План выполнения (workstreams)

Порядок: сначала гигиена и приватность (безопасно), затем доки и tmux, затем CI,
и в самом конце — **сделать репозиторий публичным** (необратимый шаг, отдельное
подтверждение владельца).

### WS0 — Гигиена репозитория (быстро, безопасно)

- [ ] `git restore --staged .claude/scheduled_tasks.lock`.
- [ ] `.gitignore`: добавить `.claude/scheduled_tasks.lock` (и/или runtime-мусор
      `.claude/*.lock`), решить по `.claude/flow.json` (D6).
- [ ] Закрепить `rust-version = "1.95"` в `Cargo.toml` (MSRV).
- [ ] Проверить `cargo build`/`test`/`clippy`/`fmt` зелёные после правок.

### WS1 — Приватность / деперсонализация (по решениям D1–D3)

- [ ] P1: заменить `YS_W2PRO_02395` и peripheral UUID на плейсхолдеры в
      `docs/research/*` и `docs/tasks/005` (если решено — D1).
- [ ] P3: убрать упоминание смежного проекта в `docs/tasks/003`, оставить нейтральное «Life OS».
- [ ] P4: переписать «two repos» разделы в `docs/tasks/009`, `011` под публичную
      формулировку (нейтральный «ваш dotfiles/конфиг»).

### WS2 — README + документация для внешних пользователей

- [ ] Переписать `README.md`: badge CI, короткий pitch, скриншот/пример вывода
      `stats`/виджета, **Quickstart** (clone → build → run → connect), **Daemon**
      (`install-daemon.sh`, что он делает, LaunchAgent), **Self-signing**
      (зачем и как, ссылка на `docs/tasks/002`), **tmux widget** (см. WS3),
      **Configuration** (`goals.json`), **Limitations/Known Issues** (список выше),
      disclaimer про Yesoul/reverse-engineering.
- [ ] `CONTRIBUTING.md` — как собрать/тестировать/линтить, docs-first-конвенция,
      стиль коммитов, что PR должен проходить (CI).
- [ ] `CHANGELOG.md` — `Keep a Changelog`, `0.1.0` как первый публичный релиз.
- [ ] `docs/README.md` — обновить индекс (добавить эту задачу, tmux-раздел).
- [ ] (Опц.) Короткий англоязычный `docs/architecture.md` или ссылка на `CLAUDE.md`
      как источник архитектуры для контрибьюторов.

### WS3 — tmux-виджет в репозиторий (Variant A)

- [ ] Перенести скрипт в `scripts/tmux/treadmill-widget.sh` (generic-версия:
      настраиваемые `TREADMILL_BIN`, иконки/цвета сверху файла; убрать
      Dracula-specific хардкод в комментарии/README-инструкцию).
- [ ] `scripts/tmux/README.md` — рецепт установки: (a) Dracula custom-plugin через
      симлинк, (b) generic-tmux вариант (`#(...)` в `status-right`) для тех, у кого
      не Dracula. Описать 8-полевой контракт и «пусто = скрыть».
- [ ] Обновить `docs/tasks/009` — «lives here now: `scripts/tmux/...`; dotfiles
      только симлинкует».
- [ ] **Внешний dotfiles-конфиг автора (*AnKor Dotfiles*, отдельно от этого репо)**: поменять
      `ln -sf` источник в `install.sh` на checkout этого репо. Личный workflow
      владельца не ломается — он по-прежнему правит рабочий файл через симлинк,
      просто цель указывает в этот репо. (Делается вне данного репо — отметить в
      задаче как follow-up, не в этом коммите.)

### WS4 — CI/CD (GitHub Actions, macOS)

- [ ] `.github/workflows/ci.yml`: `runs-on: macos-latest`, matrix при желании;
      шаги `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build`,
      `cargo test`. Кэш `~/.cargo`+`target` (`Swatinem/rust-cache`). Зависимости
      чистые crates.io + `rusqlite` bundled — `brew install` для либ не нужен.
- [ ] `.github/workflows/release.yml`: по тэгу `v*` — `cargo build --release`,
      упаковать `tar.gz` (бинарь + LaunchAgent plist генератор + `install-daemon.sh`
      + иконка), приложить к GitHub Release. **Формат: `tar.gz`, не DMG** (это
      CLI+LaunchAgent, а не `.app`-бандл; DMG избыточен).
- [ ] Артефакт **ad-hoc / unsigned** (без Developer ID). В README — шаг снятия
      quarantine + рекомендация `install-daemon.sh` (локальный self-sign под TCC).
      Полная нотаризация — опционально, только если владелец возьмёт Developer ID (D4).
- [ ] CI-бейдж в README.

### WS5 — Политика Life OS / приватности «на будущее»

- [ ] Зафиксировать в `CONTRIBUTING.md` или `docs/adr/0002-*`: правило —
      **никаких исходящих интеграций с личными системами в этом репо**; интеграция
      с Life OS живёт на стороне Life OS (потребляет публичный CLI-контракт
      `tm widget` / SQLite), а не наоборот. Публичный репо не знает о существовании
      Life OS. Это защищает от утечки приватных URL/схем/названий.

### WS6 — Финал: сделать публичным (⚠️ необратимо, отдельное подтверждение)

- [ ] Финальный ре-скан секретов на итоговом дереве перед flip.
- [ ] `gh repo edit --visibility public` (или через web UI).
- [ ] Создать первый релиз `v0.1.0` (триггерит release-workflow).
- [ ] Post-publish: проверить, что CI зелёный, релизный артефакт скачивается и
      запускается на чистой машине (по возможности).

---

## Открытые решения (нужно согласовать с владельцем)

- **D1.** Заменять ли serial-имя устройства `YS_W2PRO_02395` + peripheral UUID на
  плейсхолдеры в `docs/research/*`? *(Рекоменд.: да — деперсонализировать.)*
- **D2.** Оставить ли имя `"AnKor Treadmill BLE Dev"` как дефолтный `IDENTITY` в
  скриптах? *(Рекоменд.: оставить — это ник/бренд, уже параметризовано.)*
- **D3.** Обобщать ли упоминания смежных личных проектов и dotfiles в docs?
  *(РЕШЕНО: убрать `ReQuant` как несвязанный; dotfiles называть нейтрально
  *AnKor Dotfiles*, не раскрывая хранилище; `YS_W2PRO_02395` и `IDENTITY 'AnKor'`
  оставить.)*
- **D4.** Брать ли Apple Developer ID ($99/год) для нотаризации релизов, или
  публиковать unsigned + инструкция снять quarantine? *(Рекоменд.: пока unsigned +
  инструкция; нотаризация позже при желании.)*
- **D5.** Что делать со смешанным ru/en в `docs/tasks/*`? *(Рекоменд.: оставить как
  рабочий журнал, README/CONTRIBUTING/tmux-README — на английском для аудитории.)*
- **D6.** Гитигнорить ли весь `.claude/` (включая `flow.json`), или только
  runtime-locks? *(Рекоменд.: гитигнорить lock'и; `flow.json` — на усмотрение, лучше
  тоже убрать из публичного репо как off-topic tooling-артефакт.)*
- **D7.** tmux-виджет: подтвердить Variant A (скрипт в этот репо, dotfiles →
  симлинк). *(Рекоменд.: да.)*

---

## Критерии готовности (Definition of Done)

- Секретов нет (повторный скан перед flip — чисто).
- README покрывает: quickstart, daemon, self-signing, tmux, config, limitations.
- `CONTRIBUTING.md`, `CHANGELOG.md`, расширенный `.gitignore`, `rust-version` есть.
- tmux-виджет и его README в репо; dotfiles-симлинк перенастроен (follow-up).
- CI зелёный; release-workflow собирает скачиваемый `tar.gz`.
- Политика Life OS зафиксирована.
- Репозиторий публичный, релиз `v0.1.0` опубликован.
