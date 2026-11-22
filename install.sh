#!/usr/bin/env sh
# conector: instalar, actualizar o desinstalar con un solo comando.
# (Probar y compartir conexiones JDBC; los drivers necesitan Java 8+, y si no
#  lo hay la app descarga uno portátil.)
#
#     curl -fsSL https://raw.githubusercontent.com/airylp/conector/main/install.sh | sh
#
#   Pregunta qué hacer: si ya está instalado ofrece actualizar o desinstalar; si
#   no, instalarlo. También se puede indicar directamente (sin preguntas):
#     … | sh -s -- instalar        (instalar o actualizar)
#     … | sh -s -- desinstalar     (pregunta si borrar también la configuración)
#
#   macOS: crea conector.app (con ícono) en /Applications (o ~/Applications sin permiso).
#   Linux: instala ~/.local/bin/conector con ícono y entrada en el menú de aplicaciones.
#
# Variables opcionales: CONECTOR_REPO (usuario/repo), CONECTOR_REF (rama o tag),
#   CONECTOR_DESTINO (carpeta de instalación), CONECTOR_SOLO_BINARIO=1 (solo el
#   ejecutable en la carpeta actual, sin instalar nada).
set -eu

REPO="${CONECTOR_REPO:-airylp/conector}"
REF="${CONECTOR_REF:-main}"
ACCION="${1:-}"
OS="$(uname)"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
# Mensajes en el idioma elegido: m "español" "english" "français".
m() { case "${IDIOMA:-es}" in en) printf '%s' "$2" ;; fr) printf '%s' "$3" ;; *) printf '%s' "$1" ;; esac; }

# Preguntas: se leen de la terminal (el script llega por la tubería de curl).
pregunta() {
    # ¿Hay terminal de verdad? (abrir /dev/tty falla si se ejecuta sin ella).
    if (: < /dev/tty) 2>/dev/null; then
        printf '%s %s ' "$1" "$(m '[s/N]' '[y/N]' '[o/N]')" > /dev/tty
        read -r r < /dev/tty || r=""
        case "$r" in s|S|si|sí|Sí|y|Y|yes|o|O|oui) return 0 ;; esac
    fi
    return 1
}

if [ "$OS" = "Darwin" ]; then
    # Con CONECTOR_DESTINO solo se toca esa carpeta.
    APP_DIRS="${CONECTOR_DESTINO:-/Applications $HOME/Applications}"
    CONFIG="$HOME/Library/Application Support/conector"
    CACHE="$HOME/Library/Caches/conector-compilacion"
else
    BIN_DIR="${CONECTOR_DESTINO:-$HOME/.local/bin}"
    DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
    ICON_SIZES="16 32 64 128 256 512"
    CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/conector"
    CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/conector-compilacion"
fi

hay_terminal() { (: < /dev/tty) 2>/dev/null; }
leer() { # leer "pregunta" → respuesta en $r
    printf '%s' "$1" > /dev/tty
    read -r r < /dev/tty || r=""
}

# ------------------------------------------------------------ idioma
# CONECTOR_IDIOMA=es|en|fr, o el ya guardado en los ajustes de la app; si no
# hay ninguno y hay terminal, se pregunta (y se guarda para la app).
IDIOMA="${CONECTOR_IDIOMA:-}"
GUARDAR_IDIOMA="$IDIOMA"
[ -n "$IDIOMA" ] || IDIOMA="$(sed -n 's/^idioma=//p' "$CONFIG/ajustes.txt" 2>/dev/null | head -n 1)"
if [ -z "$IDIOMA" ]; then
    case "${LC_ALL:-${LANG:-}}" in en*) IDIOMA=en ;; fr*) IDIOMA=fr ;; *) IDIOMA=es ;; esac
    if [ -z "$ACCION" ] && hay_terminal; then
        printf 'Idioma / Language / Langue\n  1) Español\n  2) English\n  3) Français\n' > /dev/tty
        case "$IDIOMA" in en) def=2 ;; fr) def=3 ;; *) def=1 ;; esac
        leer "[1-3] (Enter = $def): "
        case "${r:-$def}" in 2) IDIOMA=en ;; 3) IDIOMA=fr ;; *) IDIOMA=es ;; esac
        GUARDAR_IDIOMA="$IDIOMA"
    fi
fi
case "$IDIOMA" in es|en|fr) ;; *) IDIOMA=es ;; esac
export IDIOMA
# Deja el idioma en los ajustes de la app (sin tocar lo demás).
guardar_idioma() {
    [ -n "$GUARDAR_IDIOMA" ] || return 0
    mkdir -p "$CONFIG"
    f="$CONFIG/ajustes.txt"
    if [ -f "$f" ]; then grep -v '^idioma=' "$f" > "$f.tmp" || true; mv "$f.tmp" "$f"; fi
    echo "idioma=$IDIOMA" >> "$f"
}

# ------------------------------------------------------------ ¿qué hacer?
# Sin acción indicada y con terminal, se pregunta. (La actualización desde la
# app indica «instalar» y no pregunta nada.)
if [ -z "$ACCION" ]; then
    ACCION=instalar
    if hay_terminal; then
        instalada=""
        if [ "$OS" = "Darwin" ]; then
            for d in $APP_DIRS; do
                [ -d "$d/conector.app" ] && { instalada="$d/conector.app"; break; }
            done
            [ -n "$instalada" ] && version=$(defaults read "$instalada/Contents/Info" CFBundleVersion 2>/dev/null || echo "?")
        elif [ -x "$BIN_DIR/conector" ]; then
            instalada="$BIN_DIR/conector"
            version="?"
        fi
        if [ -n "$instalada" ]; then
            {
                echo "$(m "conector ya está instalado: $instalada (versión $version)" "conector is already installed: $instalada (version $version)" "conector est déjà installé : $instalada (version $version)")"
                echo "  1) $(m 'Actualizar a la última versión' 'Update to the latest version' 'Mettre à jour vers la dernière version')"
                echo "  2) $(m 'Desinstalar' 'Uninstall' 'Désinstaller')"
                echo "  3) $(m 'Cancelar' 'Cancel' 'Annuler')"
            } > /dev/tty
            leer "$(m 'Elige [1-3] (Enter = 1): ' 'Choose [1-3] (Enter = 1): ' 'Choisissez [1-3] (Entrée = 1) : ')"
            case "${r:-1}" in
                1) ACCION=instalar ;;
                2) ACCION=desinstalar ;;
                *) m "Cancelado." "Cancelled." "Annulé."; echo; exit 0 ;;
            esac
        else
            destino="/Applications"
            [ "$OS" = "Darwin" ] || destino="$BIN_DIR"
            leer "$(m "Se va a instalar conector en ${CONECTOR_DESTINO:-$destino}. ¿Continuar? [S/n] " "conector will be installed in ${CONECTOR_DESTINO:-$destino}. Continue? [Y/n] " "conector va être installé dans ${CONECTOR_DESTINO:-$destino}. Continuer ? [O/n] ")"
            case "$r" in n|N|no|No|non|Non) m "Cancelado." "Cancelled." "Annulé."; echo; exit 0 ;; esac
        fi
    fi
fi

# ------------------------------------------------------------ desinstalar
if [ "$ACCION" = "desinstalar" ] || [ "$ACCION" = "--desinstalar" ]; then
    if [ "$OS" = "Darwin" ]; then
        for d in $APP_DIRS; do
            if [ -d "$d/conector.app" ]; then
                [ -n "${CONECTOR_DESTINO:-}" ] || osascript -e 'tell application id "io.github.airylp.conector" to quit' >/dev/null 2>&1 || true
                /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u "$d/conector.app" >/dev/null 2>&1 || true
                rm -rf "${d:?}/conector.app" && info "$(m "Quitada $d/conector.app" "Removed $d/conector.app" "Supprimée : $d/conector.app")"
            fi
        done
    else
        for f in "$BIN_DIR/conector" "$DATA/applications/conector.desktop"; do
            if [ -e "$f" ]; then rm -f "$f" && info "$(m "Quitado $f" "Removed $f" "Supprimé : $f")"; fi
        done
        for t in $ICON_SIZES; do rm -f "$DATA/icons/hicolor/${t}x${t}/apps/conector.png"; done
        command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$DATA/applications" 2>/dev/null || true
    fi
    rm -rf "$CACHE"
    if [ -d "$CONFIG" ]; then
        echo "$(m 'Configuración' 'Settings' 'Réglages'): $CONFIG"
        ls -1 "$CONFIG" | sed 's/^/    /'
        if pregunta "$(m '¿Borrar también la configuración (ajustes, contraseñas recordadas, Java portátil)? El catálogo compartido no se toca.' 'Also delete the settings (settings, remembered passwords, portable Java)? The shared catalog is not touched.' 'Supprimer aussi la configuration (réglages, mots de passe retenus, Java portable) ? Le catalogue partagé n’est pas touché.')"; then
            rm -rf "$CONFIG" && info "$(m 'Configuración borrada' 'Settings deleted' 'Réglages supprimés')"
        else
            m "Configuración conservada." "Settings kept." "Réglages conservés."; echo
        fi
    fi
    info "$(m 'conector desinstalado' 'conector uninstalled' 'conector désinstallé')"
    exit 0
fi
[ "$ACCION" = "instalar" ] || [ "$ACCION" = "actualizar" ] || fail "$(m "acción desconocida: $ACCION (usa instalar o desinstalar)" "unknown action: $ACCION (use instalar or desinstalar)" "action inconnue : $ACCION (utilisez instalar ou desinstalar)")"

# ------------------------------------------------------------ requisitos
command -v curl >/dev/null 2>&1 || fail "$(m 'se necesita curl' 'curl is required' 'curl est nécessaire')"
command -v tar  >/dev/null 2>&1 || fail "$(m 'se necesita tar' 'tar is required' 'tar est nécessaire')"
command -v cc   >/dev/null 2>&1 || fail "$(m 'se necesita un compilador/enlazador C (cc).' 'a C compiler/linker (cc) is required.' 'un compilateur/éditeur de liens C (cc) est nécessaire.')
  Debian/Ubuntu: sudo apt install build-essential
  Fedora:        sudo dnf install gcc
  Arch:          sudo pacman -S base-devel
  macOS:         xcode-select --install"

# Rust: usa el instalado o lo instala con rustup en ~/.cargo (sin sudo).
PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
    info "$(m 'Rust no encontrado; instalando con rustup en ~/.cargo' 'Rust not found; installing it with rustup in ~/.cargo' 'Rust introuvable ; installation avec rustup dans ~/.cargo')"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
fi
export PATH
info "$(m 'Usando' 'Using' 'Utilisation de') $(cargo --version)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

info "$(m 'Descargando' 'Downloading' 'Téléchargement de') $REPO@$REF"
curl -fsSL "https://github.com/$REPO/archive/$REF.tar.gz" | tar -xz -C "$TMP" --strip-components=1
# Versión exacta (commit), grabada en el programa.
CONECTOR_COMMIT="$(curl -fsSL -H 'Accept: application/vnd.github.sha' "https://api.github.com/repos/$REPO/commits/$REF" 2>/dev/null | cut -c1-7 || true)"
export CONECTOR_COMMIT
# Caché de compilación: las actualizaciones siguientes tardan mucho menos.
CARGO_TARGET_DIR="$CACHE"
export CARGO_TARGET_DIR

if [ "${CONECTOR_SOLO_BINARIO:-0}" = 1 ]; then
    info "$(m 'Compilando (la primera vez tarda unos minutos)' 'Building (the first time takes a few minutes)' 'Compilation (la première fois prend quelques minutes)')"
    (cd "$TMP" && cargo build --release --locked)
    cp "$CACHE/release/conector" "./conector"
    chmod +x ./conector
    info "$(m 'Listo' 'Done' 'Terminé'): $(pwd)/conector"
    exit 0
fi

if [ "$OS" = "Darwin" ]; then
    info "$(m 'Compilando y creando conector.app (la primera vez tarda unos minutos)' 'Building conector.app (the first time takes a few minutes)' 'Compilation de conector.app (la première fois prend quelques minutes)')"
    # Si conector está abierto, pregunta (en la terminal) si cerrarlo y reabrirlo;
    # desde «Buscar actualizaciones» de la propia app no se cierra (CONECTOR_NO_CERRAR=1).
    sh "$TMP/empaquetar-mac.sh" instalar --si ${CONECTOR_NO_CERRAR:+--no-cerrar} ${CONECTOR_DESTINO:+--destino "$CONECTOR_DESTINO"}
    guardar_idioma
    info "$(m 'Listo. Abre conector desde Launchpad o Spotlight (Cmd+Espacio, «conector»).' 'Done. Open conector from Launchpad or Spotlight (Cmd+Space, “conector”).' 'Terminé. Ouvrez conector depuis Launchpad ou Spotlight (Cmd+Espace, « conector »).')"
else
    info "$(m 'Compilando (la primera vez tarda unos minutos)' 'Building (the first time takes a few minutes)' 'Compilation (la première fois prend quelques minutes)')"
    (cd "$TMP" && cargo build --release --locked)
    mkdir -p "$BIN_DIR" "$DATA/applications"
    # Copia y renombra: el ejecutable en uso sigue funcionando hasta reiniciarlo.
    cp "$CACHE/release/conector" "$BIN_DIR/.conector.nuevo"
    chmod +x "$BIN_DIR/.conector.nuevo"
    mv -f "$BIN_DIR/.conector.nuevo" "$BIN_DIR/conector"
    # Iconos en varios tamaños; cada uno se escribe aparte y se renombra para que
    # GNOME nunca lea (y guarde en caché) un PNG a medio escribir.
    if "$BIN_DIR/conector" --iconset "$TMP/iconos"; then
        for t in $ICON_SIZES; do
            case $t in 64) f=icon_32x32@2x.png ;; *) f="icon_${t}x${t}.png" ;; esac
            d="$DATA/icons/hicolor/${t}x${t}/apps"
            mkdir -p "$d" && cp "$TMP/iconos/$f" "$d/.conector.png.nuevo" && mv -f "$d/.conector.png.nuevo" "$d/conector.png"
        done
        touch "$DATA/icons/hicolor" 2>/dev/null || true
    fi
    cat > "$DATA/applications/conector.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=conector
Comment=Probar y compartir conexiones JDBC
Comment[en]=Test and share JDBC connections
Comment[fr]=Tester et partager des connexions JDBC
Exec=$BIN_DIR/conector
Icon=conector
Terminal=false
Categories=Development;Database;
StartupWMClass=conector
DESKTOP
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$DATA/applications" 2>/dev/null || true
    guardar_idioma
    info "$(m "Instalado: $BIN_DIR/conector (y en el menú de aplicaciones)" "Installed: $BIN_DIR/conector (and in the applications menu)" "Installé : $BIN_DIR/conector (et dans le menu des applications)")"
    case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "   $(m "Nota: $BIN_DIR no está en tu PATH; añádelo para escribir «conector» en la terminal." "Note: $BIN_DIR is not in your PATH; add it to run “conector” from the terminal." "Remarque : $BIN_DIR n’est pas dans votre PATH ; ajoutez-le pour lancer « conector » depuis le terminal.")" ;; esac
fi
# La caché de compilación acelera las actualizaciones, pero cargo no borra lo
# viejo: si pasa de 1 GB se vacía (la próxima compilación empieza de cero).
if [ -d "$CACHE" ] && [ "$(du -sk "$CACHE" | cut -f1)" -gt 1048576 ]; then
    rm -rf "$CACHE" && info "$(m 'Caché de compilación vaciada (pasaba de 1 GB)' 'Build cache emptied (it was over 1 GB)' 'Cache de compilation vidé (il dépassait 1 Go)')"
fi
echo "   $(m 'Para actualizar más adelante: ⚙ Ajustes → «Buscar actualizaciones…», o vuelve a ejecutar este comando.' 'To update later: ⚙ Settings → “Check for updates…”, or run this command again.' 'Pour mettre à jour plus tard : ⚙ Réglages → « Rechercher des mises à jour… », ou relancez cette commande.')"
# Java hace falta para los drivers JDBC; si no lo hay, la app ofrece uno portátil.
if ! command -v java >/dev/null 2>&1 && [ -z "${JAVA_HOME:-}" ]; then
    echo "   $(m 'Java no encontrado: al abrir conector, pulsa «⬇ Descargar Java portátil».' 'Java not found: when you open conector, press “⬇ Download portable Java”.' 'Java introuvable : à l’ouverture de conector, appuyez sur « ⬇ Télécharger Java portable ».')"
fi
