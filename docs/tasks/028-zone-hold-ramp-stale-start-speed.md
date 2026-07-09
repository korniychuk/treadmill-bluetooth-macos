# 028 — Zone Hold ramp used stale pre-write belt speed

## Симптом

После watchdog-рестарта демона (`silent hang detected — exiting so launchd
restarts the daemon`) во время тренировки: пользователь заходит на ленту,
демон применяет вычисленную дефолтную скорость (0.5 → 2.6 км/ч, задача 016) —
скорость видимо встаёт нормально. Через ~20с (первый Zone Hold correction
tick) скорость внезапно падает до ~0.6 км/ч и потом медленно растёт обратно
к 2.6 в течение нескольких минут.

## Причина

`src/daemon.rs`, ветка `stream_with_presence`: при первом приходе в
`Walking` Zone Hold engage (`zone_hold_on_transition`) стартовал warm-up
`Ramp` со `start_speed_kmh = zh_resumed_kmh`, а `zh_resumed_kmh` читался как
`data.speed_kmh.unwrap_or(0.0)` — **тот же самый телеметрический сэмпл**,
что и заголовок matcha на presence-transition, т.е. значение **до** записи
default-speed/pre-pause-restore, которая происходит чуть выше в том же
match'е (`try_apply_default_speed`/`try_restore_speed`, await'ится раньше).
Комментарий над `zh_resumed_kmh` уже утверждал, что Zone Hold "видит
восстановленную скорость, не crawl" — но код на самом деле не подхватывал
результат этой записи, только сырое поле из старого сэмпла.

В сценарии "watchdog restart во время ходьбы": на реконнекте лента реально
стоит на заводском crawl (0.5), `data.speed_kmh` при первом сэмпле = 0.5,
демон применяет `try_apply_default_speed` → лента реально едет на 2.6, но
Zone Hold всё равно стартует `Ramp { start_speed_kmh: 0.5, target_speed_kmh:
2.6 }` — 5-минутный линейный ramp *от* 0.5, хотя лента уже была на целевой
скорости. Первый correction tick (interval по умолчанию ~20с) записывает
`warmup_target_speed(0.5, 2.6, 21s, 300s) ≈ 0.64` поверх уже правильной 2.6 —
воспринимается как "рандомный сброс скорости".

## Фикс

`src/daemon.rs` — заведена `zh_effective_speed_kmh`, инициализируется тем же
сырым сэмплом, но обновляется каждый раз, когда `try_restore_speed`/
`try_apply_default_speed` в этом же тике реально записали новую скорость на
ленту (используется фактически применённое значение: `SpeedRestore::to_kmh`
или `applied` из `try_apply_default_speed`). `zh_resumed_kmh`, передаваемое в
`zone_hold_on_transition`, теперь читает `zh_effective_speed_kmh` вместо
устаревшего `data.speed_kmh`.

Итог: если в этом же тике лента уже была выставлена на дефолтную/
восстановленную скорость, Ramp стартует с неё же (`start == target`,
фактически no-op) вместо крауla.

## Область изменений

Только `src/daemon.rs` (`stream_with_presence`, ветки presence-transition).
`zone_hold.rs`/`warmup_target_speed` не тронуты — баг был в том, какое
значение туда передавалось, не в самой линейной интерполяции.

## Не тронуто

Соседний блок правок в этом же файле (Zone Hold engage не только с
`Unknown→Walking`, а с любого первого прихода в `Walking` при `phase==Off`;
Grace без условия `phase != Off`) — уже был в рабочем дереве до этой задачи,
не часть этого фикса, не трогается.

## Деплой

Требует `cargo build --release` + `scripts/install-daemon.sh` (перезапуск
LaunchAgent) — сборка сама по себе не подхватывается запущенным демоном.
