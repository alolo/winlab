# TaSQ --- история изменений / Changelog

**Язык / Language:** Русский ниже · English follows

## Русская версия

В этом файле публикуются пользовательские изменения между версиями TaSQ.

### Unreleased — внутренняя разработка Stage 4.2b (не часть релиза 0.9.1)

-   Stage 4.2b1 Bounded Reaper Handoff прошёл пользовательскую проверку
    на Windows: 7/7 новых тестов; полная suite — 313 passed и 2
    намеренно ignored.
-   Stage 4.2b2 Move-only Cue Source Ownership принят после Windows-проверки:
    Ownership 12/12; Reaper 7/7; Runtime Pump 12/12; Runtime Contract 19/19;
    Sparse Mixer 11 passed, 1 ignored; полная suite — 325 passed, 0 failed,
    2 ignored. `cargo fmt --check`, `cargo check` и `cargo build` прошли.
-   Stage 4.2b3 Sparse Endpoint Renderer принят после пользовательской
    Windows-проверки: Renderer 5/5; Runtime Pump 13/13; Ownership 12/12;
    Reaper 7/7; Runtime Contract 19/19; Sparse Mixer 11 passed, 1 ignored;
    полная suite — 331 passed, 0 failed, 2 ignored. Синтетические тесты
    подтвердили sample-level parity и fan-out 16/32/64/128.
-   Stage 4.2b4 Lifecycle & Stress Testing принят после Windows-проверки:
    Renderer 9/9; Ownership 15/15; Reaper 7/7; Runtime Pump 13/13;
    Runtime Contract 19/19; Sparse Mixer 11 passed, 1 intentionally ignored;
    полная suite — 340 discovered, 338 passed, 0 failed, 2 intentionally
    ignored. `cargo fmt`, `cargo fmt --check`, `cargo check` и `cargo build`
    завершились успешно. Этап остаётся test-only и не меняет production
    playback path.
-   В Stage 4.2a command pump добавлен лимит обработки 64 команд на
    endpoint-frame boundary; PlayBatch atomicity остаётся отдельной задачей.
-   Зафиксирован будущий Stage 5: собственные DSP-эффекты Reverb, Delay,
    Compressor, Gate и Limiter; Cue/Master Insert, Send и Return; сохранение
    Reverb/Delay tails после естественного EOF; универсальный MIDI Map Mode
    для контроллеров разных производителей. Это планы, а не функции
    Preview 0.9.1. Gated Reverb и MIDI panning не входят в scope.
-   Это внутренние этапы разработки: пользовательские функции,
    production playback path и поведение DVS не изменялись. Не считать
    test-only прототипы выпущенными функциями.

### 0.9.1 Preview --- подготовка к выпуску

**Статус:** кандидат на Preview Release; окончательная сборка и
регрессионная проверка ещё должны быть завершены.

#### Документация релиза

-   Подготовлено описание проекта и пользовательских возможностей.
-   Разделены проверенные функции, известные ограничения и функции в
    разработке.
-   Зафиксирована целевая версия пакета `0.9.1`.

#### Уточнение документации по маршрутизации (без изменения программы)

-   Зафиксировано, что 16 выходов — обязательный сценарий проверки
    будущего Sparse Mixer, а не программный максимум.
-   В проектируемой архитектуре не планируется искусственный предел
    в 16 destinations; фактическое число выходов должно определяться
    возможностями endpoint и драйвера/backend.
-   Это уточнение проектного намерения, а не реализованная или
    подтверждённая функция Preview 0.9.1. Проверки выше 16 выходов и
    production-интеграция ещё впереди.

#### Важно о составе версии

Изменение номера версии само по себе не меняет поведение Audio Engine.
Подготовленный version-bump patch предназначен для обновления метаданных
версии и не содержит изменений логики воспроизведения.

Полный список изменений непосредственно в программном коде для 0.9.1
необходимо окончательно сверить с предыдущей опубликованной версией
перед публикацией GitHub Release. До этой сверки не следует приписывать
версии новые функции только на основании того, что они уже присутствуют
в текущей тестовой кодовой базе.

#### Известные ограничения

-   Производительность DVS при высокой многоканальной нагрузке требует
    дальнейшей проверки.
-   Sparse Mixer ещё не подключён к production Audio Engine.
-   Межустройственная синхронизация и ряд функций Playlist находятся в
    разработке.
-   MIDI Time Code (MTC) Output Cue добавлен в план будущих реализаций;
    в версии 0.9.1 он не реализован.
-   Экспорт Text Score / Cue Sheet в PDF/DOCX и редактирование
    пользовательских шаблонов добавлены в план будущих реализаций;
    в версии 0.9.1 они не реализованы.

Подробности: [KNOWN_ISSUES.md](KNOWN_ISSUES.md).

### 0.9.0

История изменений версии 0.9.0 будет дополнена после сверки с её
опубликованным исходным кодом и release notes. Неподтверждённые
изменения здесь намеренно не перечисляются.

------------------------------------------------------------------------

## English Version

This file records user-facing changes between TaSQ releases.

### Unreleased — Stage 4.2b Internal Development (Not Part of Release 0.9.1)

-   Stage 4.2b1 Bounded Reaper Handoff passed user-run Windows validation:
    7/7 new tests; the full suite reported 313 passed and 2 intentionally
    ignored.
-   Stage 4.2b2 Move-only Cue Source Ownership was accepted after Windows
    validation: Ownership 12/12; Reaper 7/7; Runtime Pump 12/12; Runtime
    Contract 19/19; Sparse Mixer 11 passed, 1 ignored; full suite 325 passed,
    0 failed, 2 ignored. `cargo fmt --check`, `cargo check`, and `cargo build`
    succeeded.
-   Stage 4.2b3 Sparse Endpoint Renderer was accepted after user-run
    Windows validation: Renderer 5/5; Runtime Pump 13/13; Ownership 12/12;
    Reaper 7/7; Runtime Contract 19/19; Sparse Mixer 11 passed, 1 ignored;
    full suite 331 passed, 0 failed, 2 ignored. Synthetic tests confirmed
    sample-level parity and fan-out 16/32/64/128.
-   Stage 4.2b4 Lifecycle & Stress Testing was accepted after Windows
    validation: Renderer 9/9; Ownership 15/15; Reaper 7/7; Runtime Pump
    13/13; Runtime Contract 19/19; Sparse Mixer 11 passed and 1
    intentionally ignored; full suite 340 discovered, 338 passed,
    0 failed, and 2 intentionally ignored. `cargo fmt`, `cargo fmt --check`,
    `cargo check`, and `cargo build` succeeded. The stage remains test-only
    and does not change the production playback path.
-   The Stage 4.2a command pump now has a 64-command budget per endpoint
    frame boundary; atomic PlayBatch behavior remains a separate task.
-   Stage 5 has been added to the roadmap: native Reverb, Delay,
    Compressor, Gate, and Limiter; Cue/Master Insert, Send, and Return;
    Reverb/Delay tails after natural EOF; and a generic MIDI Map Mode for
    controllers from different manufacturers. These are plans, not
    Preview 0.9.1 features. Gated Reverb and MIDI panning are out of scope.
-   These are internal development steps: no user-facing feature,
    production playback path, or DVS behavior has changed. Test-only
    prototypes are not shipped features.

### 0.9.1 Preview --- Release Preparation

**Status:** candidate Preview Release; the final build and regression
testing still need to be completed.

#### Release Documentation

-   Added a project overview and user-facing feature description.
-   Separated tested features, known limitations, and features in
    development.
-   Set the target package version to `0.9.1`.

#### Routing Documentation Clarification (No Runtime Change)

-   Clarified that 16 outputs are a mandatory validation scenario for the
    future Sparse Mixer, not a software maximum.
-   The planned architecture does not introduce an artificial
    16-destination cap; the actual output count should follow endpoint
    and driver/backend capabilities.
-   This records design intent only. It is not a shipped or verified
    Preview 0.9.1 feature. Testing above 16 outputs and production
    integration remain pending.

#### Important Notes About This Version

Changing the version number alone does not change Audio Engine behavior.
The prepared version-bump patch updates version metadata only and does
not modify playback logic.

The complete list of code changes for 0.9.1 must be reconciled with the
previously published version before the GitHub Release is published.
Until that comparison is complete, do not attribute new features to this
release solely because they are present in the current development
source.

#### Known Limitations

-   DVS performance under high multichannel load requires further
    testing.
-   Sparse Mixer has not yet been integrated into the production Audio
    Engine.
-   Inter-device synchronization and several Playlist features are still
    in development.
-   MIDI Time Code (MTC) Output Cue has been added to the future
    development plan; it is not implemented in version 0.9.1.
-   Text Score / Cue Sheet export to PDF/DOCX and editable user
    templates have been added to the future development plan; they are
    not implemented in version 0.9.1.

See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for details.

### 0.9.0

The 0.9.0 change history will be completed after checking its published
source code and release notes. Unverified changes are intentionally not
listed here.
