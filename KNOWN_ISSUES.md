# TaSQ --- известные проблемы и ограничения / Known Issues

**Язык / Language:** Русский ниже · English follows

## Русская версия

Этот список отражает известные на момент подготовки Preview 0.9.1
ограничения. Он может меняться по мере тестирования и разработки.

### 1. Dante Virtual Soundcard: высокая многоканальная нагрузка

**Статус: проблема производительности ещё не закрыта.**

В пользовательских тестах с Dante Virtual Soundcard наблюдалось
замедление воспроизведения при увеличении количества одновременно
работающих Audio Cue.

В последнем описанном тесте: - 4 Cue субъективно воспроизводились с
нормальной скоростью; - при 15 одновременно работающих Cue замедление
сохранялось.

Ранее проблема проявлялась при многоканальной конфигурации DVS. Поэтому
высоконагруженное воспроизведение через DVS пока нельзя считать
подтверждённо стабильным.

Полноценная проверка физического Dante signal path с подключённым
принимающим Dante-устройством не проводилась. Результаты DVS-тестов не
следует трактовать как подтверждение всей сетевой цепочки Dante.

**Рекомендация:** до использования DVS на реальном мероприятии проверьте
проект на целевой конфигурации, с тем же количеством каналов и
одновременно звучащих Cue.

### 2. Sparse Mixer ещё не интегрирован в рабочий Audio Engine

Прототип Sparse Mixer прошёл отдельные тесты на синтетических
аудиосигналах. Stage 4.1 Runtime Contract и Stage 4.2a Runtime Command
Pump прошли пользовательские unit-тесты. Stage 4.2b1 bounded Reaper
handoff прошёл Windows-проверку: 7/7 новых тестов; полная suite —
313 passed и 2 намеренно ignored. Stage 4.2b2 также принят после
Windows-проверки: Ownership 12/12; Reaper 7/7; Runtime Pump 12/12;
Runtime Contract 19/19; Sparse Mixer 11 passed, 1 ignored; полная suite —
325 passed, 0 failed, 2 ignored. Stage 4.2b3 также принят после
пользовательской Windows-проверки: Renderer 5/5; Runtime Pump 13/13;
Ownership 12/12; Reaper 7/7; Runtime Contract 19/19; Sparse Mixer
11 passed, 1 ignored; полная suite — 331 passed, 0 failed, 2 ignored.
Stage 4.2b4 Lifecycle & Stress Testing также принят после Windows-проверки:
Renderer 9/9; Ownership 15/15; Reaper 7/7; Runtime Pump 13/13; Runtime
Contract 19/19; Sparse Mixer 11 passed, 1 intentionally ignored; полная
suite — 340 discovered, 338 passed, 0 failed, 2 intentionally ignored.
`cargo fmt`, `cargo fmt --check`, `cargo check` и `cargo build` прошли.
Все эти модули подключаются только в `#[cfg(test)]` и не участвуют в
обычном воспроизведении TaSQ.

Stage 4.3a — Sparse Cue Control Bridge — подготовлен в test-only виде.
Он моделирует атомарные команды управления Cue для будущего Sparse
Renderer. Проверка на Windows ещё не выполнена; production playback path
не менялся, а проблема DVS остаётся открытой.

**Важно о количестве выходов:** 16 destinations — обязательный сценарий
проверки маршрутизации, а не планируемый программный предел. Искусственный
лимит в 16 выходов не закладывается. Будущая реализация должна
ориентироваться на фактическое число выходных каналов выбранного endpoint
и ограничения его драйвера/backend. При этом поддержка произвольного
числа выходов пока не подтверждена: конфигурации выше 16 ещё предстоит
проверить.

Следовательно:
- Sparse Mixer не исправляет текущую проблему DVS в этой версии;
- unit-тесты test-only runtime не подтверждают производительность
  реального ASIO/DVS playback;
- интеграция, проверка маршрутизации выше 16 выходов и аппаратные
  испытания остаются задачами разработки.

### 3. Межустройственная синхронизация

Синхронизация между независимыми аудиоустройствами с отдельными
аппаратными clock sources ещё не реализована как завершённая функция.

Не следует считать, что запуск Cue на разных устройствах гарантирует
долговременное совпадение фаз или отсутствие clock drift.

### 4. Совместимость аудиооборудования

TaSQ тестировалась на отдельных Windows-конфигурациях с ASIO, WASAPI и
DirectSound. Это не является сертификацией всех аудиоинтерфейсов,
драйверов, sample rates, buffer sizes и channel layouts.

Перед использованием на шоу проверьте именно ту конфигурацию, которая
будет использоваться на площадке.

### 5. Платформы

Текущая Preview-версия ориентирована на Windows. Готовая macOS-версия в
рамках этой версии не заявляется.

### 6. Функции, которых пока нет в Preview

Следующие возможности находятся в разработке или ещё не реализованы:

-   Random Playback в Playlist Group;
-   Infinite Loop для Playlist Group;
-   Crossfade внутри Playlist Group;
-   Undo / Redo;
-   Waveform Seek без остановки воспроизведения;
-   Automatic Silence Trim;
-   открытие `.tsq` двойным щелчком через Windows file association;
-   Single Instance и передача проекта уже запущенному экземпляру;
-   улучшенный запрос имени при Save as Bundle;
-   MIDI Time Code (MTC) Output Cue;
-   Экспорт Text Score / Cue Sheet в PDF и DOCX;
-   Редактирование пользовательских шаблонов Text Score;
-   Native DSP Effects (Reverb, Delay, Compressor, Gate, Limiter) и
    Cue/Master Insert / Send / Return;
-   Generic MIDI Map Mode для назначения MIDI-сообщений на фейдеры,
    потенциометры, кнопки, команды и параметры эффектов.

Состояние функций: [FEATURES.md](FEATURES.md).

**Текущее MIDI Learn:** в Preview имеется `Settings > MIDI Control`
с Learn для девяти команд приложения (GO, Stop, Pause, Resume,
Select Next/Previous); захват поддерживает Note On и CC button messages.
Универсальное назначение непрерывных параметров, FX controls и отдельный
MIDI Map Mode ещё не реализованы.

### 7. Как сообщить о новой проблеме

При создании Issue укажите:

1.  Версию TaSQ.
2.  Версию Windows.
3.  Модель аудиоинтерфейса.
4.  Тип драйвера и его версию.
5.  Sample rate, buffer size и количество выходных каналов.
6.  Количество одновременно воспроизводимых Audio Cue.
7.  Точные шаги для повторения проблемы.
8.  Диагностические логи, если они доступны.

Перед публикацией удалите из логов личные данные, локальные пути и
сведения о конфиденциальных проектах.

------------------------------------------------------------------------

## English Version

This list describes the limitations known at the time Preview 0.9.1 was
prepared. It may change as testing and development continue.

### 1. Dante Virtual Soundcard: High Multichannel Load

**Status: the performance issue remains unresolved.**

User tests with Dante Virtual Soundcard showed playback slowing down as
the number of simultaneously active Audio Cues increased.

In the latest reported test: - 4 Cues subjectively played at the
expected speed; - playback slowdown remained with 15 simultaneously
active Cues.

The issue had also appeared in earlier multichannel DVS tests. High-load
playback through DVS should therefore not yet be considered verified as
stable.

A complete test of the physical Dante signal path with a connected
receiving Dante device has not been performed. DVS test results should
not be treated as validation of the entire Dante network signal chain.

**Recommendation:** before using DVS in a live production, test the show
on the exact target configuration, with the same channel count and
number of simultaneously playing Cues.

### 2. Sparse Mixer Is Not Integrated into the Production Audio Engine

The Sparse Mixer prototype has passed separate tests using synthetic
audio signals. The Stage 4.1 Runtime Contract and Stage 4.2a Runtime
Command Pump have passed user-run unit tests. The Stage 4.2b1 bounded
Reaper handoff passed user-run Windows validation: 7/7 new tests, with
313 passed and 2 intentionally ignored in the full suite. Stage 4.2b2 was
also accepted after Windows validation: Ownership 12/12; Reaper 7/7;
Runtime Pump 12/12; Runtime Contract 19/19; Sparse Mixer 11 passed and
1 ignored; full suite 325 passed, 0 failed, and 2 ignored. `cargo fmt
--check`, `cargo check`, and `cargo build` succeeded. Stage 4.2b3 was
also accepted after user-run Windows validation: Renderer 5/5; Runtime
Pump 13/13; Ownership 12/12; Reaper 7/7; Runtime Contract 19/19; Sparse
Mixer 11 passed, 1 ignored; full suite 331 passed, 0 failed, 2 ignored.
Stage 4.2b4 Lifecycle & Stress Testing was also accepted after
user-run Windows validation: Renderer 9/9; Ownership 15/15; Reaper 7/7;
Runtime Pump 13/13; Runtime Contract 19/19; Sparse Mixer 11 passed and
1 intentionally ignored; full suite 340 discovered, 338 passed, 0 failed,
and 2 intentionally ignored. `cargo fmt`, `cargo fmt --check`, `cargo check`,
and `cargo build` succeeded. All these modules are included only in
`#[cfg(test)]` and do not participate in normal TaSQ playback.

Stage 4.3a — Sparse Cue Control Bridge — has been prepared in test-only
form. It models atomic Cue control commands for a future Sparse Renderer.
Windows validation has not yet been performed; the production playback
path is unchanged and the DVS issue remains open.

**Output-count clarification:** 16 destinations are a mandatory routing
validation scenario, not an intended software limit. No artificial
16-output cap is planned. The future implementation should follow the
actual output-channel count exposed by the selected endpoint and its
driver/backend. However, support for arbitrary output counts has not
been verified; configurations above 16 still require testing.

Therefore:
- Sparse Mixer does not fix the current DVS issue in this version;
- test-only runtime unit tests do not demonstrate performance of real
  ASIO/DVS playback;
- production integration, routing tests above 16 outputs, and hardware
  validation remain development tasks.

### 3. Inter-Device Synchronization

Synchronization between independent audio devices with separate hardware
clock sources is not yet implemented as a completed feature.

Starting Cues on different devices should not be assumed to guarantee
long-term phase alignment or freedom from clock drift.

### 4. Audio Hardware Compatibility

TaSQ has been tested on selected Windows configurations using ASIO,
WASAPI, and DirectSound. This is not certification of all audio
interfaces, drivers, sample rates, buffer sizes, or channel layouts.

Before using TaSQ in a show, test the exact configuration that will be
used at the venue.

### 5. Platforms

The current Preview version targets Windows. A finished macOS version is
not included in this release.

### 6. Features Not Yet Available in Preview

The following features are in development or have not yet been
implemented:

-   Random Playback in Playlist Group;
-   Infinite Loop for Playlist Group;
-   Crossfade inside Playlist Group;
-   Undo / Redo;
-   Waveform Seek without stopping playback;
-   Automatic Silence Trim;
-   opening `.tsq` files by double-click through Windows file
    association;
-   Single Instance and passing a project to an already running
    application;
-   improved project naming in Save as Bundle;
-   MIDI Time Code (MTC) Output Cue;
-   Text Score / Cue Sheet export to PDF and DOCX;
-   Editing custom Text Score templates;
-   Native DSP Effects (Reverb, Delay, Compressor, Gate, Limiter) and
    Cue/Master Insert / Send / Return;
-   Generic MIDI Map Mode for mapping MIDI messages to faders, knobs,
    buttons, commands, and effect parameters.

See [FEATURES.md](FEATURES.md) for feature status.

**Current MIDI Learn:** the Preview build includes `Settings > MIDI Control`
with Learn for nine application commands (GO, Stop, Pause, Resume,
Select Next/Previous); capture supports Note On and CC button messages.
Generic mapping of continuous parameters, FX controls, and a dedicated
MIDI Map Mode have not been implemented.

### 7. Reporting a New Issue

When creating an Issue, include:

1.  TaSQ version.
2.  Windows version.
3.  Audio interface model.
4.  Driver type and version.
5.  Sample rate, buffer size, and output channel count.
6.  Number of simultaneously playing Audio Cues.
7.  Exact steps to reproduce the problem.
8.  Diagnostic logs, if available.

Before publishing logs, remove personal data, local file paths, and
information about confidential projects.
