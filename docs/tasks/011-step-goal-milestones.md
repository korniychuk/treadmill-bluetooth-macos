# 011 — Конфигурируемые step-goal вехи с градуированными toast

## Контекст

У оператора до 3 дневных целей по шагам (дефолт 8000 / 10000 / 12000). Когда
сегодняшняя накопленная сумма шагов пересекает каждый настроенный порог, надо
выстрелить **ровно один** праздничный toast на цель на день, с нарастающей
«яркостью»:

- tier 1 (низший, напр. 8k) — минимальный праздник, базовая иконка;
- tier 2 (средний, напр. 10k) — ярче: больше эмодзи/текста, звук;
- tier 3 (высший, напр. 12k) — максимум: сильнейшие эмодзи/текст, звук.

У macOS toast нет «яркости» — выражаем интенсивность плотностью эмодзи,
формулировкой и системным звуком (`mac-notification-sys`, `.sound(...)`).

## Решение

### Конфиг: per-user, в `$HOME` (НЕ в этом репо)

Цели — личная преференция пользователя, а не данные приложения, поэтому конфиг
**не коммитится в этот репо**. Приложение мультипользовательское: каждый держит
свой файл. Путь `$HOME`-anchored (работает под launchd, где cwd ненадёжен, см.
`store.rs::open`):

- **`~/.config/treadmill-bluetooth-macos/goals.json`** — дефолтный per-user путь.

Формат минимальный, только список порогов (шаблон — `config/goals.example.json`):

```json
{ "goals": [8000, 10000, 12000] }
```

**Резолвинг пути:**

1. `TREADMILL_GOALS_CONFIG` (env) — override, если задан (тесты / нестандартный путь);
2. `$HOME/.config/treadmill-bluetooth-macos/goals.json`;
3. иначе — вшитые дефолты `[8000, 10000, 12000]`.

Логирование edge cases: нет файла — это **норма** (INFO + дефолты), битый/нечитаемый
файл — WARN. `install-daemon.sh` **ничего не прописывает** в plist (дефолтный путь
работает сам). Пользователь приносит свой файл сам — напр. симлинком из личного
dotfiles-репо (в случае оператора — `ankor-dotfiles/treadmill/goals.json` →
`~/.config/treadmill-bluetooth-macos/goals.json`, симлинк ставит dotfiles
`install.sh`).

Битый/отсутствующий конфиг → дефолты + WARN (не падаем).

### Присвоение tier: ранг по возрастанию, кап 3

Tier выводится из порядка, а не хранится в конфиге. Сортируем пороги по
возрастанию, дедуп, кап 3 штуки; tier = позиция+1 (кап 3):

- 3 цели `[8000,10000,12000]` → tier 1/2/3;
- 2 цели `[8000,10000]` → tier 1/2;
- 1 цель `[8000]` → tier 1.

(Альтернатива «верхняя всегда tier 3» отвергнута: одинокая цель 8k стреляла бы
максимальным «absolute machine» toast — не читается.) Это чистая
юнит-тестируемая функция `assign_tiers`.

### Пороговая логика — чистая функция

`goals::thresholds_to_celebrate(today_steps, &goals, &already) -> Vec<Goal>`:
возвращает цели, которые **сейчас** впервые пересечены (`today_steps >= threshold`
И порог ещё не отмечен сегодня). Тесты покрывают: точное попадание в порог,
несколько порогов в одном сэмпле, рестарт среди дня (часть уже отмечена),
переупорядоченный/сокращённый набор (1 или 2 цели). При множественном
пересечении стреляем по возрастанию порога (самый большой — последним).

### Restart-safe состояние в SQLite

Новая таблица (миграция в `store.rs`, паттерн `INSERT OR IGNORE`):

```sql
CREATE TABLE IF NOT EXISTS goal_celebrations (
    date TEXT NOT NULL,
    threshold INTEGER NOT NULL,
    celebrated_at TEXT NOT NULL,
    PRIMARY KEY (date, threshold)
);
```

- `celebrated_thresholds(date) -> Vec<i64>` — что уже отпраздновано за дату;
- `mark_goal_celebrated(date, threshold)` — `INSERT OR IGNORE`.

Дата — **local** (как `credit_activity`/`today_stats`), иначе рассинхрон около
полуночи (UTC/Local) пере-стрелит или подавит toast.

### Проводка в демоне

В `stream_with_presence` после `credit_or_hold`, **только когда `deltas.steps > 0`**
(лишь тогда дневной тотал мог вырасти): берём `store.today_stats()`, дату (local),
`celebrated_thresholds`, зовём чистую функцию; для каждой цели —
`notify::goal_reached(threshold, tier)` + `mark_goal_celebrated`. `goals`
грузим один раз в `run()` (правки конфига подхватываются рестартом демона).

### Текст toast по tier

- tier 1: `🎉 Goal reached: 8,000 steps today` (без звука);
- tier 2: `🔥🎉 10,000 steps — you're on fire today!` (звук `Glass`);
- tier 3: `🏆🔥🎉 12,000 steps — crushing it! Absolute machine 💪` (звук `Hero`).

Число — с разделением тысяч (маленький хелпер).

## Зависимость

Добавляем `serde_json` для парсинга конфига (ручной парсинг JSON хрупок).
Примечание: `logger.rs` сейчас пишет JSONL руками без serde — это первая
serde-зависимость.

## Затронутые файлы

- `Cargo.toml` — `serde_json`.
- `config/goals.example.json` — шаблон формата (реальный конфиг — per-user в `$HOME`, НЕ в репо).
- `src/goals.rs` — новый модуль: загрузка/резолвинг, `assign_tiers`,
  `thresholds_to_celebrate`, тесты.
- `src/store.rs` — таблица `goal_celebrations` + два метода + тесты.
- `src/notify.rs` — `goal_reached`, звук в `toast`.
- `src/daemon.rs` — проводка проверки целей.
- `src/main.rs` — `goal_reached` в smoke-test `run_notify_test`.
- `scripts/install-daemon.sh` — комментарий про per-user `$HOME`-путь (env больше не прописывается).
</content>
