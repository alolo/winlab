# TaSQ --- возможности и состояние функций

**Язык / Language:** Русский ниже · English follows

## Русская версия

Этот документ описывает состояние функций TaSQ для пользователей.
Отметка о проверке означает, что функция проходила испытания в
конкретных сценариях. Она не является гарантией работы на любом
компьютере, аудиоинтерфейсе или драйвере.

### 1. Реализовано и проверено в отдельных сценариях

#### Управление Cue и группами

-   [x] Создание и воспроизведение Audio Cue.
-   [x] Работа с группами событий.
-   [x] Режимы Normal, Playlist, All Cues и Timeline.
-   [x] Auto-Continue и Auto-Follow на событиях Group в проверенных
    режимах.
-   [x] Выделение следующего события, которое оператор должен запустить
    вручную, после автоматической цепочки группы.
-   [x] Сохранение и повторное открытие тестовых проектов.
-   [x] Stop отдельной Cue и Stop All.

#### Аудиовоспроизведение

-   [x] Одновременное воспроизведение нескольких Audio Cue.
-   [x] Infinite Loop для аудиособытий.
-   [x] Pause / Resume для отдельных событий и нескольких событий.
-   [x] StopFade для плавной остановки; отсутствие слышимых щелчков
    подтверждено пользователем в проверенных тестах.
-   [x] Volume Automation во время воспроизведения.
-   [x] Time/Pitch и автоматизация в проверенных сценариях.
-   [x] Изменение маршрутизации и уровней во время воспроизведения в
    проверенных сценариях.
-   [x] Глобальная громкость Master Volume с целочисленной шкалой
    0--100%.
-   [x] Индивидуальный уровень Cue Trim в dB.

#### MIDI и OSC

-   [x] MIDI Output проверен на Allen & Heath dLive S5000.
-   [x] OSC Output проверен с Resolume Arena.

#### Аудиодрайверы и оборудование

В пользовательских тестах использовались ASIO, WASAPI и DirectSound, а
также несколько аудиоинтерфейсов, включая Focusrite Scarlett 2i2, TASCAM
US-4x4, TASCAM US-366, Sound Blaster Omni, Gigaport EX и Dante Virtual
Soundcard.

Наличие устройства в списке тестирования не означает, что все его
режимы, драйверы и конфигурации полностью проверены.

#### Длительный тест Loop

В одном из длительных тестов одновременно воспроизводились 12 Audio Cue
на TASCAM US-366 ASIO при 48 kHz и буфере 256 frames. Продолжительность
составила около 4 часов 23 минут. В логах зафиксированы 764 общие
границы Loop на каждую Cue; application-level PCM prefetch underruns и
callback over-budget события в этом тесте не зарегистрированы.

Это результат конкретного теста, а не гарантия аналогичной
производительности на любой системе.

### 2. В разработке

Следующие возможности не следует считать доступными в текущей
Preview-сборке:

-   [ ] **Sparse Mixer / Dynamic PCM Mixer** --- более эффективное
    суммирование аудио для многоканальных конфигураций. Этапы Runtime
    Contract (4.1) и Runtime Command Pump (4.2a) прошли отдельные
    пользовательские unit-тесты, но остаются test-only и не подключены
    к обычному воспроизведению. Stage 4.2b1 прошёл Windows-проверку:
    7/7 новых тестов; полная suite — 313 passed, 2 намеренно ignored.
    Stage 4.2b2 принят после Windows-проверки: Ownership 12/12; Reaper
    7/7; Runtime Pump 12/12; Runtime Contract 19/19; Sparse Mixer
    11 passed, 1 ignored; полная suite — 325 passed, 0 failed, 2 ignored.
    Stage 4.2b3 также принят после Windows-проверки: Renderer 5/5;
    Runtime Pump 13/13; Ownership 12/12; Reaper 7/7; Runtime Contract
    19/19; Sparse Mixer 11 passed, 1 ignored; полная suite — 331 passed,
    0 failed, 2 ignored. `cargo fmt --check`, `cargo check` и `cargo build`
    прошли. Stage 4.2b4 Lifecycle & Stress Testing принят после
    пользовательской Windows-проверки: Renderer 9/9; Ownership 15/15;
    Reaper 7/7; Runtime Pump 13/13; Runtime Contract 19/19; Sparse Mixer
    11 passed, 1 intentionally ignored; полная suite — 340 discovered,
    338 passed, 0 failed, 2 intentionally ignored. `cargo fmt`,
    `cargo fmt --check`, `cargo check` и `cargo build` прошли. Все этапы
    остаются test-only и не подключены к обычному воспроизведению.
    **Масштабирование Routing:** 16 output destinations — обязательный
    тестовый сценарий, а не программный лимит. В Stage 4.2b3 синтетические тесты 16 / 32 / 64 / 128 destinations
    прошли; это не подтверждение аппаратной поддержки. Искусственный
    предел в 16 выходов не планируется; фактическое число должно
    определяться возможностями endpoint/backend.
-   [ ] **До 256 одновременно звучащих Audio Cue на одном endpoint** ---
    целевой показатель будущего движка. Это не реализованный лимит
    текущей версии и не ограничение общего количества Cue в проекте.
    Одна Cue считается одним audio slot независимо от числа выходов,
    на которые она маршрутизируется; число Cue slots не ограничивает
    количество output destinations.
-   [ ] **Межустройственная синхронизация** --- согласование
    воспроизведения между независимыми аудиоустройствами.
-   [ ] **Playlist Random** --- случайный порядок воспроизведения Audio
    Cue в Playlist Group.
-   [ ] **Playlist Group Infinite Loop** --- бесконечное повторение
    содержимого Playlist Group.
-   [ ] **Crossfade в Playlist Group** --- плавный переход между
    аудиособытиями.
-   [ ] **Undo / Redo** --- отмена и повтор изменений проекта.
-   [ ] **Waveform Seek во время воспроизведения** --- переход к
    выбранной позиции без остановки Cue.
-   [ ] **Automatic Silence Trim** --- автоматическое пропускание тишины
    в начале и конце аудиофайла с возможностью включения и отключения.
-   [ ] **Windows file association** --- открытие проекта `.tsq` двойным
    щелчком.
-   [ ] **Single Instance** --- передача открываемого проекта уже
    запущенному экземпляру TaSQ.
-   [ ] **Улучшение Save as Bundle** --- единое имя папки проекта и
    файла проекта.
-   [ ] **MIDI Time Code (MTC) Output Cue** --- отдельный тип события
    для передачи MIDI Time Code внешним устройствам и программам.
    Планируется поддержка Quarter Frame, Full Frame/Locate, выбора frame
    rate и Timecode Offset. Точное поведение транспорта и границы первой
    версии будут определены на этапе проектирования.
-   [ ] **Text Score / Cue Sheet Export** --- экспорт текстовой
    партитуры проекта в PDF и DOCX.
-   [ ] **Custom Export Templates** --- редактирование пользовательских
    шаблонов текстовой партитуры, включая выбор выводимых полей и
    настройки оформления. Точный состав настроек будет определён при
    проектировании.
-   [ ] **Native DSP Effects & Routing** --- собственные Reverb, Delay,
    Compressor, Gate и Limiter; Cue Insert / Send / FX Return и Master
    Insert / Send / Return. Reverb/Delay tails должны продолжать звучать
    после естественного окончания исходной Cue. Gated Reverb не входит
    в согласованный scope; обязательная зависимость от VST не планируется.
-   [ ] **Generic MIDI Learn / MIDI Map Mode** --- отдельный режим
    назначения MIDI-сообщений на фейдеры, потенциометры, кнопки, команды
    и параметры эффектов. Поддержка любого MIDI-контроллера, доступного
    системе как MIDI input; конкретные типы сообщений и расширенная
    обработка NRPN/RPN/SysEx будут определены при проектировании.
    MIDI-панорамирование не планируется: физические выходы назначаются
    через I/O Routing.

### 3. Что пока не заявляется как доступная возможность

-   Полноценная межустройственная синхронизация независимых
    аудиоустройств.
-   Гарантированная работа любого количества Cue через Dante Virtual
    Soundcard.
-   Производственный Sparse Mixer.
-   MIDI Time Code (MTC) Output.
-   Экспорт Text Score / Cue Sheet в PDF и DOCX.
-   Редактирование пользовательских шаблонов Text Score.
-   Native DSP Effects и Cue/Master FX routing.
-   Generic MIDI Map Mode для параметров и элементов интерфейса.
-   macOS-версия.

### 4. Как читать статусы

  -----------------------------------------------------------------------
  Статус                              Значение
  ----------------------------------- -----------------------------------
  Реализовано и проверено             Функция прошла указанные тестовые
                                      сценарии

  В разработке                        Функция запланирована или находится
                                      в процессе реализации

  Не заявляется                       Функция пока отсутствует либо её
                                      работа не подтверждена
  -----------------------------------------------------------------------

Для известных ограничений и рекомендаций по тестированию см.
[KNOWN_ISSUES.md](KNOWN_ISSUES.md).

------------------------------------------------------------------------

## English Version

This document describes the user-facing status of TaSQ features. A
checked item means that the feature has been tested in specific
scenarios. It does not guarantee operation on every computer, audio
interface, or driver.

### 1. Implemented and Tested in Specific Scenarios

#### Cue and Group Management

-   [x] Create and play Audio Cues.
-   [x] Work with cue groups.
-   [x] Normal, Playlist, All Cues, and Timeline modes.
-   [x] Group Auto-Continue and Auto-Follow in the tested modes.
-   [x] Select the next cue that requires manual operator action after a
    group's automatic sequence.
-   [x] Save and reopen test projects.
-   [x] Stop an individual Cue and Stop All.

#### Audio Playback

-   [x] Simultaneous playback of multiple Audio Cues.
-   [x] Infinite Loop for audio events.
-   [x] Pause / Resume for individual cues and multiple cues.
-   [x] StopFade for smooth stopping; the user reported no audible
    clicks in the tested scenarios.
-   [x] Volume Automation during playback.
-   [x] Time/Pitch and automation in the tested scenarios.
-   [x] Routing and level changes during playback in the tested
    scenarios.
-   [x] Global Master Volume with an integer 0--100% scale.
-   [x] Individual Cue Trim level in dB.

#### MIDI and OSC

-   [x] MIDI Output tested with an Allen & Heath dLive S5000.
-   [x] OSC Output tested with Resolume Arena.

#### Audio Drivers and Hardware

User testing included ASIO, WASAPI, and DirectSound, as well as several
audio interfaces, including Focusrite Scarlett 2i2, TASCAM US-4x4,
TASCAM US-366, Sound Blaster Omni, Gigaport EX, and Dante Virtual
Soundcard.

The presence of a device in this test list does not mean that every
operating mode, driver, or configuration has been fully tested.

#### Long-Duration Loop Test

In one long-duration test, 12 Audio Cues played simultaneously on a
TASCAM US-366 ASIO interface at 48 kHz with a 256-frame buffer. The test
lasted approximately 4 hours and 23 minutes. Logs recorded 764 shared
loop boundaries per Cue; no application-level PCM prefetch underruns or
callback over-budget events were recorded during that test.

This is a result from one specific test, not a guarantee of equivalent
performance on every system.

### 2. In Development

The following features should not be considered available in the current
Preview build:

-   [ ] **Sparse Mixer / Dynamic PCM Mixer** --- more efficient audio
    summing for multichannel configurations. The Runtime Contract (4.1)
    and Runtime Command Pump (4.2a) have passed separate user-run unit
    tests, but remain test-only and are not connected to normal playback.
    Stage 4.2b1 passed user-run Windows validation: 7/7 new tests, with
    313 passed and 2 intentionally ignored in the full suite. Stage 4.2b2
    was accepted after Windows validation: Ownership 12/12; Reaper 7/7;
    Runtime Pump 12/12; Runtime Contract 19/19; Sparse Mixer 11 passed
    and 1 ignored; full suite 325 passed, 0 failed, and 2 ignored.
    `cargo fmt --check`, `cargo check`, and `cargo build` succeeded.
    Stage 4.2b3 was also accepted after user-run Windows validation:
    Renderer 5/5; Runtime Pump 13/13; Ownership 12/12; Reaper 7/7;
    Runtime Contract 19/19; Sparse Mixer 11 passed, 1 ignored; full suite
    331 passed, 0 failed, 2 ignored. Stage 4.2b4 Lifecycle & Stress
    Testing was accepted after user-run Windows validation: Renderer 9/9;
    Ownership 15/15; Reaper 7/7; Runtime Pump 13/13; Runtime Contract
    19/19; Sparse Mixer 11 passed and 1 intentionally ignored; the full
    suite reported 340 discovered, 338 passed, 0 failed, and 2
    intentionally ignored. `cargo fmt`, `cargo fmt --check`, `cargo check`,
    and `cargo build` succeeded. All these stages remain test-only and
    are not connected to normal playback. **Routing scalability:** 16 output
    destinations are a mandatory test scenario, not a software limit.
    Stage 4.2b3 synthetic tests for 16 / 32 / 64 / 128 destinations passed;
    this does not establish hardware support. No artificial 16-output cap
    is intended; the actual count should follow endpoint/backend
    capabilities.
-   [ ] **Up to 256 simultaneously active Audio Cues on one endpoint**
    --- a target for the future engine. This is not an implemented limit
    in the current version and does not limit the total number of Cues
    in a project. One Cue occupies one audio slot regardless of how many
    output destinations it uses; Cue-slot capacity does not cap routing
    fan-out.
-   [ ] **Inter-device synchronization** --- synchronization of playback
    across independent audio devices.
-   [ ] **Playlist Random** --- randomized Audio Cue order in a Playlist
    Group.
-   [ ] **Playlist Group Infinite Loop** --- repeat the contents of a
    Playlist Group indefinitely.
-   [ ] **Playlist Crossfade** --- smooth transitions between audio
    events.
-   [ ] **Undo / Redo** --- undo and restore project edits.
-   [ ] **Waveform Seek during playback** --- jump to a selected
    position without stopping the Cue.
-   [ ] **Automatic Silence Trim** --- automatically skip silence at the
    beginning and end of an audio file, with an enable/disable setting.
-   [ ] **Windows file association** --- open a `.tsq` project by
    double-clicking it.
-   [ ] **Single Instance** --- pass an opened project to an already
    running TaSQ instance.
-   [ ] **Improved Save as Bundle** --- use the same name for the
    project folder and project file.
-   [ ] **MIDI Time Code (MTC) Output Cue** --- a dedicated cue type for
    sending MIDI Time Code to external devices and applications. Planned
    scope includes Quarter Frame, Full Frame/Locate, frame-rate
    selection, and Timecode Offset. Transport behavior and the exact
    scope of the first implementation will be defined during design.
-   [ ] **Text Score / Cue Sheet Export** --- export the project's text
    score to PDF and DOCX.
-   [ ] **Custom Export Templates** --- edit user-defined text-score
    templates, including selected output fields and formatting options.
    The exact settings will be defined during design.
-   [ ] **Native DSP Effects & Routing** --- built-in Reverb, Delay,
    Compressor, Gate, and Limiter; Cue Insert / Send / FX Return and
    Master Insert / Send / Return. Reverb/Delay tails should continue
    after the source Cue reaches natural EOF. Gated Reverb is outside
    the agreed scope; a VST dependency is not planned for the initial
    native-effects implementation.
-   [ ] **Generic MIDI Learn / MIDI Map Mode** --- a dedicated mapping
    mode for assigning MIDI messages to faders, knobs, buttons, commands,
    and effect parameters. It should work with any controller exposed
    by the system as a MIDI input device. The exact message matrix and
    advanced NRPN/RPN/SysEx handling will be defined during design.
    MIDI panning is not planned: physical output assignment remains in
    I/O Routing.

### 3. Features Not Currently Available

-   Full synchronization between independent audio devices.
-   Guaranteed operation with any number of Cues through Dante Virtual
    Soundcard.
-   Production Sparse Mixer.
-   MIDI Time Code (MTC) Output.
-   Text Score / Cue Sheet export to PDF and DOCX.
-   Editing custom Text Score templates.
-   Native DSP Effects and Cue/Master FX routing.
-   Generic MIDI Map Mode for parameters and UI controls.
-   macOS version.

### 4. Status Definitions

  -----------------------------------------------------------------------
  Status                              Meaning
  ----------------------------------- -----------------------------------
  Implemented and tested              The feature passed the listed test
                                      scenarios

  In development                      The feature is planned or currently
                                      being implemented

  Not available                       The feature is not present or its
                                      operation has not been verified
  -----------------------------------------------------------------------

For known limitations and testing recommendations, see
[KNOWN_ISSUES.md](KNOWN_ISSUES.md).
