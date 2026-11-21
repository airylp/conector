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
            pool.execute(new Runnable() {
                public void run() { handle(f); }
            });
        }
        for (Connection c : SESSIONS.values()) {
            try { c.close(); } catch (Throwable ignored) { }
        }
        System.exit(0);
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
        String sql = a.containsKey("sql") ? a.get("sql") : "";
        int max = a.containsKey("max") ? Integer.parseInt(a.get("max")) : 1000;
        long t0 = System.nanoTime();
        Statement st = c.createStatement();
        RUNNING.put(id, st);
        try {
            if (max > 0) st.setMaxRows(max + 1);
            st.setFetchSize(Math.min(Math.max(max, 1), 500));
            boolean rs = st.execute(sql);
            long rows = 0;
            boolean more = false;
            StringBuilder updates = new StringBuilder();
            while (true) {
                if (rs) {
                    ResultSet r = st.getResultSet();
                    try {
                        ResultSetMetaData md = r.getMetaData();
                        int n = md.getColumnCount();
                        String[] cols = new String[n];
                        for (int i = 0; i < n; i++) {
                            cols[i] = md.getColumnLabel(i + 1) + "\u0001" + md.getColumnTypeName(i + 1);
                        }
                        send("columnas", id, cols);
                        String[] cells = new String[n];
                        while (r.next()) {
                            if (max > 0 && rows >= max) { more = true; break; }
                            for (int i = 0; i < n; i++) cells[i] = cell(r, i + 1, md.getColumnType(i + 1));
                            send("fila", id, cells);
                            rows++;
                        }
                    } finally {
                        r.close();
                    }
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
        }
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
