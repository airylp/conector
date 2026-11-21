// Cliente de ejemplo del servicio de conector (Java 11+, sin dependencias).
// Se ejecuta directamente:  java Cliente.java lista
//
// Variables de entorno:
//   CONECTOR_URL    https://equipo:7470   (por defecto https://localhost:7470)
//   CONECTOR_TOKEN  token del servicio
//   CONECTOR_CA     certificado del servicio (servicio-cert.pem); sin él se usan
//                   los certificados del sistema
//
// Órdenes: lista | probar ID | consulta ID "SQL" [MAX] | eventos
// (Las respuestas son JSON; aquí se muestran tal cual. En un programa real,
//  léelas con la biblioteca JSON que ya uses.)

import java.io.FileInputStream;
import java.io.InputStream;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.time.Duration;
import java.util.stream.Stream;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManagerFactory;

public class Cliente {
    static final String URL = env("CONECTOR_URL", "https://localhost:7470").replaceAll("/+$", "");
    static final String TOKEN = env("CONECTOR_TOKEN", "");

    static String env(String k, String def) {
        String v = System.getenv(k);
        return v == null || v.isEmpty() ? def : v;
    }

    /** Confía solo en el certificado del servicio (si se indica). */
    static HttpClient client() throws Exception {
        HttpClient.Builder b = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(10));
        String ca = System.getenv("CONECTOR_CA");
        if (ca != null && !ca.isEmpty()) {
            KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
            ks.load(null, null);
            try (InputStream in = new FileInputStream(ca)) {
                int i = 0;
                for (Certificate c : CertificateFactory.getInstance("X.509").generateCertificates(in)) {
                    ks.setCertificateEntry("conector" + i++, c);
                }
            }
            TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            tmf.init(ks);
            SSLContext ctx = SSLContext.getInstance("TLS");
            ctx.init(null, tmf.getTrustManagers(), null);
            b.sslContext(ctx);
        }
        return b.build();
    }

    static HttpRequest.Builder req(String path) {
        return HttpRequest.newBuilder(URI.create(URL + path)).header("Authorization", "Bearer " + TOKEN).timeout(Duration.ofSeconds(330));
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.err.println("Uso: java Cliente.java lista | probar ID | consulta ID \"SQL\" [MAX] | eventos");
            System.exit(2);
        }
        HttpClient http = client();
        switch (args[0]) {
            case "lista":
                System.out.println(http.send(req("/v1/conexiones").GET().build(), HttpResponse.BodyHandlers.ofString()).body());
                break;
            case "probar":
                System.out.println(http.send(req("/v1/conexiones/" + enc(args[1]) + "/probar").POST(HttpRequest.BodyPublishers.noBody()).build(), HttpResponse.BodyHandlers.ofString()).body());
                break;
            case "consulta": {
                String max = args.length > 3 ? args[3] : "1000";
                HttpRequest r = req("/v1/conexiones/" + enc(args[1]) + "/consulta?max=" + enc(max)).POST(HttpRequest.BodyPublishers.ofString(args[2], StandardCharsets.UTF_8)).build();
                System.out.println(http.send(r, HttpResponse.BodyHandlers.ofString(StandardCharsets.UTF_8)).body());
                break;
            }
            case "eventos": {
                // Flujo SSE: una línea «event:» y otra «data:» por cambio.
                HttpRequest r = HttpRequest.newBuilder(URI.create(URL + "/v1/eventos")).header("Authorization", "Bearer " + TOKEN).GET().build();
                try (Stream<String> lines = http.send(r, HttpResponse.BodyHandlers.ofLines()).body()) {
                    lines.filter(l -> l.startsWith("event:") || l.startsWith("data:")).forEach(l -> {
                        System.out.println(l);
                        System.out.flush();
                    });
                }
                break;
            }
            default:
                System.err.println("Orden desconocida: " + args[0]);
                System.exit(2);
        }
    }

    static String enc(String s) {
        return URLEncoder.encode(s, StandardCharsets.UTF_8);
    }
}
