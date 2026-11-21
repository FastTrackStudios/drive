//! Signing a PDF with a digital ID, natively only: the private key never
//! leaves the computer that has it (and never reaches a browser).
//!
//! The seam: the browser asks the node to have a PDF signed
//! (`POST /pdf/sign/{hash}` with [`SignRequest`] minus the key); the node
//! hands the request to the signer's own agent over the connection it
//! already keeps (as `/fetch` asks an agent for a file), and that agent,
//! holding the PKCS #12 file and its password in its own config, calls
//! [`sign`] and stores the signed file as a new blob. Not wired yet: see
//! docs/viewers.md.

use pdfcraft_cos::Document;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What a person asks for when they sign (no key in it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SignRequest {
    /// Sign this existing, empty signature field…
    #[serde(default)]
    pub field: Option<String>,
    /// …or put a new one on this page (0-based), here (PDF user space;
    /// `None`: an invisible signature).
    #[serde(default)]
    pub page: usize,
    #[serde(default)]
    pub rect: Option<[f64; 4]>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
}

/// Sign `pdf` with the digital ID in `p12` (a PKCS #12 file and its
/// password), dated `unix_secs`: the signed file's bytes.
pub fn sign(
    pdf: Vec<u8>,
    p12: &[u8],
    password: &str,
    req: &SignRequest,
    unix_secs: i64,
) -> Result<Vec<u8>, String> {
    let doc = Document::open(Arc::new(pdf)).map_err(|e| e.to_string())?;
    let id = pdfcraft_sign::pkcs12::open(p12, password).map_err(|e| e.to_string())?;
    let opts = pdfcraft_sign::SignOptions {
        field: req.field.clone(),
        page: req.page,
        rect: req.rect,
        reason: req.reason.clone(),
        location: req.location.clone(),
        date: pdfcraft_cos::pdf_date(unix_secs),
        ..Default::default()
    };
    pdfcraft_sign::sign(&doc, &id, &opts).map_err(|e| e.to_string())
}

#[cfg(all(test, feature = "pdf"))]
mod tests {
    use super::*;

    /// Opt-in (no key belongs in the repo): `DRIVE_VIEW_TEST_P12` names a
    /// throwaway PKCS #12 file with the password `test`, e.g.
    /// `openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=Test -keyout k.pem -out c.pem -days 2`
    /// then `openssl pkcs12 -export -inkey k.pem -in c.pem -out t.p12 -passout pass:test`.
    #[test]
    fn signs_with_a_digital_id() {
        let Ok(path) = std::env::var("DRIVE_VIEW_TEST_P12") else {
            return;
        };
        let p12 = std::fs::read(path).expect("the test ID reads");
        let req = SignRequest {
            reason: Some("Approved".into()),
            ..Default::default()
        };
        let signed = sign(
            crate::pdf::tests::hello(),
            &p12,
            "test",
            &req,
            1_795_000_000,
        )
        .expect("signs");
        let doc = Document::open(Arc::new(signed.clone())).expect("still a PDF");
        assert!(
            pdfcraft_sign::has_signatures(&doc),
            "the signed file has a signature"
        );
        assert!(crate::pdf::PdfDoc::open(signed).is_ok());
        assert!(sign(crate::pdf::tests::hello(), &p12, "wrong", &req, 0).is_err());
    }
}
