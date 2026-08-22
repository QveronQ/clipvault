//! Recopie d'une entrée dans le presse-papier système (le daemon devient
//! propriétaire de la sélection ; wl-clipboard-rs sert les demandes dans un
//! thread interne jusqu'à la prochaine copie).

use anyhow::Result;
use wl_clipboard_rs::copy::{MimeType, Options, Source};

pub fn set(mime: &str, data: Vec<u8>) -> Result<()> {
    let opts = Options::new();
    opts.copy(
        Source::Bytes(data.into()),
        MimeType::Specific(mime.to_string()),
    )?;
    Ok(())
}
