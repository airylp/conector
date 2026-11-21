//! TLS del servicio de red (rustls con ring).
//!
//! Certificado propio si se indica (`servir_cert=` + `servir_clave_tls=`, PEM);
//! si no, uno autofirmado que se crea la primera vez en la carpeta de
//! configuración (`servicio-cert.pem`, `servicio-clave.pem` con permisos 600),
//! válido para localhost, el nombre del equipo y su IP local. Los clientes lo
//! aceptan con `curl --cacert servicio-cert.pem` o fijando su huella SHA-256.

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Tls {
    pub config: Arc<ServerConfig>,
    /// SHA-256 del certificado (DER), en hexadecimal con «:».
    pub fingerprint: String,
    pub cert_path: PathBuf,
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Nombre de este equipo (sin dependencias).
fn hostname() -> Option<String> {
    let from_env = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok();
    let name = from_env
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))?;
    let name = name.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// IP local por la que sale el tráfico (UDP «connect» no envía nada).
fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

fn default_paths() -> Option<(PathBuf, PathBuf)> {
    let dir = crate::store::config_dir()?;
    Some((dir.join("servicio-cert.pem"), dir.join("servicio-clave.pem")))
}

/// Crea un certificado autofirmado y su clave en las rutas dadas.
pub fn create_self_signed(cert: &Path, key: &Path) -> Result<(), String> {
    let mut names = vec!["localhost".to_owned(), "127.0.0.1".to_owned(), "::1".to_owned()];
    if let Some(h) = hostname() {
        names.push(h.clone());
        if !h.contains('.') {
            names.push(format!("{h}.local"));
        }
    }
    if let Some(ip) = local_ip() {
        names.push(ip);
    }
    names.dedup();
    let mut params = rcgen::CertificateParams::new(names).map_err(|e| e.to_string())?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "conector");
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "conector (autofirmado)");
    let pair = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let c = params.self_signed(&pair).map_err(|e| e.to_string())?;
    if let Some(d) = cert.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    crate::store::write_atomic(key, pair.serialize_pem().as_bytes(), true).map_err(|e| e.to_string())?;
    crate::store::write_atomic(cert, c.pem().as_bytes(), false).map_err(|e| e.to_string())
}

/// Carga el certificado indicado o el autofirmado (creándolo si falta).
pub fn load(cert: &str, key: &str) -> Result<Tls, String> {
    let (cert_path, key_path) = if !cert.trim().is_empty() && !key.trim().is_empty() {
        (PathBuf::from(cert.trim()), PathBuf::from(key.trim()))
    } else {
        let (c, k) = default_paths().ok_or("sin carpeta de configuración")?;
        if !c.is_file() || !k.is_file() {
            create_self_signed(&c, &k)?;
        }
        (c, k)
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .map_err(|e| format!("{}: {e}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert_path.display()))?;
    let first = certs.first().ok_or_else(|| format!("{}: sin certificados", cert_path.display()))?;
    let fp = fingerprint(first.as_ref());
    let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("certificado o clave no válidos: {e}"))?;
    Ok(Tls { config: Arc::new(config), fingerprint: fp, cert_path })
}
