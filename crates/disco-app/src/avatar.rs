//! Custom agent pictures: `<app data>/avatars/<agent>.png|jpg`, handed to the UI as a
//! data URL (read once, then cached until the next change).
use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD, Engine};
use tauri::Manager;

use crate::Core;

/// Largest picture accepted, decoded.
const MAX_BYTES: usize = 1 << 20;
/// File extension, MIME type and leading bytes of each accepted format.
const KINDS: [(&str, &str, &[u8]); 2] = [("png", "image/png", b"\x89PNG"), ("jpg", "image/jpeg", b"\xFF\xD8\xFF")];

/// `data:image/png;base64,…` (or JPEG) → (extension, picture bytes).
pub fn parse(url: &str) -> Result<(&'static str, Vec<u8>), String> {
    let (ext, magic, b64) = KINDS.iter()
        .find_map(|&(ext, mime, magic)| Some((ext, magic, url.strip_prefix("data:")?.strip_prefix(mime)?.strip_prefix(";base64,")?)))
        .ok_or("头像只支持 PNG 或 JPEG 的 data URL")?;
    // Refuse oversize input before decoding it.
    if b64.len() > MAX_BYTES.div_ceil(3) * 4 { return Err("头像不能超过 1 MB".into()); }
    let bytes = STANDARD.decode(b64).map_err(|_| "头像不是有效的 base64")?;
    if bytes.len() > MAX_BYTES { return Err("头像不能超过 1 MB".into()); }
    if !bytes.starts_with(magic) { return Err("头像内容和格式不符".into()); }
    Ok((ext, bytes))
}

/// Both possible files of an agent's picture, in `KINDS` order.
fn paths(core: &Core, agent: &str) -> Result<Vec<PathBuf>, String> {
    let base = core.app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("avatars");
    let name = crate::mcp::space_name(agent)?;
    Ok(KINDS.iter().map(|(ext, ..)| base.join(format!("{name}.{ext}"))).collect())
}

fn read(core: &Core, agent: &str) -> Option<String> {
    let paths = paths(core, agent).ok()?;
    KINDS.iter().zip(paths).find_map(|(&(_, mime, _), p)| std::fs::read(p).ok().map(|b| format!("data:{mime};base64,{}", STANDARD.encode(b))))
}

fn forget(core: &Core, agent: &str) {
    core.avatars.lock().unwrap_or_else(|p| p.into_inner()).remove(agent);
}

/// The agent's picture as a data URL, if it has one.
pub fn get(core: &Core, agent: &str) -> Option<String> {
    let hit = core.avatars.lock().unwrap_or_else(|p| p.into_inner()).get(agent).cloned();
    if let Some(v) = hit { return v; }
    let v = read(core, agent);
    core.avatars.lock().unwrap_or_else(|p| p.into_inner()).insert(agent.to_owned(), v.clone());
    v
}

/// Save (`Some(data URL)`) or remove (`None`) the agent's picture.
pub fn set(core: &Core, agent: &str, data: Option<&str>) -> Result<(), String> {
    let paths = paths(core, agent)?;
    let new = data.map(parse).transpose()?;
    if let Some((ext, bytes)) = &new {
        let i = KINDS.iter().position(|k| k.0 == *ext).unwrap_or(0);
        if let Some(d) = paths[i].parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
        // Write then rename, so a reader never sees half a picture.
        let tmp = crate::mcp::tmp_path(&paths[i]);
        std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &paths[i])).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
    }
    for (p, (ext, ..)) in paths.iter().zip(KINDS) {
        if new.as_ref().is_none_or(|(e, _)| *e != ext) { let _ = std::fs::remove_file(p); }
    }
    forget(core, agent);
    Ok(())
}

/// A renamed agent keeps its picture.
pub fn rename(core: &Core, old: &str, new: &str) {
    if let (Ok(from), Ok(to)) = (paths(core, old), paths(core, new)) {
        for (f, t) in from.iter().zip(&to) {
            let _ = std::fs::remove_file(t);
            if f.exists() { let _ = std::fs::rename(f, t); }
        }
    }
    forget(core, old);
    forget(core, new);
}

#[cfg(test)]
mod tests {
    use super::{parse, MAX_BYTES, STANDARD};
    use base64::Engine;

    #[test]
    fn data_urls() {
        let png = format!("data:image/png;base64,{}", STANDARD.encode(b"\x89PNG\r\n\x1a\nrest"));
        assert_eq!(parse(&png).map(|(e, b)| (e, b.len())), Ok(("png", 12)));
        let jpg = format!("data:image/jpeg;base64,{}", STANDARD.encode(b"\xFF\xD8\xFF\xE0x"));
        assert_eq!(parse(&jpg).map(|(e, _)| e), Ok("jpg"));
        // Wrong type, wrong encoding, bad base64, content not matching the type.
        for bad in ["data:image/gif;base64,R0lG", "data:image/png,abc", "data:image/png;base64,***", "image/png;base64,iVBO",
            &format!("data:image/png;base64,{}", STANDARD.encode(b"\xFF\xD8\xFF")), "data:image/png;base64,"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let mut big = b"\x89PNG".to_vec();
        big.resize(MAX_BYTES + 1, 0);
        assert!(parse(&format!("data:image/png;base64,{}", STANDARD.encode(&big))).is_err());
        big.truncate(MAX_BYTES);
        assert!(parse(&format!("data:image/png;base64,{}", STANDARD.encode(&big))).is_ok());
    }
}
