// Proceso auxiliar de conector: habla JDBC en nombre de la interfaz en Rust.
//
// Protocolo por stdin/stdout, una línea por mensaje (UTF-8), campos separados
// por TAB y escapados (\\ \t \n \r; un campo «\N» es NULL):
//   petición:  orden <TAB> id <TAB> clave=valor ...
//   respuesta: tipo  <TAB> id <TAB> campos ...
// Órdenes: probar, abrir, consulta, cancelar, cerrar, salir.
// Cada petición corre en su propio hilo: una conexión colgada no bloquea las demás.
// Compatible con Java 8 (los drivers antiguos suelen exigirlo).

import java.io.*;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.sql.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicLong;

public final class ConectorHelper {
    static final Writer OUT = new BufferedWriter(new OutputStreamWriter(new FileOutputStream(FileDescriptor.out), StandardCharsets.UTF_8), 1 << 16);
    static final Map<String, ClassLoader> LOADERS = new ConcurrentHashMap<String, ClassLoader>();
    static final Map<Long, Connection> SESSIONS = new ConcurrentHashMap<Long, Connection>();
    static final Map<String, Statement> RUNNING = new ConcurrentHashMap<String, Statement>();
    static final Map<String, ExecutorService> QUEUES = new HashMap<String, ExecutorService>();
    static final AtomicLong NEXT_SESSION = new AtomicLong(1);
    /** Máximo de caracteres por celda que se envían (ahorra memoria en la interfaz). */
    static final int MAX_CELL = 4096;

    public static void main(String[] args) throws Exception {
        // Lo que impriman los drivers no debe mezclarse con el protocolo.
        System.setOut(System.err);
        ExecutorService pool = Executors.newCachedThreadPool(new ThreadFactory() {
            public Thread newThread(Runnable r) {
                Thread t = new Thread(r, "peticion");
                t.setDaemon(true);
                return t;
            }
        });
        send("listo", "0", "java=" + System.getProperty("java.version"), "vm=" + System.getProperty("java.vm.name"));
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        String line;
        while ((line = in.readLine()) != null) {
            if (line.isEmpty()) continue;
            final List<String> f = split(line);
            if (f.get(0).equals("salir")) break;
            Runnable job = new Runnable() {
                public void run() { handle(f); }
            };
            // Una conexión JDBC no admite dos órdenes a la vez: las de una misma
            // sesión van a su cola, en orden de llegada. «cancelar» nunca espera.
            String s = f.get(0).equals("cancelar") ? null : field(f, "sesion");
            if (s == null) {
                pool.execute(job);
            } else {
                ExecutorService q = QUEUES.get(s);
                if (q == null) {
                    q = Executors.newSingleThreadExecutor(new ThreadFactory() {
                        public Thread newThread(Runnable r) {
                            Thread t = new Thread(r, "sesion");
                            t.setDaemon(true);
                            return t;
                        }
                    });
                    QUEUES.put(s, q);
                }
                q.execute(job);
                if (f.get(0).equals("cerrar")) {
                    QUEUES.remove(s);
                    q.shutdown(); // termina tras lo pendiente
                }
            }
        }
        for (Connection c : SESSIONS.values()) {
            try { c.close(); } catch (Throwable ignored) { }
        }
        System.exit(0);
    }

    static String field(List<String> f, String key) {
        for (int i = 2; i < f.size(); i++) {
            String kv = f.get(i);
            if (kv != null && kv.startsWith(key + "=")) return kv.substring(key.length() + 1);
        }
        return null;
    }

    static void handle(List<String> f) {
        String cmd = f.get(0);
        String id = f.size() > 1 ? f.get(1) : "0";
        Map<String, String> a = new HashMap<String, String>();
        List<String> jars = new ArrayList<String>();
        Properties props = new Properties();
        for (int i = 2; i < f.size(); i++) {
            String kv = f.get(i);
            if (kv == null) continue;
            int eq = kv.indexOf('=');
            if (eq < 0) continue;
            String k = kv.substring(0, eq), v = kv.substring(eq + 1);
            if (k.equals("jar")) jars.add(v);
            else if (k.startsWith("prop.")) props.setProperty(k.substring(5), v);
            else a.put(k, v);
        }
        try {
            if (cmd.equals("probar")) test(id, a, jars, props);
            else if (cmd.equals("abrir")) open(id, a, jars, props);
            else if (cmd.equals("consulta")) query(id, a);
            else if (cmd.equals("meta")) meta(id, a);
            else if (cmd.equals("lote")) batch(id, a, f);
            else if (cmd.equals("editable")) editable(id, a);
            else if (cmd.equals("editar")) edit(id, a, f);
            else if (cmd.equals("cancelar")) cancel(id, a);
            else if (cmd.equals("cerrar")) close(id, a);
            else send("error", id, "mensaje=orden desconocida: " + cmd);
        } catch (Throwable e) {
            sendError(id, e);
        }
    }

    // ---------- conexión ----------

    static Connection connect(Map<String, String> a, List<String> jars, Properties props) throws Exception {
        String url = a.containsKey("url") ? a.get("url") : "";
        if (a.containsKey("usuario")) props.setProperty("user", a.get("usuario"));
        if (a.containsKey("clave")) props.setProperty("password", a.get("clave"));
        int timeout = a.containsKey("timeout") ? Integer.parseInt(a.get("timeout")) : 15;
        DriverManager.setLoginTimeout(timeout);
        ClassLoader loader = loader(jars);
        String cls = a.get("driver");
        Driver driver = null;
        if (cls != null && !cls.isEmpty()) {
            driver = (Driver) Class.forName(cls, true, loader).getDeclaredConstructor().newInstance();
        } else {
            // Sin clase: el primer driver de los jar (META-INF/services) que acepte la URL.
            Iterator<Driver> it = ServiceLoader.load(Driver.class, loader).iterator();
            for (int tries = 0; driver == null && tries < 64; tries++) {
                try {
                    if (!it.hasNext()) break;
                    Driver d = it.next();
                    if (d.acceptsURL(url)) driver = d;
                } catch (Throwable ignored) { } // un driver roto no impide probar los demás
            }
        }
        Connection c;
        if (driver != null) {
            // Directo al driver: DriverManager rechaza clases de otro ClassLoader.
            c = driver.connect(url, props);
            if (c == null) throw new SQLException("el driver " + driver.getClass().getName() + " no acepta la URL: " + url, "08001");
        } else {
            c = DriverManager.getConnection(url, props);
        }
        return c;
    }

    /** Un ClassLoader por conjunto de jars (se reutiliza: cargar un driver cuesta). */
    static ClassLoader loader(List<String> jars) throws Exception {
        if (jars.isEmpty()) return ConectorHelper.class.getClassLoader();
        String key = String.join(File.pathSeparator, jars);
        ClassLoader l = LOADERS.get(key);
        if (l != null) return l;
        URL[] urls = new URL[jars.size()];
        for (int i = 0; i < urls.length; i++) {
            File file = new File(jars.get(i));
            if (!file.isFile()) throw new FileNotFoundException("no existe el jar: " + file);
            urls[i] = file.toURI().toURL();
        }
        l = new URLClassLoader(urls, ConectorHelper.class.getClassLoader());
        LOADERS.put(key, l);
        return l;
    }

    static void test(String id, Map<String, String> a, List<String> jars, Properties props) throws Exception {
        long t0 = System.nanoTime();
        Connection c = connect(a, jars, props);
        long ms = (System.nanoTime() - t0) / 1000000;
        try {
            List<String> out = info(c);
            out.add(0, "ms=" + ms);
            try { out.add("valida=" + c.isValid(5)); } catch (Throwable ignored) { }
            send("ok", id, out.toArray(new String[0]));
        } finally {
            try { c.close(); } catch (Throwable ignored) { }
        }
    }

    static void open(String id, Map<String, String> a, List<String> jars, Properties props) throws Exception {
        long t0 = System.nanoTime();
        Connection c = connect(a, jars, props);
        long ms = (System.nanoTime() - t0) / 1000000;
        long s = NEXT_SESSION.getAndIncrement();
        SESSIONS.put(s, c);
        List<String> out = info(c);
        out.add(0, "ms=" + ms);
        out.add(0, "sesion=" + s);
        send("ok", id, out.toArray(new String[0]));
    }

    static List<String> info(Connection c) {
        List<String> out = new ArrayList<String>();
        try {
            DatabaseMetaData m = c.getMetaData();
            add(out, "producto", m.getDatabaseProductName());
            add(out, "version", m.getDatabaseProductVersion());
            add(out, "driver", m.getDriverName());
            add(out, "driver_version", m.getDriverVersion());
            add(out, "jdbc", m.getJDBCMajorVersion() + "." + m.getJDBCMinorVersion());
            add(out, "usuario", m.getUserName());
        } catch (Throwable ignored) { }
        try { add(out, "catalogo", c.getCatalog()); } catch (Throwable ignored) { }
        try { add(out, "esquema", c.getSchema()); } catch (Throwable ignored) { }
        return out;
    }

    static void add(List<String> out, String k, String v) {
        if (v != null) out.add(k + "=" + v);
    }

    // ---------- consultas ----------

    static void query(String id, Map<String, String> a) throws Exception {
        Connection c = session(a);
        runQuery(id, c, a);
    }

    static void runQuery(String id, Connection c, Map<String, String> a) throws Exception {
        String sql = a.containsKey("sql") ? a.get("sql") : "";
        int max = a.containsKey("max") ? Integer.parseInt(a.get("max")) : 1000;
        long t0 = System.nanoTime();
        // Sin límite (exportar): por cursor, sin cargar todo en memoria. Algunos
        // drivers (PostgreSQL) solo respetan fetchSize fuera de autocommit.
        boolean stream = max <= 0 && c.getAutoCommit();
        if (stream) {
            try { c.setAutoCommit(false); } catch (Throwable e) { stream = false; }
        }
        Statement st = c.createStatement();
        RUNNING.put(id, st);
        try {
            if (max > 0) st.setMaxRows(max + 1);
            st.setFetchSize(max > 0 ? Math.min(max, 500) : 500);
            boolean rs = st.execute(sql);
            long rows = 0;
            boolean more = false;
            StringBuilder updates = new StringBuilder();
            while (true) {
                if (rs) {
                    long[] res = stream(id, st.getResultSet(), max);
                    rows = res[0];
                    more = res[1] != 0;
                    break; // la interfaz muestra un resultado
                }
                int u = st.getUpdateCount();
                if (u == -1) break;
                if (updates.length() > 0) updates.append(',');
                updates.append(u);
                rs = st.getMoreResults();
            }
            long ms = (System.nanoTime() - t0) / 1000000;
            send("fin", id, "filas=" + rows, "mas=" + more, "actualizadas=" + updates, "ms=" + ms);
        } finally {
            RUNNING.remove(id);
            try { st.close(); } catch (Throwable ignored) { }
            if (stream) {
                try { c.rollback(); } catch (Throwable ignored) { }
                try { c.setAutoCommit(true); } catch (Throwable ignored) { }
            }
        }
    }

    /** Envía columnas y filas de un ResultSet (y lo cierra). Devuelve {filas, ¿hay más?}. */
    static long[] stream(String id, ResultSet r, int max) throws SQLException {
        long rows = 0;
        boolean more = false;
        try {
            ResultSetMetaData md = r.getMetaData();
            int n = md.getColumnCount();
            String[] cols = new String[n];
            int[] types = new int[n];
            for (int i = 0; i < n; i++) {
                cols[i] = md.getColumnLabel(i + 1) + "\u0001" + md.getColumnTypeName(i + 1);
                types[i] = md.getColumnType(i + 1);
            }
            send("columnas", id, cols);
            String[] cells = new String[n];
            while (r.next()) {
                if (max > 0 && rows >= max) { more = true; break; }
                for (int i = 0; i < n; i++) cells[i] = cell(r, i + 1, types[i]);
                send("fila", id, cells);
                rows++;
            }
        } finally {
            r.close();
        }
        return new long[] { rows, more ? 1 : 0 };
    }

    /**
     * Explorador: nivel=esquemas | tablas (catalogo, esquema) | columnas (+ tabla).
     * Responde como una consulta (columnas, filas, fin) con las de DatabaseMetaData.
     */
    static void meta(String id, Map<String, String> a) throws Exception {
        Connection c = session(a);
        DatabaseMetaData m = c.getMetaData();
        String level = a.containsKey("nivel") ? a.get("nivel") : "esquemas";
        String cat = emptyToNull(a.get("catalogo")), sch = emptyToNull(a.get("esquema"));
        long t0 = System.nanoTime();
        long[] res;
        if (level.equals("esquemas")) {
            // Unas bases usan esquemas (Oracle, PostgreSQL), otras catálogos (MySQL).
            ResultSet r = m.getSchemas();
            List<String[]> rows = new ArrayList<String[]>();
            try {
                while (r.next()) rows.add(new String[] { safeString(r, "TABLE_CATALOG"), r.getString("TABLE_SCHEM") });
            } finally {
                r.close();
            }
            if (rows.isEmpty()) {
                r = m.getCatalogs();
                try {
                    while (r.next()) rows.add(new String[] { r.getString("TABLE_CAT"), null });
                } finally {
                    r.close();
                }
            }
            send("columnas", id, "TABLE_CAT\u0001", "TABLE_SCHEM\u0001");
            for (String[] row : rows) send("fila", id, row);
            res = new long[] { rows.size(), 0 };
        } else if (level.equals("tablas")) {
            res = stream(id, m.getTables(cat, sch, "%", null), 50000);
        } else if (level.equals("columnas")) {
            res = stream(id, m.getColumns(cat, sch, a.get("tabla"), "%"), 50000);
        } else if (level.equals("ddl")) {
            String kind = a.containsKey("tipo") ? a.get("tipo") : "TABLE";
            send("columnas", id, "DDL\u0001");
            send("fila", id, ddl(c, m, cat, sch, a.get("tabla"), kind));
            res = new long[] { 1, 0 };
        } else {
            throw new IllegalArgumentException("nivel desconocido: " + level);
        }
        long ms = (System.nanoTime() - t0) / 1000000;
        send("fin", id, "filas=" + res[0], "mas=" + (res[1] != 0), "actualizadas=", "ms=" + ms);
    }

    // ---------- DDL ----------

    /**
     * DDL de una tabla o vista. Primero la de la propia base si sabe darla
     * (MySQL/MariaDB, Oracle, SQLite; vistas por information_schema); si no,
     * se arma con los metadatos JDBC: columnas, clave primaria, foráneas e índices.
     */
    static String ddl(Connection c, DatabaseMetaData m, String cat, String sch, String table, String kind) throws SQLException {
        String product = String.valueOf(m.getDatabaseProductName()).toLowerCase(java.util.Locale.ROOT);
        boolean view = kind.toUpperCase(java.util.Locale.ROOT).contains("VIEW");
        String nat = null;
        try {
            if (product.contains("mysql") || product.contains("mariadb")) {
                String q = "`" + table.replace("`", "``") + "`";
                if (cat != null) q = "`" + cat.replace("`", "``") + "`." + q;
                nat = queryText(c, "show create " + (view ? "view " : "table ") + q, 2, (String[]) null);
            } else if (product.contains("oracle")) {
                nat = queryText(c, "select dbms_metadata.get_ddl(?, ?, ?) from dual", 1, view ? "VIEW" : "TABLE", table, sch == null ? m.getUserName() : sch);
            } else if (product.contains("sqlite")) {
                nat = queryText(c, "select sql from sqlite_master where name = ?", 1, table);
            } else if (view) {
                String body = queryText(c, "select view_definition from information_schema.views where table_name = ?"
                        + (sch != null ? " and table_schema = ?" : ""), 1, sch != null ? new String[] { table, sch } : new String[] { table });
                if (body != null && !body.trim().isEmpty()) nat = "CREATE VIEW " + qualified(m, cat, sch, table) + " AS\n" + body.trim();
            }
        } catch (SQLException e) {
            nat = null; // sin permisos o sin soporte: se genera por metadatos
        }
        if (nat != null && !nat.trim().isEmpty()) {
            nat = nat.trim();
            return nat.endsWith(";") || nat.endsWith("/") ? nat : nat + ";";
        }
        return generatedDdl(m, cat, sch, table, view);
    }

    static String queryText(Connection c, String sql, int col, String... params) throws SQLException {
        PreparedStatement ps = c.prepareStatement(sql);
        try {
            if (params != null) for (int i = 0; i < params.length; i++) ps.setString(i + 1, params[i]);
            ResultSet r = ps.executeQuery();
            try {
                if (!r.next()) return null;
                Object o = r.getObject(col);
                if (o instanceof Clob) {
                    Clob cl = (Clob) o;
                    return cl.getSubString(1, (int) Math.min(cl.length(), 4000000));
                }
                return o == null ? null : o.toString();
            } finally {
                r.close();
            }
        } finally {
            ps.close();
        }
    }

    static String generatedDdl(DatabaseMetaData m, String cat, String sch, String table, boolean view) throws SQLException {
        String name = qualified(m, cat, sch, table);
        StringBuilder sb = new StringBuilder();
        sb.append("-- ").append(view ? "Vista" : "Tabla").append(" generada a partir de los metadatos JDBC (aproximada)\n");
        sb.append(view ? "-- CREATE VIEW " : "CREATE TABLE ").append(name).append(" (\n");
        List<String> lines = new ArrayList<String>();
        List<String[]> remarks = new ArrayList<String[]>();
        ResultSet r = m.getColumns(cat, sch, table, "%");
        try {
            while (r.next()) {
                String col = r.getString("COLUMN_NAME");
                StringBuilder l = new StringBuilder("    ").append(quote(m, col)).append(' ').append(typeOf(r));
                String def = safeString(r, "COLUMN_DEF");
                if (def != null && !def.trim().isEmpty()) l.append(" DEFAULT ").append(def.trim());
                else if ("YES".equalsIgnoreCase(safeString(r, "IS_AUTOINCREMENT"))) l.append(" /* autoincremental */");
                if (r.getInt("NULLABLE") == DatabaseMetaData.columnNoNulls) l.append(" NOT NULL");
                lines.add(l.toString());
                String rem = safeString(r, "REMARKS");
                if (rem != null && !rem.trim().isEmpty()) remarks.add(new String[] { col, rem });
            }
        } finally {
            r.close();
        }
        String pkName = null;
        Set<String> keyCols = new HashSet<String>();
        Set<String> keyNames = new HashSet<String>();
        if (!view) {
            // Clave primaria (columnas en el orden KEY_SEQ).
            pkName = null;
            java.util.TreeMap<Integer, String> pk = new java.util.TreeMap<Integer, String>();
            try {
                r = m.getPrimaryKeys(cat, sch, table);
                try {
                    while (r.next()) {
                        pk.put(r.getInt("KEY_SEQ"), r.getString("COLUMN_NAME"));
                        pkName = r.getString("PK_NAME");
                    }
                } finally {
                    r.close();
                }
            } catch (SQLException ignored) {
            }
            if (!pk.isEmpty()) {
                StringBuilder l = new StringBuilder("    ");
                if (pkName != null && !pkName.isEmpty()) l.append("CONSTRAINT ").append(quote(m, pkName)).append(' ');
                l.append("PRIMARY KEY (").append(joinQuoted(m, pk.values())).append(')');
                keyCols.add(joinQuoted(m, pk.values()));
                if (pkName != null) keyNames.add(pkName);
                lines.add(l.toString());
            }
            // Claves foráneas, agrupadas por nombre.
            Map<String, String[]> fks = new java.util.LinkedHashMap<String, String[]>();
            try {
                r = m.getImportedKeys(cat, sch, table);
                try {
                    while (r.next()) {
                        String fk = r.getString("FK_NAME");
                        String key = fk != null ? fk : r.getString("PKTABLE_NAME") + "#" + fks.size();
                        String[] cur = fks.get(key);
                        String from = quote(m, r.getString("FKCOLUMN_NAME")), to = quote(m, r.getString("PKCOLUMN_NAME"));
                        if (cur == null) {
                            String ref = qualified(m, safeString(r, "PKTABLE_CAT"), safeString(r, "PKTABLE_SCHEM"), r.getString("PKTABLE_NAME"));
                            fks.put(key, new String[] { fk, from, ref, to, rule(r.getShort("DELETE_RULE"), "DELETE") + rule(r.getShort("UPDATE_RULE"), "UPDATE") });
                        } else {
                            cur[1] += ", " + from;
                            cur[3] += ", " + to;
                        }
                    }
                } finally {
                    r.close();
                }
            } catch (SQLException ignored) {
            }
            for (String[] f : fks.values()) {
                keyCols.add(f[1]);
                if (f[0] != null) keyNames.add(f[0]);
                StringBuilder l = new StringBuilder("    ");
                if (f[0] != null && !f[0].isEmpty()) l.append("CONSTRAINT ").append(quote(m, f[0])).append(' ');
                l.append("FOREIGN KEY (").append(f[1]).append(") REFERENCES ").append(f[2]).append(" (").append(f[3]).append(')').append(f[4]);
                lines.add(l.toString());
            }
        }
        for (int i = 0; i < lines.size(); i++) {
            sb.append(view ? "-- " : "").append(lines.get(i)).append(i + 1 < lines.size() ? ",\n" : "\n");
        }
        sb.append(view ? "-- );\n" : ");\n");
        if (!view) {
            // Índices (menos el de la clave primaria, que ya está arriba).
            Map<String, String[]> idx = new java.util.LinkedHashMap<String, String[]>();
            try {
                r = m.getIndexInfo(cat, sch, table, false, true);
                try {
                    while (r.next()) {
                        if (r.getShort("TYPE") == DatabaseMetaData.tableIndexStatistic) continue;
                        String in = r.getString("INDEX_NAME"), col = r.getString("COLUMN_NAME");
                        if (in == null || col == null) continue;
                        String asc = safeString(r, "ASC_OR_DESC");
                        String c = quote(m, col) + ("D".equals(asc) ? " DESC" : "");
                        String[] cur = idx.get(in);
                        if (cur == null) idx.put(in, new String[] { r.getBoolean("NON_UNIQUE") ? "" : "UNIQUE ", c });
                        else cur[1] += ", " + c;
                    }
                } finally {
                    r.close();
                }
            } catch (SQLException ignored) {
            }
            boolean first = true;
            for (Map.Entry<String, String[]> e : idx.entrySet()) {
                String in = e.getKey();
                String up = in.toUpperCase(java.util.Locale.ROOT);
                // Índices que la base crea sola para la clave primaria y las foráneas.
                boolean implicit = keyNames.contains(in) || up.startsWith("PRIMARY") || up.endsWith("_PKEY")
                        || ((up.startsWith("CONSTRAINT_INDEX") || up.startsWith("SYS_") || up.startsWith("PK_") || up.startsWith("FK_")) && keyCols.contains(e.getValue()[1]));
                if (implicit) continue;
                if (first) sb.append('\n');
                first = false;
                sb.append("CREATE ").append(e.getValue()[0]).append("INDEX ").append(quote(m, in)).append(" ON ").append(name)
                        .append(" (").append(e.getValue()[1]).append(");\n");
            }
        }
        for (String[] rm : remarks) {
            sb.append("COMMENT ON COLUMN ").append(name).append('.').append(quote(m, rm[0])).append(" IS '").append(rm[1].replace("'", "''")).append("';\n");
        }
        return sb.toString().trim();
    }

    static String rule(short r, String what) {
        switch (r) {
            case DatabaseMetaData.importedKeyCascade: return " ON " + what + " CASCADE";
            case DatabaseMetaData.importedKeySetNull: return " ON " + what + " SET NULL";
            case DatabaseMetaData.importedKeySetDefault: return " ON " + what + " SET DEFAULT";
            default: return "";
        }
    }

    static String typeOf(ResultSet r) throws SQLException {
        String t = r.getString("TYPE_NAME");
        if (t == null) t = "?";
        if (t.indexOf('(') >= 0) return t;
        int size = r.getInt("COLUMN_SIZE");
        boolean noSize = r.wasNull() || size <= 0;
        int dec = r.getInt("DECIMAL_DIGITS");
        boolean noDec = r.wasNull();
        String u = t.toUpperCase(java.util.Locale.ROOT);
        boolean lob = u.contains("LOB") || u.contains("TEXT") || u.equals("BYTEA") || u.contains("LONG");
        if (noSize || lob || size >= 1000000000) return t;
        if (u.contains("CHAR") || u.contains("BINARY") || u.equals("BIT VARYING")) return t + "(" + size + ")";
        if (u.equals("NUMERIC") || u.equals("DECIMAL") || u.equals("NUMBER")) return t + "(" + size + (!noDec && dec > 0 ? "," + dec : "") + ")";
        return t;
    }

    static String joinQuoted(DatabaseMetaData m, java.util.Collection<String> cols) throws SQLException {
        StringBuilder sb = new StringBuilder();
        for (String c : cols) sb.append(sb.length() > 0 ? ", " : "").append(quote(m, c));
        return sb.toString();
    }

    static String qualified(DatabaseMetaData m, String cat, String sch, String table) throws SQLException {
        StringBuilder sb = new StringBuilder();
        if (sch != null && !sch.isEmpty()) sb.append(quote(m, sch)).append('.');
        else if (cat != null && !cat.isEmpty()) sb.append(quote(m, cat)).append('.');
        return sb.append(quote(m, table)).toString();
    }

    /** Pone comillas solo si hacen falta (caracteres raros o mayúsculas/minúsculas que la base cambiaría). */
    static String quote(DatabaseMetaData m, String id) throws SQLException {
        if (id == null) return "";
        boolean simple = id.matches("[A-Za-z_][A-Za-z0-9_$]*");
        if (simple && m.storesUpperCaseIdentifiers() && !id.equals(id.toUpperCase(java.util.Locale.ROOT))) simple = false;
        if (simple && m.storesLowerCaseIdentifiers() && !id.equals(id.toLowerCase(java.util.Locale.ROOT))) simple = false;
        if (simple) return id;
        String q = m.getIdentifierQuoteString();
        if (q == null || q.trim().isEmpty()) q = "\"";
        q = q.trim();
        return q + id.replace(q, q + q) + q;
    }

    /**
     * Importar: inserta filas en `tabla` (columnas separadas por \u0001 en
     * `columnas`). Valores en orden fila a fila: `v=texto` o `z=` (NULL).
     * Convierte cada valor al tipo de la columna (getColumns) y todo el lote va
     * en una transacción: si algo falla no queda nada a medias.
     */
    static void batch(String id, Map<String, String> a, List<String> f) throws Exception {
        Connection c = session(a);
        String table = a.get("tabla");
        String[] cols = a.get("columnas").split("\u0001", -1);
        List<String> vals = new ArrayList<String>();
        for (int i = 2; i < f.size(); i++) {
            String kv = f.get(i);
            if (kv == null) continue;
            if (kv.startsWith("v=")) vals.add(kv.substring(2));
            else if (kv.startsWith("z=")) vals.add(null);
        }
        int n = cols.length;
        if (n == 0 || vals.size() % n != 0) throw new IllegalArgumentException("número de valores incorrecto");
        int[] types = columnTypes(c, table, cols);
        StringBuilder sql = new StringBuilder("insert into ").append(table).append(" (");
        for (int i = 0; i < n; i++) sql.append(i > 0 ? ", " : "").append(cols[i]);
        sql.append(") values (");
        for (int i = 0; i < n; i++) sql.append(i > 0 ? ", ?" : "?");
        sql.append(')');
        long t0 = System.nanoTime();
        boolean auto = c.getAutoCommit();
        int rows = vals.size() / n;
        int row = 0;
        PreparedStatement ps = c.prepareStatement(sql.toString());
        RUNNING.put(id, ps);
        try {
            c.setAutoCommit(false);
            for (row = 0; row < rows; row++) {
                for (int i = 0; i < n; i++) bind(ps, i + 1, types[i], vals.get(row * n + i));
                ps.addBatch();
            }
            ps.executeBatch();
            c.commit();
        } catch (Exception e) {
            try { c.rollback(); } catch (Throwable ignored) { }
            if (e instanceof BatchUpdateException && e.getCause() == null && ((SQLException) e).getNextException() != null) {
                throw ((SQLException) e).getNextException();
            }
            if (row < rows) throw new SQLException("fila " + (row + 1) + " del lote: " + describe(e), e);
            throw e;
        } finally {
            RUNNING.remove(id);
            try { ps.close(); } catch (Throwable ignored) { }
            try { c.setAutoCommit(auto); } catch (Throwable ignored) { }
        }
        send("ok", id, "filas=" + rows, "ms=" + (System.nanoTime() - t0) / 1000000);
    }

    // ---------- editar resultados ----------

    /**
     * ¿Se pueden editar las filas de `sql`? Solo si todas sus columnas vienen de una
     * tabla y entre ellas está su clave primaria (o un índice único). Describe la
     * consulta sin ejecutarla (si el driver no sabe, la ejecuta con 1 fila como máximo).
     * Responde ok con catalogo, esquema, tabla (tal cual), sql_tabla (con comillas),
     * columnas / sql_columnas (columna base de cada columna del resultado, vacía si no
     * se puede editar) y clave (posiciones en el resultado, separadas por comas).
     */
    static void editable(String id, Map<String, String> a) throws Exception {
        Connection c = session(a);
        DatabaseMetaData m = c.getMetaData();
        String sql = a.get("sql");
        int n;
        String[] cat, sch, tab, col;
        int[] type;
        PreparedStatement ps = c.prepareStatement(sql);
        ResultSet rs = null;
        try {
            ResultSetMetaData md = ps.getMetaData();
            if (md == null) {
                ps.setMaxRows(1);
                rs = ps.executeQuery();
                md = rs.getMetaData();
            }
            n = md.getColumnCount();
            cat = new String[n]; sch = new String[n]; tab = new String[n]; col = new String[n];
            type = new int[n];
            for (int i = 0; i < n; i++) {
                cat[i] = emptyToNull(safe(md, "getCatalogName", i + 1));
                sch[i] = emptyToNull(baseOr(md, "getBaseSchemaName", "getSchemaName", i + 1));
                tab[i] = emptyToNull(baseOr(md, "getBaseTableName", "getTableName", i + 1));
                col[i] = emptyToNull(baseOr(md, "getBaseColumnName", "getColumnName", i + 1));
                type[i] = md.getColumnType(i + 1);
            }
        } finally {
            if (rs != null) try { rs.close(); } catch (Throwable ignored) { }
            try { ps.close(); } catch (Throwable ignored) { }
        }
        // Drivers que no dicen la tabla: «select … from X» simple, columnas por su etiqueta.
        boolean any = false;
        for (String t : tab) any |= t != null;
        if (!any) {
            String t = simpleFrom(sql);
            if (t == null) throw new SQLException("no se puede editar: el driver no dice de qué tabla vienen las columnas y la consulta no es un «select … from tabla» simple");
            String s = null;
            int dot = t.lastIndexOf('.');
            if (dot >= 0) { s = unquote(t.substring(0, dot)); t = t.substring(dot + 1); }
            t = unquote(t);
            Map<String, Integer> known = typesOf(m, null, s, t);
            if (known.isEmpty()) throw new SQLException("no se puede editar: no se encuentra la tabla " + t);
            String[] found = foundName(m, s, t);
            PreparedStatement p2 = c.prepareStatement(sql);
            try {
                ResultSetMetaData md = p2.getMetaData();
                for (int i = 0; i < n; i++) {
                    String label = md == null ? col[i] : md.getColumnLabel(i + 1);
                    if (label != null && known.containsKey(label.toLowerCase(Locale.ROOT))) {
                        cat[i] = found[0]; sch[i] = found[1]; tab[i] = found[2]; col[i] = label;
                    }
                }
            } finally {
                try { p2.close(); } catch (Throwable ignored) { }
            }
        }
        // Una sola tabla.
        String tCat = null, tSch = null, tTab = null;
        for (int i = 0; i < n; i++) {
            if (tab[i] == null) continue;
            if (tTab == null) { tCat = cat[i]; tSch = sch[i]; tTab = tab[i]; }
            else if (!tTab.equalsIgnoreCase(tab[i]) || (tSch != null && sch[i] != null && !tSch.equalsIgnoreCase(sch[i]))) {
                throw new SQLException("no se puede editar: la consulta une varias tablas (" + tTab + ", " + tab[i] + ")");
            }
        }
        if (tTab == null) throw new SQLException("no se puede editar: ninguna columna viene de una tabla");
        // Catálogo y esquema según getTables: el de ResultSetMetaData no siempre es fiable
        // (SQLite da el nombre de la tabla como catálogo).
        String[] real = foundName(m, tSch, tTab);
        tCat = real[0]; tSch = real[1]; tTab = real[2];
        List<String> key = keyColumns(m, tCat, tSch, tTab);
        if (key.isEmpty()) throw new SQLException("no se puede editar: la tabla " + tTab + " no tiene clave primaria ni índice único");
        StringBuilder keyIdx = new StringBuilder();
        for (String k : key) {
            int at = -1;
            for (int i = 0; i < n && at < 0; i++) if (tab[i] != null && k.equalsIgnoreCase(col[i])) at = i;
            if (at < 0) throw new SQLException("no se puede editar: falta la columna clave " + k + " en el resultado");
            keyIdx.append(keyIdx.length() > 0 ? "," : "").append(at);
        }
        StringBuilder cols = new StringBuilder(), sqlCols = new StringBuilder();
        for (int i = 0; i < n; i++) {
            boolean ok = tab[i] != null && col[i] != null && !isBinary(type[i]);
            if (i > 0) { cols.append('\u0001'); sqlCols.append('\u0001'); }
            if (ok) { cols.append(col[i]); sqlCols.append(quote(m, col[i])); }
        }
        send("ok", id, "catalogo=" + (tCat == null ? "" : tCat), "esquema=" + (tSch == null ? "" : tSch), "tabla=" + tTab,
            "sql_tabla=" + qualified(m, tCat, tSch, tTab), "columnas=" + cols, "sql_columnas=" + sqlCols, "clave=" + keyIdx);
    }

    /**
     * Guarda cambios en filas de una tabla: `catalogo`, `esquema`, `tabla`, `clave`
     * (columnas clave, \u0001) y por cada fila `set=colA\u0001colB` seguido de los
     * valores nuevos y de los de la clave (`v=texto` / `z=` NULL). Un UPDATE por fila,
     * con parámetros y todo en una transacción: si alguno no cambia exactamente una
     * fila, no se guarda nada.
     */
    static void edit(String id, Map<String, String> a, List<String> f) throws Exception {
        Connection c = session(a);
        DatabaseMetaData m = c.getMetaData();
        String cat = emptyToNull(a.get("catalogo")), sch = emptyToNull(a.get("esquema")), tab = a.get("tabla");
        String[] key = a.get("clave").split("\u0001", -1);
        List<String[]> sets = new ArrayList<String[]>();
        List<List<String>> vals = new ArrayList<List<String>>();
        for (int i = 2; i < f.size(); i++) {
            String kv = f.get(i);
            if (kv == null) continue;
            if (kv.startsWith("set=")) { sets.add(kv.substring(4).split("\u0001", -1)); vals.add(new ArrayList<String>()); }
            else if (!vals.isEmpty() && kv.startsWith("v=")) vals.get(vals.size() - 1).add(kv.substring(2));
            else if (!vals.isEmpty() && kv.startsWith("z=")) vals.get(vals.size() - 1).add(null);
        }
        Map<String, Integer> types = typesOf(m, cat, sch, tab);
        String table = qualified(m, cat, sch, tab);
        long t0 = System.nanoTime();
        boolean auto = c.getAutoCommit();
        int row = 0;
        try {
            c.setAutoCommit(false);
            for (row = 0; row < sets.size(); row++) {
                String[] set = sets.get(row);
                List<String> v = vals.get(row);
                if (v.size() != set.length + key.length) throw new IllegalArgumentException("número de valores incorrecto");
                StringBuilder sql = new StringBuilder("update ").append(table).append(" set ");
                for (int i = 0; i < set.length; i++) sql.append(i > 0 ? ", " : "").append(quote(m, set[i])).append(" = ?");
                sql.append(" where ");
                for (int i = 0; i < key.length; i++) {
                    sql.append(i > 0 ? " and " : "").append(quote(m, key[i]));
                    sql.append(v.get(set.length + i) == null ? " is null" : " = ?");
                }
                PreparedStatement ps = c.prepareStatement(sql.toString());
                RUNNING.put(id, ps);
                try {
                    int p = 1;
                    for (int i = 0; i < set.length; i++) bind(ps, p++, typeOf(types, set[i]), v.get(i));
                    for (int i = 0; i < key.length; i++) {
                        String kv = v.get(set.length + i);
                        if (kv != null) bind(ps, p++, typeOf(types, key[i]), kv);
                    }
                    int u = ps.executeUpdate();
                    if (u != 1) throw new SQLException("el cambio " + (row + 1) + " afectó " + u + " filas (se esperaba 1): no se guardó nada");
                } finally {
                    RUNNING.remove(id);
                    try { ps.close(); } catch (Throwable ignored) { }
                }
            }
            c.commit();
        } catch (Exception e) {
            try { c.rollback(); } catch (Throwable ignored) { }
            if (e instanceof SQLException && e.getMessage() != null && e.getMessage().startsWith("el cambio ")) throw e;
            throw new SQLException("cambio " + (row + 1) + ": " + describe(e) + " (no se guardó nada)", e);
        } finally {
            try { c.setAutoCommit(auto); } catch (Throwable ignored) { }
        }
        send("ok", id, "filas=" + sets.size(), "ms=" + (System.nanoTime() - t0) / 1000000);
    }

    static boolean isBinary(int t) {
        return t == Types.BINARY || t == Types.VARBINARY || t == Types.LONGVARBINARY || t == Types.BLOB;
    }

    static int typeOf(Map<String, Integer> types, String col) {
        Integer t = types.get(col.toLowerCase(Locale.ROOT));
        return t == null ? Types.OTHER : t;
    }

    /** ResultSetMetaData: método del driver (PostgreSQL: getBaseTableName…) o el estándar. */
    static String baseOr(ResultSetMetaData md, String base, String std, int i) {
        try {
            Object v = md.getClass().getMethod(base, int.class).invoke(md, i);
            if (v != null && !v.toString().isEmpty()) return v.toString();
        } catch (Throwable ignored) { }
        return safe(md, std, i);
    }

    static String safe(ResultSetMetaData md, String method, int i) {
        try {
            Object v = ResultSetMetaData.class.getMethod(method, int.class).invoke(md, i);
            return v == null ? null : v.toString();
        } catch (Throwable e) {
            return null;
        }
    }

    /** Tabla de «select … from X [where/order …]» sin JOIN ni varias tablas; null si no lo es. */
    static String simpleFrom(String sql) {
        java.util.regex.Matcher mt = java.util.regex.Pattern
            .compile("(?is)^\\s*select\\s.+?\\sfrom\\s+([^\\s,;()]+)\\s*(?:(?:as\\s+)?[A-Za-z_]\\w*\\s*)?(?:$|;|\\s(?:where|order|limit|fetch|offset|for)\\b.*)")
            .matcher(sql.trim());
        if (!mt.matches()) return null;
        String t = mt.group(1);
        String rest = sql.substring(mt.end(1)).toLowerCase(Locale.ROOT);
        if (rest.matches("(?s).*\\bjoin\\b.*")) return null;
        String w = t.toLowerCase(Locale.ROOT);
        return w.equals("where") || w.equals("order") ? null : t;
    }

    static String unquote(String s) {
        s = s.trim();
        if (s.length() >= 2 && (s.charAt(0) == '"' || s.charAt(0) == '`' || s.charAt(0) == '[')) return s.substring(1, s.length() - 1);
        return s;
    }

    /** Nombre real (catálogo, esquema, tabla) tal cual lo guarda la base, probando MAYÚS/minús. */
    static String[] foundName(DatabaseMetaData m, String sch, String tab) {
        String[][] tries = { { sch, tab }, { up(sch), up(tab) }, { low(sch), low(tab) } };
        for (String[] tr : tries) {
            try {
                ResultSet r = m.getTables(null, tr[0], tr[1], null);
                try {
                    if (r.next()) return new String[] { emptyToNull(r.getString("TABLE_CAT")), emptyToNull(r.getString("TABLE_SCHEM")), r.getString("TABLE_NAME") };
                } finally {
                    r.close();
                }
            } catch (Throwable ignored) { }
        }
        return new String[] { null, sch, tab };
    }

    /** Columnas de la tabla (en minúsculas) → tipo JDBC. */
    static Map<String, Integer> typesOf(DatabaseMetaData m, String cat, String sch, String tab) {
        Map<String, Integer> found = new HashMap<String, Integer>();
        String[][] tries = { { sch, tab }, { up(sch), up(tab) }, { low(sch), low(tab) } };
        for (String[] tr : tries) {
            try {
                ResultSet r = m.getColumns(cat, tr[0], tr[1], null);
                try {
                    while (r.next()) found.put(r.getString("COLUMN_NAME").toLowerCase(Locale.ROOT), r.getInt("DATA_TYPE"));
                } finally {
                    r.close();
                }
            } catch (Throwable ignored) { }
            if (!found.isEmpty()) break;
        }
        return found;
    }

    /** Columnas de la clave primaria; si no hay, las del primer índice único. */
    static List<String> keyColumns(DatabaseMetaData m, String cat, String sch, String tab) {
        String[][] tries = { { sch, tab }, { up(sch), up(tab) }, { low(sch), low(tab) } };
        for (String[] tr : tries) {
            TreeMap<Integer, String> pk = new TreeMap<Integer, String>();
            try {
                ResultSet r = m.getPrimaryKeys(cat, tr[0], tr[1]);
                try {
                    while (r.next()) pk.put(r.getInt("KEY_SEQ"), r.getString("COLUMN_NAME"));
                } finally {
                    r.close();
                }
            } catch (Throwable ignored) { }
            if (!pk.isEmpty()) return new ArrayList<String>(pk.values());
        }
        for (String[] tr : tries) {
            LinkedHashMap<String, TreeMap<Integer, String>> idx = new LinkedHashMap<String, TreeMap<Integer, String>>();
            try {
                ResultSet r = m.getIndexInfo(cat, tr[0], tr[1], true, false);
                try {
                    while (r.next()) {
                        String name = r.getString("INDEX_NAME"), col = r.getString("COLUMN_NAME");
                        if (name == null || col == null || r.getBoolean("NON_UNIQUE")) continue;
                        if (!idx.containsKey(name)) idx.put(name, new TreeMap<Integer, String>());
                        idx.get(name).put((int) r.getShort("ORDINAL_POSITION"), col);
                    }
                } finally {
                    r.close();
                }
            } catch (Throwable ignored) { }
            if (!idx.isEmpty()) return new ArrayList<String>(idx.values().iterator().next().values());
        }
        return new ArrayList<String>();
    }

    /** Tipos JDBC de las columnas (Types.OTHER si no se sabe). */
    static int[] columnTypes(Connection c, String table, String[] cols) {
        int[] t = new int[cols.length];
        Arrays.fill(t, Types.OTHER);
        String sch = null, name = table;
        int dot = table.lastIndexOf('.');
        if (dot >= 0) { sch = table.substring(0, dot); name = table.substring(dot + 1); }
        Map<String, Integer> found = new HashMap<String, Integer>();
        try {
            DatabaseMetaData m = c.getMetaData();
            // Probar el nombre tal cual, en mayúsculas y en minúsculas (cada base guarda distinto).
            String[][] tries = { { sch, name }, { up(sch), up(name) }, { low(sch), low(name) } };
            for (String[] tr : tries) {
                ResultSet r = m.getColumns(null, tr[0], tr[1], null);
                try {
                    while (r.next()) found.put(r.getString("COLUMN_NAME").toLowerCase(Locale.ROOT), r.getInt("DATA_TYPE"));
                } finally {
                    r.close();
                }
                if (!found.isEmpty()) break;
            }
        } catch (Throwable ignored) { }
        for (int i = 0; i < cols.length; i++) {
            Integer ty = found.get(cols[i].trim().replace("\"", "").toLowerCase(Locale.ROOT));
            if (ty != null) t[i] = ty;
        }
        return t;
    }

    static String up(String s) { return s == null ? null : s.toUpperCase(Locale.ROOT); }
    static String low(String s) { return s == null ? null : s.toLowerCase(Locale.ROOT); }

    static void bind(PreparedStatement ps, int i, int type, String v) throws SQLException {
        if (v == null) {
            ps.setNull(i, type == Types.OTHER ? Types.VARCHAR : type);
            return;
        }
        String s = v.trim();
        switch (type) {
            case Types.TINYINT: case Types.SMALLINT: case Types.INTEGER: case Types.BIGINT:
                ps.setLong(i, Long.parseLong(s)); break;
            case Types.DECIMAL: case Types.NUMERIC:
                ps.setBigDecimal(i, new java.math.BigDecimal(s.replace(',', '.'))); break;
            case Types.FLOAT: case Types.REAL: case Types.DOUBLE:
                ps.setDouble(i, Double.parseDouble(s.replace(',', '.'))); break;
            case Types.BOOLEAN: case Types.BIT:
                ps.setBoolean(i, s.equalsIgnoreCase("true") || s.equals("1") || s.equalsIgnoreCase("si") || s.equalsIgnoreCase("sí") || s.equalsIgnoreCase("yes")); break;
            case Types.DATE:
                ps.setDate(i, java.sql.Date.valueOf(s.length() > 10 ? s.substring(0, 10) : s)); break;
            case Types.TIMESTAMP: case Types.TIMESTAMP_WITH_TIMEZONE:
                ps.setTimestamp(i, Timestamp.valueOf(s.length() == 10 ? s + " 00:00:00" : s.replace('T', ' '))); break;
            case Types.TIME:
                ps.setTime(i, Time.valueOf(s)); break;
            case Types.OTHER:
                ps.setObject(i, v); break;
            default:
                ps.setString(i, v);
        }
    }

    static String emptyToNull(String s) {
        return s == null || s.isEmpty() ? null : s;
    }

    /** TABLE_CATALOG no existe en drivers JDBC 3 antiguos. */
    static String safeString(ResultSet r, String col) {
        try { return r.getString(col); } catch (Throwable e) { return null; }
    }

    static String cell(ResultSet r, int i, int type) throws SQLException {
        String s;
        switch (type) {
            case Types.BINARY: case Types.VARBINARY: case Types.LONGVARBINARY: case Types.BLOB: {
                byte[] b = r.getBytes(i);
                if (b == null) return null;
                StringBuilder sb = new StringBuilder("0x");
                int n = Math.min(b.length, MAX_CELL / 2);
                for (int k = 0; k < n; k++) sb.append(String.format("%02X", b[k] & 0xff));
                if (n < b.length) sb.append("… (").append(b.length).append(" bytes)");
                return sb.toString();
            }
            default:
                s = r.getString(i);
        }
        if (s != null && s.length() > MAX_CELL) s = s.substring(0, MAX_CELL) + "…";
        return s;
    }

    static Connection session(Map<String, String> a) throws SQLException {
        String s = a.get("sesion");
        Connection c = s == null ? null : SESSIONS.get(Long.parseLong(s));
        if (c == null) throw new SQLException("la sesión no existe (¿se reinició Java?)");
        return c;
    }

    static void cancel(String id, Map<String, String> a) throws Exception {
        Statement st = RUNNING.get(a.get("objetivo"));
        if (st != null) st.cancel();
        send("ok", id, "cancelada=" + (st != null));
    }

    static void close(String id, Map<String, String> a) throws Exception {
        String s = a.get("sesion");
        Connection c = s == null ? null : SESSIONS.remove(Long.parseLong(s));
        if (c != null) c.close();
        send("ok", id);
    }

    // ---------- protocolo ----------

    static void sendError(String id, Throwable e) {
        List<String> out = new ArrayList<String>();
        out.add("mensaje=" + describe(e));
        if (e instanceof SQLException) {
            SQLException s = (SQLException) e;
            if (s.getSQLState() != null) out.add("sqlstate=" + s.getSQLState());
            out.add("codigo=" + s.getErrorCode());
        }
        out.add("clase=" + e.getClass().getName());
        // Cadena de causas: suele estar ahí lo útil (red, SSL, credenciales).
        StringBuilder chain = new StringBuilder();
        Throwable t = e.getCause();
        for (int i = 0; t != null && i < 8; i++, t = t.getCause()) {
            if (chain.length() > 0) chain.append('\n');
            chain.append(t.getClass().getName()).append(": ").append(t.getMessage());
        }
        if (chain.length() > 0) out.add("causas=" + chain);
        // Errores encadenados (DB2, SQL Server, lotes): la causa real suele estar aquí.
        if (e instanceof SQLException) {
            StringBuilder next = new StringBuilder();
            SQLException n = ((SQLException) e).getNextException();
            for (int i = 0; n != null && i < 8; i++, n = n.getNextException()) {
                if (next.length() > 0) next.append('\n');
                next.append(n.getClass().getName()).append(": ").append(n.getMessage());
                if (n.getSQLState() != null) next.append(" [").append(n.getSQLState()).append(']');
            }
            if (next.length() > 0) out.add("siguientes=" + next);
        }
        send("error", id, out.toArray(new String[0]));
    }

    static String describe(Throwable e) {
        if (e instanceof ClassNotFoundException) return "no se encuentra la clase del driver: " + e.getMessage() + " (¿falta el jar?)";
        String m = e.getMessage();
        return m == null || m.isEmpty() ? e.getClass().getSimpleName() : m;
    }

    static void send(String kind, String id, String... fields) {
        StringBuilder sb = new StringBuilder(64);
        sb.append(kind).append('\t').append(id);
        for (String f : fields) {
            sb.append('\t');
            if (f == null) { sb.append("\\N"); continue; }
            for (int i = 0; i < f.length(); i++) {
                char ch = f.charAt(i);
                switch (ch) {
                    case '\\': sb.append("\\\\"); break;
                    case '\t': sb.append("\\t"); break;
                    case '\n': sb.append("\\n"); break;
                    case '\r': sb.append("\\r"); break;
                    default: sb.append(ch);
                }
            }
        }
        sb.append('\n');
        synchronized (OUT) {
            try {
                OUT.write(sb.toString());
                // Las filas se agrupan; el resto sale enseguida.
                if (!kind.equals("fila")) OUT.flush();
            } catch (IOException e) {
                System.exit(1);
            }
        }
    }

    static List<String> split(String line) {
        List<String> out = new ArrayList<String>();
        StringBuilder sb = new StringBuilder();
        boolean nul = false;
        for (int i = 0; i < line.length(); i++) {
            char ch = line.charAt(i);
            if (ch == '\t') {
                out.add(nul && sb.length() == 0 ? null : sb.toString());
                sb.setLength(0);
                nul = false;
            } else if (ch == '\\' && i + 1 < line.length()) {
                char n = line.charAt(++i);
                switch (n) {
                    case 't': sb.append('\t'); break;
                    case 'n': sb.append('\n'); break;
                    case 'r': sb.append('\r'); break;
                    case 'N': nul = true; break;
                    default: sb.append(n);
                }
            } else {
                sb.append(ch);
            }
        }
        out.add(nul && sb.length() == 0 ? null : sb.toString());
        return out;
    }
}
