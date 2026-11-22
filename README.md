# TaSQ --- Theatre Show Cue

**Язык / Language:** Русский ниже · English follows

## Русская версия

**TaSQ** --- программа для управления Cue и последовательностями событий
во время театральных, концертных и других live-мероприятий.

**Версия:** `0.9.1 Preview`\
**Платформа:** Windows\
**Статус:** предварительная версия для тестирования и сбора обратной
связи.

TaSQ развивается как инструмент для управления аудиовоспроизведением,
группами событий, MIDI- и OSC-командами. Программа находится в активной
разработке. Перед использованием на ответственном мероприятии необходимо
проверить её на собственном оборудовании и на копии рабочего проекта.

### Возможности

-   Управление аудиособытиями (Audio Cue).
-   Группы и последовательности событий.
-   Режимы Normal, Playlist, All Cues и Timeline.
-   Auto-Continue и Auto-Follow для автоматического перехода между
    событиями и группами.
-   Одновременное воспроизведение нескольких Audio Cue.
-   Infinite Loop, Pause / Resume, Stop и StopFade.
-   Громкость, маршрутизация и автоматизация параметров воспроизведения.
-   MIDI Output и OSC Output.
-   MIDI Time Code (MTC) Output Cue запланирован, но в версии 0.9.1
    Preview ещё не реализован.
-   Экспорт текстовой партитуры в PDF/DOCX и редактирование шаблонов экспорта запланированы, но пока не реализованы.

Подробное состояние функций и границы проведённого тестирования см. в
[FEATURES.md](FEATURES.md).

### Состояние разработки

TaSQ 0.9.1 планируется как **Preview Release**, а не как универсально
подтверждённая production-stable версия.

Известные ограничения и незавершённые функции перечислены отдельно:

-   [KNOWN_ISSUES.md](KNOWN_ISSUES.md) --- известные проблемы и
    ограничения.
-   [FEATURES.md](FEATURES.md) --- что уже работает и что находится в
    разработке.
-   [CHANGELOG.md](CHANGELOG.md) --- история изменений по версиям.

В частности, остаётся проблема производительности при высокой
многоканальной нагрузке через Dante Virtual Soundcard. Sparse Mixer,
который должен помочь с эффективностью многоканального суммирования,
пока не интегрирован в рабочий Audio Engine.

### Перед использованием

1.  Сначала проверьте программу на тестовом проекте.
2.  Проверьте используемый аудиоинтерфейс, драйвер, sample rate, размер
    буфера и маршрутизацию.
3.  Проверьте запуск, остановку, Pause / Resume, Loop и автоматизацию.
4.  Сохраняйте резервные копии проектов.
5.  Не используйте Preview-сборку как единственную систему
    воспроизведения критически важного шоу без предварительных испытаний
    на целевой конфигурации.

Тестирование отдельных моделей аудиоинтерфейсов не означает
гарантированную совместимость со всеми устройствами и версиями
драйверов.

### Сообщить об ошибке

При сообщении об ошибке укажите:

-   версию TaSQ и Windows;
-   модель аудиоинтерфейса и версию драйвера;
-   тип драйвера: ASIO, WASAPI или DirectSound;
-   sample rate, размер буфера и число выходных каналов;
-   количество одновременно воспроизводимых Audio Cue;
-   последовательность действий, после которой возникла проблема;
-   диагностические логи, если они доступны.

Не публикуйте конфиденциальные шоу-проекты, личные данные и
аудиоматериалы, на распространение которых у вас нет прав.

### Лицензия и распространение

Условия использования и распространения определяются лицензией,
опубликованной в репозитории. Если файл лицензии отсутствует, не следует
предполагать, что исходный код разрешено свободно использовать, изменять
или распространять.

------------------------------------------------------------------------

## English Version

**TaSQ (Theatre Show Cue)** is an application for managing cues and
event sequences during theatre productions, concerts, and other live
events.

**Version:** `0.9.1 Preview`\
**Platform:** Windows\
**Status:** Preview build intended for testing and feedback.

TaSQ is being developed as a tool for audio playback, cue groups, and
MIDI/OSC control. The application is under active development. Before
using it in a live production, test it with your own equipment and a
copy of your show project.

### Features

-   Audio Cue management and playback.
-   Cue groups and event sequences.
-   Normal, Playlist, All Cues, and Timeline modes.
-   Auto-Continue and Auto-Follow for automatic transitions between cues
    and groups.
-   Simultaneous playback of multiple Audio Cues.
-   Infinite Loop, Pause / Resume, Stop, and StopFade.
-   Playback volume, routing, and parameter automation.
-   MIDI Output and OSC Output.
-   MIDI Time Code (MTC) Output Cue is planned but is not implemented in
    Preview 0.9.1.
-   Text Score export to PDF/DOCX and editable export templates are planned but not yet implemented.

See [FEATURES.md](FEATURES.md) for the feature status and the scope of
testing performed.

### Development Status

TaSQ 0.9.1 is intended as a **Preview Release**, not as a universally
validated production-stable version.

Known limitations and unfinished features are documented separately:

-   [KNOWN_ISSUES.md](KNOWN_ISSUES.md) --- known issues and limitations.
-   [FEATURES.md](FEATURES.md) --- available features and features in
    development.
-   [CHANGELOG.md](CHANGELOG.md) --- version history.

One known issue is performance under high multichannel load with Dante
Virtual Soundcard. The Sparse Mixer, which is intended to improve
multichannel mixing efficiency, has not yet been integrated into the
production Audio Engine.

### Before Using TaSQ

1.  Start with a test project.
2.  Verify your audio interface, driver, sample rate, buffer size, and
    routing.
3.  Test GO, Stop, Pause / Resume, Loop, and automation.
4.  Keep backup copies of your projects.
5.  Do not use a Preview build as the sole playback system for a
    critical show without testing it on the exact target configuration.

Testing selected audio interfaces does not guarantee compatibility with
every device or driver version.

### Reporting an Issue

When reporting a problem, please include:

-   TaSQ and Windows versions;
-   audio interface model and driver version;
-   driver type: ASIO, WASAPI, or DirectSound;
-   sample rate, buffer size, and output channel count;
-   number of simultaneously playing Audio Cues;
-   steps required to reproduce the issue;
-   TaSQ diagnostic logs, if available.

Do not publish confidential show files, personal information, or audio
materials that you do not have permission to distribute.

### License and Distribution

Use and distribution are governed by the license published in the
repository. If no license file is present, do not assume that the source
code may be freely used, modified, or redistributed.
