#!/usr/bin/env bash
# Ставит replay-rs в домашний каталог пользователя: бинарь, пункт меню и
# (по желанию) автозапуск. Права root не нужны.
#
#   ./packaging/install.sh              — бинарь и пункт меню
#   ./packaging/install.sh --autostart  — плюс автозапуск при входе в сессию
#   ./packaging/install.sh --uninstall  — убрать всё установленное
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
BIN="$BIN_DIR/replay-rs"

uninstall() {
    rm -f "$BIN" "$APP_DIR/replay-rs.desktop" "$AUTOSTART_DIR/replay-rs.desktop"
    update-desktop-database "$APP_DIR" 2>/dev/null || true
    echo "удалено: бинарь, пункт меню, автозапуск"
    echo "настройки и клипы не тронуты"
}

case "${1:-}" in
    --uninstall) uninstall; exit 0 ;;
esac

# Собираем всегда: сборка инкрементальная, и если всё актуально, это
# секунды. Раньше сборка шла, только когда бинаря не было вовсе, и скрипт
# молча ставил устаревший release, собранный до последних правок.
echo "собираю release…"
(cd "$REPO" && cargo build --release)

install -Dm755 "$REPO/target/release/replay-rs" "$BIN"

install -d "$APP_DIR"
sed "s|^Exec=replay-rs$|Exec=$BIN|" "$REPO/packaging/replay-rs.desktop" \
    > "$APP_DIR/replay-rs.desktop"
update-desktop-database "$APP_DIR" 2>/dev/null || true

echo "бинарь:     $BIN"
echo "пункт меню: $APP_DIR/replay-rs.desktop"

if [ "${1:-}" = "--autostart" ]; then
    install -d "$AUTOSTART_DIR"
    sed -e "s|^Exec=replay-rs$|Exec=$BIN --headless|" \
        -e "s|^Name=replay-rs$|Name=replay-rs (фоновая запись)|" \
        "$REPO/packaging/replay-rs.desktop" > "$AUTOSTART_DIR/replay-rs.desktop"
    echo "автозапуск: $AUTOSTART_DIR/replay-rs.desktop"
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo
       echo "ВНИМАНИЕ: $BIN_DIR не в PATH, поэтому 'replay-rs --save' из"
       echo "терминала не найдётся. Либо добавьте каталог в PATH, либо"
       echo "используйте полный путь: $BIN --save" ;;
esac
