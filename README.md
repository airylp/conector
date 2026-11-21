# conector

Interfaz ligera en Rust (egui/eframe) para probar conexiones JDBC, guardarlas
y compartirlas entre equipos. Los drivers JDBC son los jar de siempre: un
pequeño proceso Java auxiliar habla JDBC y la interfaz habla con él por
stdin/stdout.

## Uso

    cargo build --release
    ./target/release/conector                     # interfaz
    ./target/release/conector --listar            # id, nombre, URL
    ./target/release/conector --probar ID         # 0 si conecta (para scripts)

Requisitos: Java 8+ para ejecutar los drivers. Con JDK, el auxiliar se compila
una vez (`javac --release 8`); con solo JRE hace falta Java 11+ JDK o un
`.class` ya compilado en la carpeta del auxiliar.

## Catálogo compartible

Cada conexión es un archivo de texto `<catálogo>/<id>.conexion`:

    nombre=Oracle facturación
    grupo=Producción
    url=jdbc:oracle:thin:@//db-prod:1521/FACT
    usuario=lectura
    driver=oracle.jdbc.OracleDriver
    jar=drivers/ojdbc11.jar
    timeout=15
    prop.oracle.net.CONNECT_TIMEOUT=5000

- Pon el catálogo en una carpeta compartida (unidad de red, repositorio git,
  Syncthing…) desde Ajustes o con `CONECTOR_CATALOGO`: los demás equipos ven
  los cambios en unos 2 segundos.
- Los jar relativos se resuelven desde el catálogo; sin `jar=` se cargan todos
  los de `<catálogo>/drivers`. Así el catálogo lleva también los drivers.
- Las contraseñas **no** van al catálogo: «Recordar en este equipo» las guarda
  en `secretos.txt` de la configuración local (permisos 600).
- Escritura atómica (temporal + rename) y aviso si otro equipo cambió la
  conexión mientras la editabas.
