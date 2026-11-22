# TaSQ --- история изменений / Changelog

**Язык / Language:** Русский ниже · English follows

## Русская версия

В этом файле публикуются пользовательские изменения между версиями TaSQ.

### 0.9.1 Preview --- подготовка к выпуску

**Статус:** кандидат на Preview Release; окончательная сборка и
регрессионная проверка ещё должны быть завершены.

#### Документация релиза

-   Подготовлено описание проекта и пользовательских возможностей.
-   Разделены проверенные функции, известные ограничения и функции в
    разработке.
-   Зафиксирована целевая версия пакета `0.9.1`.

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

Подробности: [KNOWN_ISSUES.md](KNOWN_ISSUES.md).

### 0.9.0

История изменений версии 0.9.0 будет дополнена после сверки с её
опубликованным исходным кодом и release notes. Неподтверждённые
изменения здесь намеренно не перечисляются.

------------------------------------------------------------------------

## English Version

This file records user-facing changes between TaSQ releases.

### 0.9.1 Preview --- Release Preparation

**Status:** candidate Preview Release; the final build and regression
testing still need to be completed.

#### Release Documentation

-   Added a project overview and user-facing feature description.
-   Separated tested features, known limitations, and features in
    development.
-   Set the target package version to `0.9.1`.

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

See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for details.

### 0.9.0

The 0.9.0 change history will be completed after checking its published
source code and release notes. Unverified changes are intentionally not
listed here.
