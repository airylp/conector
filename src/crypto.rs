//! Contraseñas compartidas cifradas con una frase común del equipo.
//!
//! - `<catálogo>/cifrado.txt` guarda la sal y los parámetros de Argon2id y un
//!   verificador (el texto `conector` cifrado): sirve para saber si la frase
//!   es correcta sin probarla contra ninguna conexión. No contiene la clave.
//! - Clave = Argon2id(frase, sal) de 32 bytes; cada contraseña se cifra con
//!   XChaCha20-Poly1305 y nonce aleatorio de 24 bytes:
//!   `clave_cifrada=x1:<base64(nonce ‖ cifrado ‖ etiqueta)>` en el `.conexion`.
//! - Cada equipo recuerda la clave derivada (no la frase) en su `secretos.txt`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::path::Path;

pub const FILE: &str = "cifrado.txt";
const PREFIX: &str = "x1:";
const CHECK: &str = "conector";

pub type Key = [u8; 32];

#[derive(Clone, Debug, PartialEq)]
pub struct Lock {
    pub salt: [u8; 16],
    /// Memoria (KiB), iteraciones y paralelismo de Argon2id.
    pub m: u32,
    pub t: u32,
    pub p: u32,
    pub check: String,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("sin aleatoriedad del sistema");
    b
}

pub fn derive(pass: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Key, String> {
    let params = argon2::Params::new(m, t, p, Some(32)).map_err(|e| e.to_string())?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    a.hash_password_into(pass.as_bytes(), salt, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn encrypt(key: &Key, plain: &str) -> String {
    let cipher = XChaCha20Poly1305::new(&(*key).into());
    let nonce: [u8; 24] = random();
    let ct = cipher.encrypt(&XNonce::from(nonce), plain.as_bytes()).expect("cifrar");
    let mut buf = nonce.to_vec();
    buf.extend_from_slice(&ct);
    format!("{PREFIX}{}", B64.encode(buf))
}

/// `None` si la clave no es la correcta o el texto está dañado.
pub fn decrypt(key: &Key, text: &str) -> Option<String> {
    let raw = B64.decode(text.trim().strip_prefix(PREFIX)?).ok()?;
    if raw.len() < 24 + 16 {
        return None;
    }
    let (nonce, ct) = raw.split_at(24);
    let nonce: [u8; 24] = nonce.try_into().ok()?;
    let cipher = XChaCha20Poly1305::new(&(*key).into());
    String::from_utf8(cipher.decrypt(&XNonce::from(nonce), ct).ok()?).ok()
}

impl Lock {
    /// Nueva frase: sal nueva (parámetros recomendados por OWASP).
    pub fn create(pass: &str) -> Result<(Lock, Key), String> {
        if pass.chars().count() < 8 {
            return Err("La frase debe tener al menos 8 caracteres.".into());
        }
        let salt = random();
        let (m, t, p) = (19 * 1024, 2, 1);
        let key = derive(pass, &salt, m, t, p)?;
        Ok((Lock { salt, m, t, p, check: encrypt(&key, CHECK) }, key))
    }

    /// Clave si la frase es la correcta.
    pub fn unlock(&self, pass: &str) -> Option<Key> {
        let key = derive(pass, &self.salt, self.m, self.t, self.p).ok()?;
        self.verify(&key).then_some(key)
    }

    pub fn verify(&self, key: &Key) -> bool {
        decrypt(key, &self.check).as_deref() == Some(CHECK)
    }

    pub fn load(catalog: &Path) -> Option<Lock> {
        let text = std::fs::read_to_string(catalog.join(FILE)).ok()?;
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(str::trim);
        let salt = B64.decode(get("sal")?).ok()?.try_into().ok()?;
        Some(Lock {
            salt,
            m: get("m")?.parse().ok()?,
            t: get("t")?.parse().ok()?,
            p: get("p")?.parse().ok()?,
            check: get("verificador")?.to_owned(),
        })
    }

    pub fn save(&self, catalog: &Path) -> std::io::Result<()> {
        let text = format!(
            "# conector: parámetros de la frase común (no contiene la clave)\nkdf=argon2id\nsal={}\nm={}\nt={}\np={}\nverificador={}\n",
            B64.encode(self.salt),
            self.m,
            self.t,
            self.p,
            self.check
        );
        std::fs::create_dir_all(catalog)?;
        crate::store::write_atomic(&catalog.join(FILE), text.as_bytes(), false)
    }
}

pub fn key_to_hex(k: &Key) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn key_from_hex(s: &str) -> Option<Key> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_roundtrip() {
        let dir = crate::store::temp_dir("cripto");
        assert!(Lock::create("corta").is_err());
        let (lock, key) = Lock::create("frase común del equipo").unwrap();
        lock.save(&dir).unwrap();
        let l2 = Lock::load(&dir).unwrap();
        assert_eq!(l2, lock);
        assert_eq!(l2.unlock("frase común del equipo"), Some(key));
        assert_eq!(l2.unlock("otra frase cualquiera"), None);
        let c = encrypt(&key, "s3cr3t·ñ");
        assert!(c.starts_with("x1:"));
        assert_ne!(encrypt(&key, "s3cr3t·ñ"), c, "nonce aleatorio");
        assert_eq!(decrypt(&key, &c).as_deref(), Some("s3cr3t·ñ"));
        let mut other = key;
        other[0] ^= 1;
        assert_eq!(decrypt(&other, &c), None);
        assert_eq!(key_from_hex(&key_to_hex(&key)), Some(key));
        let _ = std::fs::remove_dir_all(dir);
    }
}
