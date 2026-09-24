# Windows 10 / 11 (x86_64)

Windows-backend добавлен в исходники. Проверены типы и условная компиляция
для Windows на Linux; нативная сборка MSVC и ручной прогон на Windows
пока не подтверждены. Первая Windows-сборка — pre-release v0.2.0-beta.1:
workflow `Windows` собирает её после успешных тестов и выкладывает в Releases.
Linux-релиз — v0.1.0.

## Возможности

- Захват основного или выбранного монитора через Direct3D 11 / DXGI.
- H.264 через программный `x264enc`; видеобуфер и сохранение MP4 без перекодирования.
- Системный звук через WASAPI loopback, микрофон через WASAPI.
- Нативный интерфейс, превью, открытие файлов и папок через Windows Shell.
- Хоткей `Ctrl+Alt+S`, меню в области уведомлений: сохранение, папка, выход.
- Команды `--save`, `--quit`, `--headless`; установка и автозапуск для пользователя.

В Windows крестик **завершает приложение и запись**. Чтобы продолжать запись,
сверните окно обычной кнопкой или используйте `--headless`. Открытие окна из
фонового режима пока требует завершения и повторного запуска.

## Зависимости

1. Windows 10 или 11, x86_64, интерактивная графическая сессия с поддержкой DXGI.
2. GStreamer **MSVC x86_64**, Runtime, установка **Complete**. Для сборки из
   исходников также нужны Development-файлы той же версии.
   Workflow использует 1.26.10; более новые версии требуют отдельной проверки.
3. Для сборки — Rust stable с `x86_64-pc-windows-msvc`, Visual Studio Build Tools
   с компонентами C++ и Windows SDK.

[Установка GStreamer для Windows](https://gstreamer.freedesktop.org/documentation/installing/on-windows.html).
Не смешивайте MSVC и MinGW либо x86 и x86_64.

## Сборка из исходников

Откройте PowerShell в каталоге проекта (при необходимости Developer PowerShell
из Visual Studio). Пример для стандартного пути GStreamer:

```powershell
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = 'C:\gstreamer\1.0\msvc_x86_64\'
$env:PATH = "$env:GSTREAMER_1_0_ROOT_MSVC_X86_64\bin;$env:PATH"
$env:PKG_CONFIG_PATH = "$env:GSTREAMER_1_0_ROOT_MSVC_X86_64\lib\pkgconfig"

rustup default stable-msvc
cargo build --release --locked
.\target\release\replay-rs.exe
```

Проверьте плагины, если захват или запись не запускаются:

```powershell
gst-inspect-1.0 d3d11screencapturesrc
gst-inspect-1.0 wasapi2src
gst-inspect-1.0 x264enc
gst-inspect-1.0 avenc_aac
gst-inspect-1.0 mp4mux
gst-inspect-1.0 jpegenc
```

Указанные элементы должны быть доступны в той же среде, из которой запускается
replay-rs. DLL GStreamer не включены в ZIP приложения.

## Установка

Установщик `packaging/install-windows.ps1` работает без прав администратора и
ставит приложение только для текущего пользователя:

- файлы — в `%LOCALAPPDATA%\Programs\replay-rs` (`replay-rs.exe`,
  launcher `run-windows.ps1` и `gstreamer-path.txt` с путём к GStreamer);
- ярлык **replay-rs** — в меню «Пуск»;
- с `-Autostart` — ещё ярлык в папке «Автозагрузка» с запуском `--headless`.

Launcher добавляет `bin` GStreamer в PATH только для процесса replay-rs;
системный и пользовательский PATH не меняются. DLL GStreamer в приложение не
копируются, поэтому GStreamer Runtime должен оставаться установленным.

### Из ZIP релиза

1. Установите GStreamer MSVC x86_64 Runtime (см. «Зависимости»).
2. Скачайте со страницы Releases архив `replay-rs-<версия>-windows-x86_64.zip`
   и `SHA256SUMS` в один каталог. Сборки с каждого push лежат там же в
   виде артефакта `replay-rs-windows-x86_64` во вкладке Actions (нужен вход в
   GitHub); в них архив называется `replay-rs-dev-windows-x86_64.zip`.
3. Сверьте контрольную сумму и распакуйте архив (пример для v0.2.0-beta.1):

   ```powershell
   $zip = '.\replay-rs-v0.2.0-beta.1-windows-x86_64.zip'
   (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
   Get-Content .\SHA256SUMS
   Expand-Archive $zip -DestinationPath .\replay-rs
   Set-Location .\replay-rs
   ```

4. Запустите установщик из распакованного каталога:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\install-windows.ps1
   ```

Структура архива (`target\release\replay-rs.exe`, `packaging\*.ps1`)
совпадает с деревом исходников, поэтому указывать путь к бинарю не нужно.
ZIP собирается CI, но не подписан и пока не проверен вручную на Windows.

### Из собственной сборки

После `cargo build --release --locked` выполните в корне проекта ту же команду:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\install-windows.ps1
```

### Параметры установщика

| Параметр | Назначение |
| --- | --- |
| `-GStreamerRoot <путь>` | Каталог GStreamer, например `'D:\GStreamer\1.0\msvc_x86_64'`. По умолчанию — `GSTREAMER_1_0_ROOT_MSVC_X86_64`, иначе `C:\gstreamer\1.0\msvc_x86_64`. Установщик проверяет наличие `bin\gstreamer-1.0-0.dll`. |
| `-Binary <путь>` | Другой `replay-rs.exe`. По умолчанию — `..\target\release\replay-rs.exe` относительно скрипта. |
| `-Autostart` | Добавить запуск без окна (`--headless`) после входа в Windows. |
| `-Uninstall` | Удалить приложение и ярлыки. |

`-ExecutionPolicy Bypass` действует только на этот запуск PowerShell и не меняет
политику системы. Ярлыки тоже запускают launcher с этим параметром.

### Обновление

Завершите работающий экземпляр (иначе `replay-rs.exe` заблокирован) и
запустите установщик новой версии с теми же параметрами:

```powershell
& "$env:LOCALAPPDATA\Programs\replay-rs\run-windows.ps1" --quit
powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\install-windows.ps1
```

Файлы и ярлыки перезаписываются; настройки и клипы не затрагиваются. Если
GStreamer переустановлен в другой каталог, повторите установку с новым
`-GStreamerRoot`: путь хранится в `gstreamer-path.txt` и сам не обновляется.

### Автозапуск

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\install-windows.ps1 -Autostart
```

Повторная установка без `-Autostart` ярлык автозагрузки **не удаляет**.
Чтобы отключить автозапуск, удалите `replay-rs.lnk` из папки, которую открывает
`shell:startup` (Win+R), или выполните `-Uninstall` и установите заново без
`-Autostart`.

### Удаление

Завершите приложение, затем:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\install-windows.ps1 -Uninstall
```

Удаляются ярлыки «Пуск» и автозагрузки и файлы приложения. Сохраняются
настройки, кеш, клипы (см. «Данные и ограничения») и установленный GStreamer;
пустой каталог `%LOCALAPPDATA%\Programs\replay-rs` можно удалить вручную.

### Если приложение не запускается

Ярлык запускает PowerShell со скрытым окном, поэтому ошибки не видны.
Запустите launcher из PowerShell, чтобы увидеть вывод:

```powershell
& "$env:LOCALAPPDATA\Programs\replay-rs\run-windows.ps1"
```

- `Install GStreamer MSVC x86_64 Runtime (Complete), or specify -GStreamerRoot.` —
  GStreamer не найден: установите Runtime или передайте `-GStreamerRoot`.
- `Release binary not found...` — нет `target\release\replay-rs.exe`: соберите
  проект, запустите установщик из распакованного ZIP или передайте `-Binary`.
- Окно не появляется, ошибка DLL или плагина — проверьте путь в
  `gstreamer-path.txt` и плагины через `gst-inspect-1.0` (см. «Сборка из
  исходников»). Нужна установка GStreamer **Complete** той же разрядности (MSVC x86_64).
- Приложение уже запущено в фоне — проверьте значок в области уведомлений
  или выполните launcher с `--quit`.

## Управление

Команды ниже выполняются из каталога сборки при настроенном PATH GStreamer:

```powershell
.\replay-rs.exe --monitor -1    # основной монитор
.\replay-rs.exe --monitor 0     # первый монитор в нумерации DXGI
.\replay-rs.exe --headless
.\replay-rs.exe --save
.\replay-rs.exe --quit
```

Монитор также выбирается в настройках окна. Применение очищает буфер и
перезапускает захват. Индексы DXGI могут отличаться от номеров в настройках
дисплея Windows. `--mode desktop` выбирает захват Windows явно;
`--mode test` заменяет видео тестовым источником, но не отключает звук.

Команды установленной версии можно выполнять через launcher, который сам
находит DLL:

```powershell
& "$env:LOCALAPPDATA\Programs\replay-rs\run-windows.ps1" --save
& "$env:LOCALAPPDATA\Programs\replay-rs\run-windows.ps1" --quit
```

`Ctrl+Alt+S` регистрируется автоматически и пока не переназначается внутри
приложения. Если комбинацию заняла другая программа, интерфейс покажет ошибку;
используйте кнопку, трей или `--save`. `--configure-shortcut` показывает справку.

## Данные и ограничения

- Настройки: `%APPDATA%\replay-rs\config.json`.
- Кеш: `%LOCALAPPDATA%\replay-rs\cache\thumbs`.
- Клипы по умолчанию: `%USERPROFILE%\Videos\replays` (папка может быть изменена).
- Блокировка экземпляра: `%APPDATA%\replay-rs\replay-rs.pid`.
- Один экземпляр на пользователя. Команды доступны в той же графической сессии
  и при совместимом уровне привилегий; запускайте приложение без администратора.
- Захват защищённого контента, экрана UAC, заблокированного рабочего стола и
  работа через RDP не гарантируются.
- Программный H.264 нагружает CPU. При необходимости уменьшите FPS/битрейт.
  Аппаратные NVENC, QSV и AMF пока не интегрированы.
- Звук использует устройства Windows по умолчанию. После смены устройства
  может потребоваться перезапуск. Микрофон требует системного разрешения.
- Смена фактического аудиокодека требует перезапуска приложения.

## Проверка на Windows

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

На Windows тест `software_recording_saves_unicode_paths_and_previews` запускается
обычным `cargo test`: проверяет тестовое видео → H.264 → MP4 → JPEG, включая
пробелы и кириллицу в пути. На Linux этот тест запускается отдельно с `--ignored`
и требует `x264enc`. Он не доказывает работу DXGI, WASAPI или глобального хоткея.

Перед объявлением Windows-поддержки проверенной выполните на Windows 10 и 11:

1. Запуск из «Пуска», захват основного и дополнительного монитора.
2. Сохранение кнопкой, хоткеем, из трея и командой; воспроизведение MP4 со звуком.
3. Микрофон, отсутствие звука в системе, смена устройства вывода.
4. Работа после изменения настроек, выход из окна/трея/CLI и повторный запуск.
5. Повторный запуск при уже работающем экземпляре, конфликт хоткея,
   каталог с кириллицей и пробелами, автозапуск после входа в Windows.

Описание API: [DXGI capture](https://gstreamer.freedesktop.org/documentation/d3d11/d3d11screencapturesrc.html),
[WASAPI](https://gstreamer.freedesktop.org/documentation/wasapi2/wasapi2src.html),
[RegisterHotKey](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerhotkey).
