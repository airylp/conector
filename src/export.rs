//! Exportar resultados en streaming (fila a fila, sin guardarlas en memoria):
//! CSV, JSON, Excel (.xlsx escrito a mano: XML dentro de un zip sin comprimir,
//! la hoja pasa por un temporal) y sentencias INSERT. Y leer CSV para importar.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    Csv,
    Json,
    Xlsx,
    Insert,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Csv, Format::Json, Format::Xlsx, Format::Insert];
    pub fn ext(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Xlsx => "xlsx",
            Format::Insert => "sql",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Json => "JSON",
            Format::Xlsx => "Excel (.xlsx)",
            Format::Insert => "SQL (INSERT)",
        }
    }
}

/// Máximo de filas de una hoja de Excel (sin la cabecera).
pub const XLSX_MAX_ROWS: u64 = 1_048_575;

/// ¿Tipo SQL numérico? (sin comillas en INSERT, número en Excel).
pub fn numeric(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    ["INT", "NUMBER", "NUMERIC", "DECIMAL", "FLOAT", "DOUBLE", "REAL", "SERIAL", "MONEY"].iter().any(|k| t.contains(k))
}

pub struct Exporter {
    fmt: Format,
    path: PathBuf,
    out: BufWriter<File>,
    cols: Vec<(String, String)>,
    table: String,
    rows: u64,
    /// Excel: la hoja va a un temporal y al final se arma el zip.
    sheet: Option<PathBuf>,
}

fn xml_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Caracteres de control no válidos en XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

fn csv_field(s: &str, out: &mut String) {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// Letra(s) de columna de Excel: 0 → A, 26 → AA.
fn col_name(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

impl Exporter {
    /// `table` solo se usa en INSERT.
    pub fn create(path: &Path, fmt: Format, table: &str) -> std::io::Result<Exporter> {
        let target = if fmt == Format::Xlsx { path.with_extension("hoja.tmp") } else { path.with_extension(format!("{}.tmp", fmt.ext())) };
        let out = BufWriter::with_capacity(1 << 16, File::create(&target)?);
        Ok(Exporter { fmt, path: path.to_owned(), out, cols: Vec::new(), table: table.to_owned(), rows: 0, sheet: Some(target) })
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn columns(&mut self, cols: &[(String, String)]) -> std::io::Result<()> {
        self.cols = cols.to_vec();
        let mut s = String::new();
        match self.fmt {
            Format::Csv => {
                for (i, (n, _)) in cols.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    csv_field(n, &mut s);
                }
                s.push_str("\r\n");
            }
            Format::Json => s.push('['),
            Format::Xlsx => {
                s.push_str("<row r=\"1\">");
                for (i, (n, _)) in cols.iter().enumerate() {
                    s.push_str(&format!("<c r=\"{}1\" t=\"inlineStr\" s=\"1\"><is><t>", col_name(i)));
                    xml_escape(n, &mut s);
                    s.push_str("</t></is></c>");
                }
                s.push_str("</row>");
            }
            Format::Insert => {}
        }
        self.out.write_all(s.as_bytes())
    }

    pub fn row(&mut self, cells: &[Option<Box<str>>]) -> std::io::Result<()> {
        if self.fmt == Format::Xlsx && self.rows >= XLSX_MAX_ROWS {
            return Ok(()); // límite de Excel: el resto se descarta (se avisa al terminar)
        }
        let mut s = String::with_capacity(64);
        let cell = |i: usize| cells.get(i).and_then(|c| c.as_deref());
        match self.fmt {
            Format::Csv => {
                for i in 0..self.cols.len() {
                    if i > 0 {
                        s.push(',');
                    }
                    if let Some(v) = cell(i) {
                        csv_field(v, &mut s);
                    }
                }
                s.push_str("\r\n");
            }
            Format::Json => {
                if self.rows > 0 {
                    s.push(',');
                }
                s.push_str("\n{");
                for (i, (n, t)) in self.cols.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&crate::server::js(n));
                    s.push(':');
                    match cell(i) {
                        None => s.push_str("null"),
                        Some(v) if numeric(t) && v.parse::<f64>().is_ok_and(f64::is_finite) => s.push_str(v),
                        Some(v) => s.push_str(&crate::server::js(v)),
                    }
                }
                s.push('}');
            }
            Format::Xlsx => {
                let r = self.rows + 2;
                s.push_str(&format!("<row r=\"{r}\">"));
                for (i, (_, t)) in self.cols.iter().enumerate() {
                    let Some(v) = cell(i) else { continue };
                    let at = format!("{}{r}", col_name(i));
                    if numeric(t) && v.parse::<f64>().is_ok_and(f64::is_finite) {
                        s.push_str(&format!("<c r=\"{at}\"><v>{v}</v></c>"));
                    } else {
                        s.push_str(&format!("<c r=\"{at}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">"));
                        xml_escape(v, &mut s);
                        s.push_str("</t></is></c>");
                    }
                }
                s.push_str("</row>");
            }
            Format::Insert => {
                s.push_str("insert into ");
                s.push_str(&self.table);
                s.push_str(" (");
                s.push_str(&self.cols.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
                s.push_str(") values (");
                for (i, (_, t)) in self.cols.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    match cell(i) {
                        None => s.push_str("NULL"),
                        Some(v) if numeric(t) && v.parse::<f64>().is_ok_and(f64::is_finite) => s.push_str(v),
                        Some(v) => {
                            s.push('\'');
                            s.push_str(&v.replace('\'', "''"));
                            s.push('\'');
                        }
                    }
                }
                s.push_str(");\n");
            }
        }
        self.rows += 1;
        self.out.write_all(s.as_bytes())
    }

    /// Cierra y deja el archivo final (renombrando el temporal). Devuelve las filas.
    pub fn finish(mut self) -> std::io::Result<u64> {
        if self.fmt == Format::Json {
            self.out.write_all(if self.rows > 0 { b"\n]\n" } else { b"]\n" })?;
        }
        self.out.flush()?;
        let tmp = self.sheet.take().unwrap();
        drop(self.out);
        if self.fmt == Format::Xlsx {
            let r = write_xlsx(&self.path, &tmp, self.cols.len());
            let _ = std::fs::remove_file(&tmp);
            r?;
        } else {
            std::fs::rename(&tmp, &self.path)?;
        }
        Ok(self.rows)
    }

    /// Abandona (borra el temporal).
    pub fn abort(mut self) {
        if let Some(t) = self.sheet.take() {
            let _ = std::fs::remove_file(t);
        }
    }
}

// ---------- .xlsx mínimo ----------

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    crc = !crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Zip sin compresión (método 0) con ZIP64 desactivado: hojas < 4 GB.
struct Zip<W: Write> {
    out: W,
    offset: u64,
    central: Vec<u8>,
    count: u16,
}

impl<W: Write> Zip<W> {
    fn add(&mut self, name: &str, mut data: impl Read, len: u64, crc: u32) -> std::io::Result<()> {
        if len > u32::MAX as u64 || self.offset > u32::MAX as u64 {
            return Err(std::io::Error::other("la hoja supera 4 GB: exporta a CSV"));
        }
        let mut h = Vec::new();
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&[20, 0, 0, 0x08, 0, 0, 0, 0, 0x21, 0]); // versión, flags UTF-8, método 0, hora/fecha fijas
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&(len as u32).to_le_bytes());
        h.extend_from_slice(&(len as u32).to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        self.out.write_all(&h)?;
        let copied = std::io::copy(&mut data, &mut self.out)?;
        if copied != len {
            return Err(std::io::Error::other("tamaño inesperado"));
        }
        let mut c = Vec::new();
        c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        c.extend_from_slice(&[20, 0, 20, 0, 0, 0x08, 0, 0, 0, 0, 0x21, 0]);
        c.extend_from_slice(&crc.to_le_bytes());
        c.extend_from_slice(&(len as u32).to_le_bytes());
        c.extend_from_slice(&(len as u32).to_le_bytes());
        c.extend_from_slice(&(name.len() as u16).to_le_bytes());
        c.extend_from_slice(&[0; 8]); // extra, comentario, disco, atributos internos
        c.extend_from_slice(&0u32.to_le_bytes());
        c.extend_from_slice(&(self.offset as u32).to_le_bytes());
        c.extend_from_slice(name.as_bytes());
        self.central.extend_from_slice(&c);
        self.offset += h.len() as u64 + len;
        self.count += 1;
        Ok(())
    }

    fn add_bytes(&mut self, name: &str, data: &[u8]) -> std::io::Result<()> {
        self.add(name, data, data.len() as u64, crc32_update(0, data))
    }

    fn finish(mut self) -> std::io::Result<()> {
        let mut e = Vec::new();
        e.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        e.extend_from_slice(&[0, 0, 0, 0]);
        e.extend_from_slice(&self.count.to_le_bytes());
        e.extend_from_slice(&self.count.to_le_bytes());
        e.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        e.extend_from_slice(&(self.offset as u32).to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        self.out.write_all(&self.central)?;
        self.out.write_all(&e)?;
        self.out.flush()
    }
}

fn write_xlsx(path: &Path, rows_xml: &Path, ncols: usize) -> std::io::Result<()> {
    // Cabecera fija (primera fila congelada) y, tras los datos, autofiltro.
    let head = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetViews><sheetView workbookViewId=\"0\"><pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/></sheetView></sheetViews><sheetData>".to_owned();
    let filter = if ncols > 0 { format!("<autoFilter ref=\"A1:{}1\"/>", col_name(ncols - 1)) } else { String::new() };
    let tail = format!("</sheetData>{filter}</worksheet>");
    // CRC y tamaño de la hoja: cabecera + filas (del temporal) + cierre, sin cargarla.
    let mut crc = crc32_update(0, head.as_bytes());
    let mut f = File::open(rows_xml)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc = crc32_update(crc, &buf[..n]);
    }
    crc = crc32_update(crc, tail.as_bytes());
    let len = head.len() as u64 + std::fs::metadata(rows_xml)?.len() + tail.len() as u64;

    let tmp = path.with_extension("xlsx.tmp");
    let mut z = Zip { out: BufWriter::new(File::create(&tmp)?), offset: 0, central: Vec::new(), count: 0 };
    z.add_bytes(
        "[Content_Types].xml",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/></Types>",
    )?;
    z.add_bytes(
        "_rels/.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>",
    )?;
    z.add_bytes(
        "xl/workbook.xml",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Datos\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>",
    )?;
    z.add_bytes(
        "xl/_rels/workbook.xml.rels",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/></Relationships>",
    )?;
    // Estilo 1 = cabecera en negrita.
    z.add_bytes(
        "xl/styles.xml",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font><font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"2\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/><xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>",
    )?;
    let sheet = head.as_bytes().chain(File::open(rows_xml)?).chain(tail.as_bytes());
    z.add("xl/worksheets/sheet1.xml", sheet, len, crc)?;
    z.finish()?;
    std::fs::rename(&tmp, path)
}

// ---------- CSV de entrada ----------

/// Lector de CSV en streaming: comillas, comillas dobles escapadas y saltos
/// dentro de comillas. Separador `,`, `;` o tabulador.
pub struct CsvReader<R: Read> {
    r: std::io::BufReader<R>,
    pub sep: u8,
    buf: Vec<u8>,
}

/// Separador más probable mirando la primera línea.
pub fn guess_sep(first_line: &str) -> u8 {
    [b',', b';', b'\t', b'|'].into_iter().max_by_key(|s| first_line.bytes().filter(|b| b == s).count()).unwrap_or(b',')
}

impl<R: Read> CsvReader<R> {
    pub fn new(r: R, sep: u8) -> CsvReader<R> {
        CsvReader { r: std::io::BufReader::with_capacity(1 << 16, r), sep, buf: Vec::new() }
    }

    /// Siguiente registro; `None` al final.
    pub fn record(&mut self) -> std::io::Result<Option<Vec<String>>> {
        use std::io::BufRead;
        let mut fields = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut quoted = false;
        let mut any = false;
        loop {
            self.buf.clear();
            let n = self.r.read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                if !any {
                    return Ok(None);
                }
                break;
            }
            any = true;
            let line = std::mem::take(&mut self.buf);
            let mut i = 0;
            // BOM al principio del archivo.
            if fields.is_empty() && cur.is_empty() && line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                i = 3;
            }
            while i < line.len() {
                let c = line[i];
                if quoted {
                    if c == b'"' {
                        if line.get(i + 1) == Some(&b'"') {
                            cur.push(b'"');
                            i += 1;
                        } else {
                            quoted = false;
                        }
                    } else {
                        cur.push(c);
                    }
                } else if c == b'"' && cur.is_empty() {
                    quoted = true;
                } else if c == self.sep {
                    fields.push(String::from_utf8_lossy(&cur).into_owned());
                    cur.clear();
                } else if c == b'\n' || (c == b'\r' && line.get(i + 1) == Some(&b'\n')) {
                    break;
                } else {
                    cur.push(c);
                }
                i += 1;
            }
            self.buf = line;
            if !quoted {
                break;
            }
        }
        fields.push(String::from_utf8_lossy(&cur).into_owned());
        Ok(Some(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(fmt: Format) -> (PathBuf, String) {
        let dir = crate::store::temp_dir(&format!("export-{}", fmt.ext()));
        let path = dir.join(format!("x.{}", fmt.ext()));
        let mut e = Exporter::create(&path, fmt, "ventas.t").unwrap();
        e.columns(&[("id".into(), "INTEGER".into()), ("nombre".into(), "VARCHAR".into()), ("nota".into(), "TEXT".into())]).unwrap();
        e.row(&[Some("1".into()), Some("Ana, \"la\" <grande>".into()), None]).unwrap();
        e.row(&[Some("2".into()), Some("O'Neil\nsegunda".into()), Some("ñ".into())]).unwrap();
        assert_eq!(e.finish().unwrap(), 2);
        let text = if fmt == Format::Xlsx { String::new() } else { std::fs::read_to_string(&path).unwrap() };
        (path, text)
    }

    #[test]
    fn csv_json_insert() {
        let (_, csv) = export(Format::Csv);
        assert_eq!(csv, "id,nombre,nota\r\n1,\"Ana, \"\"la\"\" <grande>\",\r\n2,\"O'Neil\nsegunda\",ñ\r\n");
        let (_, json) = export(Format::Json);
        assert_eq!(json, "[\n{\"id\":1,\"nombre\":\"Ana, \\\"la\\\" <grande>\",\"nota\":null},\n{\"id\":2,\"nombre\":\"O'Neil\\nsegunda\",\"nota\":\"ñ\"}\n]\n");
        let (_, sql) = export(Format::Insert);
        assert_eq!(sql.lines().next().unwrap(), "insert into ventas.t (id, nombre, nota) values (1, 'Ana, \"la\" <grande>', NULL);");
        assert!(sql.contains("'O''Neil\nsegunda'"));
    }

    #[test]
    fn xlsx_is_a_valid_zip() {
        let (path, _) = export(Format::Xlsx);
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[..4], b"PK\x03\x04");
        // Cada entrada: su CRC coincide con los datos (lectura del directorio central).
        let end = data.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let count = u16::from_le_bytes([data[end + 10], data[end + 11]]) as usize;
        let mut at = u32::from_le_bytes(data[end + 16..end + 20].try_into().unwrap()) as usize;
        let mut names = Vec::new();
        for _ in 0..count {
            let crc = u32::from_le_bytes(data[at + 16..at + 20].try_into().unwrap());
            let size = u32::from_le_bytes(data[at + 20..at + 24].try_into().unwrap()) as usize;
            let nlen = u16::from_le_bytes([data[at + 28], data[at + 29]]) as usize;
            let off = u32::from_le_bytes(data[at + 42..at + 46].try_into().unwrap()) as usize;
            let name = String::from_utf8(data[at + 46..at + 46 + nlen].to_vec()).unwrap();
            let lnlen = u16::from_le_bytes([data[off + 26], data[off + 27]]) as usize;
            let body = &data[off + 30 + lnlen..off + 30 + lnlen + size];
            assert_eq!(crc32_update(0, body), crc, "{name}");
            if name == "xl/worksheets/sheet1.xml" {
                let s = String::from_utf8(body.to_vec()).unwrap();
                assert!(s.contains("<c r=\"A2\"><v>1</v></c>") && s.contains("Ana, &quot;la&quot; &lt;grande&gt;"), "{s}");
            }
            names.push(name);
            at += 46 + nlen;
        }
        assert!(names.contains(&"[Content_Types].xml".to_owned()));
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(701), "ZZ");
    }

    #[test]
    fn csv_reader() {
        let data = "\u{feff}id;nombre;nota\r\n1;\"Ana; \"\"la\"\"\";\n2;\"dos\nlíneas\";ñ\n";
        assert_eq!(guess_sep("id;nombre;nota"), b';');
        let mut r = CsvReader::new(data.as_bytes(), b';');
        assert_eq!(r.record().unwrap().unwrap(), vec!["id", "nombre", "nota"]);
        assert_eq!(r.record().unwrap().unwrap(), vec!["1", "Ana; \"la\"", ""]);
        assert_eq!(r.record().unwrap().unwrap(), vec!["2", "dos\nlíneas", "ñ"]);
        assert_eq!(r.record().unwrap(), None);
    }
}
