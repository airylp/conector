#!/bin/sh
# Compila src/java/ConectorHelper.java a src/java/auxiliar.jar (bytecode Java 8,
# sin comprimir: lleva dentro el .java para que el test `auxiliar_al_dia`
# compruebe que el jar corresponde al código). Ejecutar tras cambiar el .java.
# JDK: $JAVA_HOME, si no entorno/jdk/*, si no el del PATH.
set -eu
cd "$(dirname "$0")/.."
if [ -n "${JAVA_HOME:-}" ]; then BIN="$JAVA_HOME/bin/"
elif ls -d entorno/jdk/*/bin >/dev/null 2>&1; then BIN="$PWD/$(ls -d entorno/jdk/*/bin | head -1)/"
else BIN=""; fi
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
"${BIN}javac" -nowarn -encoding UTF-8 --release 8 -d "$TMP" src/java/ConectorHelper.java 2>&1 | grep -v '^warning\|^Nota\|^Note\|warnings\?$' || true
cp src/java/ConectorHelper.java "$TMP/"
# Fechas fijas: el mismo código da el mismo jar.
find "$TMP" -exec touch -t 202601010000 {} +
(cd "$TMP" && "${BIN}jar" --create --no-compress --file auxiliar.jar --date=2026-01-01T00:00:00Z ConectorHelper*.class ConectorHelper.java)
mv "$TMP/auxiliar.jar" src/java/auxiliar.jar
echo "src/java/auxiliar.jar: $(wc -c < src/java/auxiliar.jar) bytes"
