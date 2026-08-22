//! Recopie d'une entrée dans le presse-papier système.

use anyhow::Result;

#[cfg(target_os = "linux")]
pub fn set(mime: &str, data: Vec<u8>) -> Result<()> {
    // Le daemon devient propriétaire de la sélection ; wl-clipboard-rs sert
    // les demandes dans un thread interne jusqu'à la prochaine copie.
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let opts = Options::new();
    opts.copy(
        Source::Bytes(data.into()),
        MimeType::Specific(mime.to_string()),
    )?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set(mime: &str, data: Vec<u8>) -> Result<()> {
    use anyhow::Context as _;
    let mut clipboard = arboard::Clipboard::new()?;
    if mime.starts_with("image/") {
        let img = image::load_from_memory(&data)
            .context("décodage de l'image à recopier")?
            .to_rgba8();
        let (w, h) = img.dimensions();
        clipboard.set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: img.into_raw().into(),
        })?;
    } else {
        clipboard.set_text(String::from_utf8_lossy(&data).into_owned())?;
    }
    Ok(())
}
